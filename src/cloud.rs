// Cloud sessions: Claude Code sessions in Anthropic's cloud, on the Claude
// logins (this machine's, and a vault's when a plugin brings one). A plugin
// can put another service behind the same engine (`oak_sdk::CloudBackend`:
// its own accounts and the way to them), and give a service's sessions a
// server of its own (`oak_sdk::SessionKit`: a command line tool in the
// session, files to and from the user, conversations carried from one
// account to the next). OAK lists the sessions (Home), opens one as a tab,
// and talks to it through Anthropic's session API, as Claude Code's web
// client does. Anthropic is asked only while the user looks: the list is
// fetched while Home shows it, a session's stream is open only while its tab
// is shown, and its files are listed only then. Nothing polls in the
// background.

use crate::tools::Head;
use crate::plugins::{CloudBackend, SessionKit, Tags};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::api;
use crate::store::Conversation;

mod cloud_carry;
use cloud_carry::{carry_block, carry_text, history};

pub use oak_sdk::cloud::{File, is_key, key, parse_key};
/// An answer from the sessions API or a kit's server.
type Response = crate::plugins::Response;

const BETA: &str = "ccr-byoc-2025-07-29";
const CLOUD_KIND: &str = "anthropic_cloud";
const MAX_TITLE: usize = 60;
/// A tab seen within this long counts as still showing: its stream stays open.
const WATCH_GRACE: Duration = Duration::from_secs(3);

/// The service a session runs through: Oak's own (the Claude logins), or a
/// plugin's backend, by its place among those registered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Provider(Option<usize>);

impl Provider {
    /// Oak's own: sessions on the Claude logins.
    pub const OWN: Provider = Provider(None);

    fn backends() -> &'static [Arc<CloudBackend>] {
        &crate::plugins::parts().cloud_backends
    }

    /// Every provider: Oak's own, then the plugins'.
    pub fn all() -> Vec<Provider> {
        std::iter::once(Provider::OWN).chain((0..Self::backends().len()).map(|i| Provider(Some(i)))).collect()
    }

    fn backend(self) -> Option<&'static Arc<CloudBackend>> {
        self.0.and_then(|i| Self::backends().get(i))
    }

    /// The provider with this id ("": Oak's own, also "cloud").
    pub fn from_id(id: &str) -> Option<Provider> {
        if id.is_empty() || id == "cloud" {
            return Some(Provider::OWN);
        }
        Self::backends().iter().position(|b| b.id() == id).map(|i| Provider(Some(i)))
    }

    /// Its id ("" for Oak's own).
    pub fn id(self) -> String {
        self.backend().map(|b| b.id()).unwrap_or_default()
    }

    /// Whose account `rid` is: a plugin backend's start with its id and "~".
    pub fn from_rid(rid: &str) -> Self {
        match rid.split_once('~') {
            Some((id, _)) if !id.is_empty() => Self::backends().iter().position(|b| b.id() == id).map_or(Provider(Some(usize::MAX)), |i| Provider(Some(i))),
            _ => Provider::OWN,
        }
    }

    pub fn is_own(self) -> bool {
        self.0.is_none()
    }

    /// As in "Cloud session on a@b.c".
    pub fn name(self) -> String {
        self.backend().map_or_else(|| "Cloud".to_string(), |b| b.name())
    }

    /// Its icon, and whether that is a character of the text font (not a
    /// Nerd Font symbol).
    pub fn icon(self) -> (String, bool) {
        self.backend().map_or_else(|| (crate::cloud_view::CLOUD_ICON.to_string(), false), |b| (b.icon(), b.icon_is_text()))
    }

    pub fn accounts(self) -> Vec<api::CloudAccount> {
        match self.0 {
            None => api::cloud_accounts(),
            Some(_) => self.backend().map(|b| b.accounts()).unwrap_or_default(),
        }
    }

    /// Why no session can start through it now; None when one can.
    pub fn unavailable(self) -> Option<String> {
        match self.0 {
            None => api::cloud_accounts().is_empty().then(|| NO_ACCOUNTS.to_string()),
            Some(_) => match self.backend() {
                Some(b) => b.unavailable(),
                None => Some(GONE.to_string()),
            },
        }
    }

    /// What its sessions are given, if a plugin brought that.
    fn kit(self) -> Option<&'static Arc<SessionKit>> {
        let id = self.id();
        if self.0.is_some() && self.backend().is_none() {
            return None;
        }
        crate::plugins::parts().session_kits.iter().rev().find(|(of, k)| *of == id && k.ready()).map(|(_, k)| k)
    }

    /// What its kit's server is called in sentences ("the team server").
    pub fn server_name(self) -> Option<String> {
        self.kit().map(|k| k.server_name())
    }

    /// What the New screen says under it.
    pub fn about(self) -> String {
        match self.backend() {
            Some(b) => b.about(),
            None => "A Claude Code session in Anthropic's cloud, on one of your Claude accounts: it carries on while this computer is off.".to_string(),
        }
    }

    /// What hovering its choice on the New screen says.
    pub fn tip(self) -> String {
        match self.backend() {
            Some(b) => b.tip(),
            None => "Runs in Anthropic's cloud, on one of your Claude accounts".to_string(),
        }
    }

    /// Its sessions have a server: files, and moving to another account.
    pub fn has_kit(self) -> bool {
        self.kit().is_some()
    }

    /// A request to its kit's server.
    fn raw(self, method: reqwest::Method, path: &str, body: Option<(Vec<u8>, &str)>, timeout: Duration) -> Result<Response, String> {
        let kit = self.kit().ok_or(NO_KIT)?;
        kit.raw(method.as_str(), path, body.map(|(b, kind)| (b, kind.to_string())), timeout)
    }

    fn json(self, method: reqwest::Method, path: &str) -> Result<Value, String> {
        self.raw(method, path, None, SHORT)?.json()
    }

    /// The tags its sessions' first messages are wrapped in.
    fn tags(self) -> Tags {
        self.kit().map_or_else(|| Tags::of("oak"), |k| k.tags())
    }
}

/// Why a session's provider can't be used: its plugin isn't loaded.
const GONE: &str = "This session's service isn't available: the plugin that connects to it isn't loaded.";
const NO_ACCOUNTS: &str = "No Claude sign-in: cloud sessions run on your Claude accounts. Add one with + Add account on Home.";
const NO_KIT: &str = "These sessions have no file server.";

pub fn provider(key: &str) -> Provider {
    parse_key(key).map(|(rid, _)| Provider::from_rid(&rid)).unwrap_or_default()
}

/// Whether the session's account can be reached now. A plugin backend's
/// can't while it is disconnected (or connected to another server).
pub fn accessible(key: &str) -> bool {
    let Some((rid, _)) = parse_key(key) else { return false };
    let provider = Provider::from_rid(&rid);
    provider.is_own() || provider.accounts().iter().any(|a| a.rid == rid)
}

/// What to say about a session that isn't `accessible`.
fn inaccessible(key: &str) -> String {
    let p = provider(key);
    p.unavailable().unwrap_or_else(|| format!("This session's account isn't among {}'s now.", p.name()))
}

fn note_limit(rid: &str, until: i64) {
    match Provider::from_rid(rid).backend() {
        None => api::note_cloud_limit(rid, until),
        Some(b) => b.note_limit(rid, until),
    }
}

fn request(rid: &str, method: reqwest::Method, path: &str, beta: Option<&str>, org: Option<&str>, body: Option<&Value>, timeout: Duration) -> Result<Response, String> {
    let provider = Provider::from_rid(rid);
    if provider.is_own() {
        return api::cloud_request(rid, method, path, beta, org, body, timeout).map(crate::plugins::response);
    }
    let backend = provider.backend().ok_or(GONE)?;
    backend.request(rid, oak_sdk::cloud::Call { method: method.as_str().to_string(), path: path.to_string(), beta: beta.map(String::from), org: org.map(String::from), body: body.cloned(), timeout })
}

/// The events API names a session `cse_...`; the list names it `session_...`.
fn cse(id: &str) -> String {
    match id.strip_prefix("session_") {
        Some(rest) => format!("cse_{rest}"),
        None => id.to_string(),
    }
}

fn session_id(id: &str) -> String {
    match id.strip_prefix("cse_") {
        Some(rest) => format!("session_{rest}"),
        None => id.to_string(),
    }
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("session_").is_some_and(|r| !r.is_empty() && r.len() <= 120 && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// Milliseconds since 1970 from an ISO time, or seconds or milliseconds.
fn instant(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            if x < 1e12 { (x * 1000.0) as i64 } else { x as i64 }
        }
        Value::String(s) => crate::time::parse_time(s).unwrap_or(0),
        _ => 0,
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = crate::tools::one_line(s);
    if flat.chars().count() <= max { flat } else { format!("{}…", flat.head(max - 1)) }
}

fn title_for(text: &str) -> String {
    one_line(text.lines().next().unwrap_or(""), MAX_TITLE)
}

// State
/// A session in the list.
#[derive(Clone, Debug)]
pub struct Listed {
    pub key: String,
    pub account: String,
    pub title: String,
    /// "running", "requires_action" or "idle".
    pub status: String,
    pub updated: i64,
}

/// An open session: its conversation as OAK's transcript shows one, built
/// from the session's events.
pub struct Chat {
    pub rid: String,
    pub id: String,
    pub title: String,
    /// The events as OAK messages (`messages` alone is filled in).
    pub conv: Conversation,
    pub live: crate::agent::Live,
    /// The last event seen.
    sequence: u64,
    pub working: bool,
    /// An environment line from while it starts ("Installing…").
    pub status: String,
    /// The agent asks to use a tool: (request id, tool, its input).
    pub pending: Option<(String, String, Value)>,
    /// Its account is out of usage until then (ms since 1970), or 0.
    pub limited_until: i64,
    /// The stream is open.
    pub connected: bool,
    /// Why it isn't, or what went wrong last.
    pub trouble: String,
    /// The last thing that went wrong with a turn, for the transcript's end.
    pub notes: Vec<(String, bool)>,
    /// Message uuids already shown (sent from here, or seen).
    uuids: HashSet<String>,
    /// Being moved to another account.
    pub moving: bool,
    /// A message on its way.
    pub sending: bool,
    /// The tab was shown then; the stream stays open while it is recent.
    seen: Instant,
    streaming: bool,
    /// Ended for good (the session is gone).
    gone: bool,
}

impl Chat {
    fn new(rid: &str, id: &str, title: &str) -> Chat {
        Chat {
            rid: rid.into(), id: id.into(), title: title.into(), conv: Conversation::default(),
            live: Default::default(), sequence: 0, working: false, status: String::new(),
            pending: None, limited_until: 0, connected: false, trouble: String::new(), notes: vec![],
            uuids: HashSet::new(), moving: false, sending: false,
            seen: Instant::now(), streaming: false, gone: false,
        }
    }
}

#[derive(Default)]
struct RemoteState {
    generation: u64,
    list: Vec<Listed>,
    listed_at: Option<Instant>,
    listing: bool,
    list_error: Option<String>,
    files: Vec<File>,
    files_at: Option<Instant>,
    files_loading: bool,
    files_error: Option<String>,
}
#[derive(Default)]
struct State {
    /// The list and the files, by provider.
    remotes: HashMap<Provider, RemoteState>,
    chats: HashMap<String, Arc<Mutex<Chat>>>,
    orgs: HashMap<String, String>,
    envs: HashMap<String, String>,
    starting: bool,
    started: Option<Result<String, String>>,
}
impl State {
    fn remote(&self, provider: Provider) -> &RemoteState {
        static NONE: LazyLock<RemoteState> = LazyLock::new(RemoteState::default);
        self.remotes.get(&provider).unwrap_or(&NONE)
    }
    fn remote_mut(&mut self, provider: Provider) -> &mut RemoteState {
        self.remotes.entry(provider).or_default()
    }
}
/// Marks a fetch under way (the flag `of` gives, beside when the last one
/// came) unless one is or the last is younger than `fresh`: the state's
/// generation, for `update_remote`.
fn start_fetch(provider: Provider, fresh: Duration, of: impl FnOnce(&mut RemoteState) -> (&mut bool, Option<Instant>)) -> Option<u64> {
    let mut state = STATE.lock().unwrap();
    let s = state.remote_mut(provider);
    let generation = s.generation;
    let (busy, at) = of(s);
    if *busy || at.is_some_and(|t| t.elapsed() < fresh) {
        return None;
    }
    *busy = true;
    Some(generation)
}
/// A background job's result reaches `provider`'s state through `f`, then
/// the window repaints; not when the state was reset since `generation`.
fn update_remote(provider: Provider, generation: Option<u64>, ctx: &eframe::egui::Context, f: impl FnOnce(&mut RemoteState)) {
    let mut state = STATE.lock().unwrap();
    let s = state.remote_mut(provider);
    if generation.is_some_and(|g| g != s.generation) { return; }
    f(s);
    drop(state);
    ctx.request_repaint();
}
/// Forgets what is held of backend `id`'s sessions (it disconnected, or
/// connects to another server); the others' stay.
pub fn forget(id: &str) {
    let Some(gone) = Provider::from_id(id).filter(|p| !p.is_own()) else { return };
    let mut s = STATE.lock().unwrap();
    let generation = s.remote(gone).generation + 1;
    s.remotes.insert(gone, RemoteState { generation, ..Default::default() });
    s.chats.retain(|key, _| provider(key) != gone);
    s.orgs.retain(|rid, _| Provider::from_rid(rid) != gone);
    s.envs.retain(|rid, _| Provider::from_rid(rid) != gone);
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

// The API
const SHORT: Duration = Duration::from_secs(30);
/// A stream is closed after this and reopened if still wanted, so a stream for a tab no longer shown ends within it.
const STREAM_SPAN: Duration = Duration::from_secs(25);

fn problem(rid: &str, resp: Response) -> String {
    let status = resp.status;
    // A plugin backend's answers aren't quoted (its server could echo a
    // token into a log or a transcript): it says what the status means.
    if let Some(b) = Provider::from_rid(rid).backend() {
        return b.problem(status).unwrap_or_else(|| format!("{}: HTTP {status}", b.name()));
    }
    let text = resp.text().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let msg = v["error"]["message"].as_str().or(v["error"].as_str()).map(String::from).unwrap_or_else(|| text.head(200));
    match status {
        429 => "Too many requests; try again shortly.".into(),
        _ => format!("HTTP {status}: {msg}"),
    }
}

/// A JSON call on account `rid`; `scoped` adds its organization.
fn call(rid: &str, method: reqwest::Method, path: &str, beta: bool, scoped: bool, body: Option<&Value>) -> Result<Value, String> {
    let org = if scoped { Some(organization(rid)?) } else { None };
    let resp = request(rid, method, path, beta.then_some(BETA), org.as_deref(), body, SHORT)?;
    if !resp.ok() {
        return Err(problem(rid, resp));
    }
    let text = resp.text()?;
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Every sessions call is scoped to the login's organization.
fn organization(rid: &str) -> Result<String, String> {
    if let Some(o) = STATE.lock().unwrap().orgs.get(rid) {
        return Ok(o.clone());
    }
    let p = call(rid, reqwest::Method::GET, "/api/oauth/profile", false, false, None)?;
    let org = p["organization"]["uuid"].as_str().ok_or("The account's profile names no organization.")?.to_string();
    STATE.lock().unwrap().orgs.insert(rid.into(), org.clone());
    Ok(org)
}

/// The account's OAK environment, made if there is none.
fn environment(rid: &str) -> Result<String, String> {
    if let Some(e) = STATE.lock().unwrap().envs.get(rid) {
        return Ok(e.clone());
    }
    // With a kit, the environment installs its tool and knows its server;
    // without, it is a bare one.
    let kit = Provider::from_rid(rid).kit();
    let (label, script, variables, seed) = match kit {
        Some(kit) => {
            let (server, token) = kit.server()?;
            let script = kit.setup_script(&server);
            (kit.environment_name(), script.clone(), kit.variables(&server, &token), format!("{script}\n{server}\n{token}"))
        }
        None => ("OAK".to_string(), String::new(), json!({}), "bare".to_string()),
    };
    // A hash of the script, server and token, so a change to any makes a new name.
    use sha2::Digest;
    let sum = sha2::Sha256::digest(seed.as_bytes());
    let name = format!("{label} {}", api::hex(&sum[..4]));
    // The list comes in pages, read as Claude Code reads them (it grows
    // when something makes an environment per job).
    let mut found = None;
    let mut after = String::new();
    for _ in 0..50 {
        let path = if after.is_empty() {
            "/v1/environment_providers".to_string()
        } else {
            format!("/v1/environment_providers?after_id={after}")
        };
        let listed = call(rid, reqwest::Method::GET, &path, false, true, None)?;
        let page = listed["environments"].as_array().cloned().unwrap_or_default();
        found = page
            .iter()
            .find(|e| e["kind"] == CLOUD_KIND && e["name"] == name.as_str())
            .and_then(|e| e["environment_id"].as_str().map(String::from));
        let last = listed["last_id"].as_str().unwrap_or_default().to_string();
        let plain = !last.is_empty() && last.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if found.is_some() || listed["has_more"] != true || page.is_empty() || !plain || last == after {
            break;
        }
        after = last;
    }
    let id = match found {
        Some(id) => id,
        None => {
            let made = call(rid, reqwest::Method::POST, "/v1/environment_providers/cloud/create", true, true, Some(&json!({
                "name": name, "kind": CLOUD_KIND,
                "description": format!("{label} - full network access"),
                "config": {"environment_type": "anthropic", "cwd": "/home/user", "init_script": script, "environment": variables,
                           "languages": [{"name": "python", "version": "3.11"}, {"name": "node", "version": "20"}],
                           "network_config": {"allowed_hosts": ["*"], "allow_default_hosts": true}},
            })))?;
            made["environment_id"].as_str().ok_or("The cloud made no environment.")?.to_string()
        }
    };
    STATE.lock().unwrap().envs.insert(rid.into(), id.clone());
    Ok(id)
}

fn control_request(request: Value) -> Value {
    json!({"type": "control_request", "request_id": api::new_uuid(), "request": request, "uuid": api::new_uuid()})
}

fn mode_request() -> Value {
    control_request(json!({"subtype": "set_permission_mode", "mode": "auto"}))
}

/// Summaries of the model's thinking, as OAK shows its own; the budget stays as it is.
fn thinking_request() -> Value {
    control_request(json!({"subtype": "set_max_thinking_tokens", "thinking_display": "summarized"}))
}

/// A session's Messages block as the transcript shows it, in the Oak format.
fn oak_block(b: &Value) -> Value {
    crate::oak::from_anthropic_block(b, "claude")
}

/// A user message: the images (OAK's image blocks, as Messages ones), then the text.
fn user_event(id: &str, text: &str, images: &[Value], session: &str) -> Value {
    let content = if images.is_empty() {
        Value::String(text.to_string())
    } else {
        let mut c: Vec<Value> = images.iter().map(|b| json!({"type": "image", "source": {"type": "base64", "media_type": b["media_type"], "data": b["data"]}})).collect();
        if !text.trim().is_empty() {
            c.push(json!({"type": "text", "text": text}));
        }
        Value::Array(c)
    };
    json!({"uuid": id, "session_id": session, "type": "user", "parent_tool_use_id": null, "message": {"role": "user", "content": content}})
}

fn post_events(rid: &str, id: &str, payloads: Vec<Value>) -> Result<(), String> {
    let events: Vec<Value> = payloads.into_iter().map(|p| json!({"payload": p})).collect();
    call(rid, reqwest::Method::POST, &format!("/v1/code/sessions/{}/events", cse(id)), false, true, Some(&json!({"events": events}))).map(|_| ())
}

// The list
/// How long a list is good for while Home shows it.
const LIST_FRESH: Duration = Duration::from_secs(20);

/// Home shows the list: fetches it when it is stale, in the background.
pub fn want_list(provider: Provider, ctx: &eframe::egui::Context) {
    let Some(generation) = start_fetch(provider, LIST_FRESH, |s| (&mut s.listing, s.listed_at)) else { return };
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let (mut list, error) = fetch_list(provider);
        update_remote(provider, Some(generation), &ctx, |s| {
            // A session just started here may not be listed yet.
            for old in &s.list {
                if !list.iter().any(|x| x.key == old.key) && error.as_ref().is_some_and(|e| e.contains(&old.account)) {
                    list.push(old.clone());
                }
            }
            list.sort_by_key(|x| std::cmp::Reverse(x.updated));
            s.list = list;
            s.list_error = error;
            s.listing = false;
            s.listed_at = Some(Instant::now());
        });
    });
}

/// Fetches the list again when it is next wanted.
pub fn stale_list() {
    for r in STATE.lock().unwrap().remotes.values_mut() {
        r.listed_at = None;
    }
}

fn fetch_list(provider: Provider) -> (Vec<Listed>, Option<String>) {
    let accounts = provider.accounts();
    if accounts.is_empty() {
        return (vec![], provider.unavailable().or(Some(NO_ACCOUNTS.into())));
    }
    let results: Vec<_> = std::thread::scope(|sc| {
        let handles: Vec<_> = accounts
            .iter()
            .map(|a| sc.spawn(move || call(&a.rid, reqwest::Method::GET, "/v1/code/sessions", false, false, None)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(Err("failed".into()))).collect()
    });
    let mut list = vec![];
    let mut failed = vec![];
    for (a, r) in accounts.iter().zip(results) {
        let v = match r {
            Ok(v) => v,
            Err(_) => {
                failed.push(a.label.clone());
                continue;
            }
        };
        for x in v["data"].as_array().into_iter().flatten() {
            if x["environment_kind"].as_str().is_some_and(|k| k != CLOUD_KIND) || x["status"] == "archived" {
                continue;
            }
            let id = session_id(x["id"].as_str().unwrap_or(""));
            if !valid_id(&id) {
                continue;
            }
            let updated = [&x["last_event_at"], &x["updated_at"], &x["created_at"]].into_iter().map(instant).find(|&t| t > 0).unwrap_or(0);
            list.push(Listed {
                key: key(&a.rid, &id),
                account: a.label.clone(),
                title: x["title"].as_str().unwrap_or("").trim().to_string(),
                status: x["worker_status"].as_str().unwrap_or("idle").to_string(),
                updated,
            });
        }
    }
    let error = (!failed.is_empty()).then(|| format!("Couldn't list the sessions of {}.", failed.join(", ")));
    (list, error)
}

/// The list as last fetched, newest first, with what open sessions are doing
/// now; whether it is loading, and what went wrong.
pub fn list(provider: Provider) -> (Vec<Listed>, bool, Option<String>, bool) {
    let state = STATE.lock().unwrap();
    let s = state.remote(provider);
    let mut list = s.list.clone();
    for x in &mut list {
        if let Some(c) = state.chats.get(&x.key) {
            let c = c.lock().unwrap();
            if c.connected {
                x.status = if c.pending.is_some() { "requires_action" } else if c.working { "running" } else { "idle" }.into();
            }
        }
    }
    (list, s.listing, s.list_error.clone(), s.listed_at.is_some())
}

/// What a tab shows: the session's title and whether it works.
pub fn tab_info(key: &str) -> (String, bool) {
    let s = STATE.lock().unwrap();
    if let Some(c) = s.chats.get(key) {
        let c = c.lock().unwrap();
        let title = if c.title.is_empty() { "Untitled".to_string() } else { c.title.clone() };
        return (title, c.working);
    }
    let listed = s.remote(provider(key)).list.iter().find(|x| x.key == key);
    (listed.map(|x| x.title.clone()).filter(|t| !t.is_empty()).unwrap_or("Cloud session".into()), listed.is_some_and(|x| x.status == "running"))
}

// A session's stream
/// The session behind tab `key`, made (empty) if need be.
pub fn chat(key: &str) -> Option<Arc<Mutex<Chat>>> {
    let (rid, id) = parse_key(key)?;
    let mut s = STATE.lock().unwrap();
    let title = s.remote(provider(key)).list.iter().find(|x| x.key == key).map(|x| x.title.clone()).unwrap_or_default();
    Some(s.chats.entry(key.into()).or_insert_with(|| Arc::new(Mutex::new(Chat::new(&rid, &id, &title)))).clone())
}

/// Names the session behind tab `key` before it is opened, for one the
/// list doesn't show (one a plugin started, archived when it ended).
pub fn name(key: &str, title: &str) {
    if let Some(c) = chat(key) {
        let mut c = c.lock().unwrap();
        if c.title.is_empty() {
            c.title = title.to_string();
        }
    }
}

/// The tab is shown: keeps its stream open (opening it if need be), and its
/// files listed.
pub fn watch(key: &str, ctx: &eframe::egui::Context) {
    let Some(chat) = chat(key) else { return };
    if !accessible(key) {
        let mut c = chat.lock().unwrap();
        c.connected = false;
        c.trouble = inaccessible(key);
        return;
    }
    want_files(provider(key), ctx);
    let mut c = chat.lock().unwrap();
    c.seen = Instant::now();
    if c.streaming || c.gone {
        return;
    }
    c.streaming = true;
    drop(c);
    let ctx = ctx.clone();
    std::thread::spawn(move || stream(chat, ctx));
}

fn stream(chat: Arc<Mutex<Chat>>, ctx: eframe::egui::Context) {
    let mut delay = Duration::from_secs(1);
    loop {
        let (rid, id, after) = {
            let mut c = chat.lock().unwrap();
            if c.seen.elapsed() > WATCH_GRACE || c.gone {
                c.streaming = false;
                c.connected = false;
                return;
            }
            (c.rid.clone(), c.id.clone(), c.sequence)
        };
        let query = if after > 0 { format!("?from_sequence_num={after}") } else { String::new() };
        let path = format!("/v1/code/sessions/{}/events/stream{query}", cse(&id));
        let result = organization(&rid).and_then(|org| request(&rid, reqwest::Method::GET, &path, None, Some(&org), None, STREAM_SPAN));
        match result {
            Ok(resp) if resp.ok() => {
                {
                    let mut c = chat.lock().unwrap();
                    c.connected = true;
                    c.trouble.clear();
                }
                ctx.request_repaint();
                delay = Duration::from_secs(1);
                read_events(resp, &chat, &ctx);
            }
            Ok(resp) => {
                let status = resp.status;
                let why = problem(&rid, resp);
                let mut c = chat.lock().unwrap();
                c.connected = false;
                if status == 404 {
                    c.trouble = "This session no longer exists.".into();
                    c.gone = true;
                    c.streaming = false;
                    ctx.request_repaint();
                    return;
                }
                c.trouble = if status == 403 { why } else { "Reconnecting…".into() };
            }
            Err(e) => {
                let mut c = chat.lock().unwrap();
                c.connected = false;
                c.trouble = if api::vault_failed(&e) { e } else { "Reconnecting…".into() };
            }
        }
        ctx.request_repaint();
        // A stream that ran its span reconnects at once; a failure waits.
        if !chat.lock().unwrap().connected {
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(15));
        }
    }
}

/// Reads server-sent events until the stream ends, applying each.
fn read_events(resp: Response, chat: &Arc<Mutex<Chat>>, ctx: &eframe::egui::Context) {
    let mut reader = BufReader::new(resp.body);
    let (mut event, mut id, mut data) = (String::new(), String::new(), Vec::<String>::new());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            if !data.is_empty() || !event.is_empty() {
                let changed = chat.lock().unwrap().apply(&event, &id, &data.join("\n"));
                if changed {
                    ctx.request_repaint();
                }
            }
            (event, id) = (String::new(), String::new());
            data.clear();
            // Not shown any more: let go of the stream.
            if chat.lock().unwrap().seen.elapsed() > WATCH_GRACE {
                return;
            }
            continue;
        }
        if l.starts_with(':') {
            continue;
        }
        let (field, v) = l.split_once(':').map(|(f, v)| (f, v.strip_prefix(' ').unwrap_or(v))).unwrap_or((l, ""));
        match field {
            "event" => event = v.into(),
            "data" => data.push(v.into()),
            "id" => id = v.into(),
            _ => {}
        }
    }
}

/// The text of a message's content.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n\n"),
        _ => String::new(),
    }
}

/// The text without a leading block between `open` and `close`.
fn strip_block(text: &str, open: &str, close: &str) -> Option<String> {
    let t = text.trim();
    if !t.starts_with(open) {
        return None;
    }
    let end = t.find(close)?;
    Some(t[end + close.len()..].trim().to_string())
}

fn split_files_for(provider: Provider, text: &str) -> (String, Vec<String>) {
    let (files_open, files_close) = provider.tags().files;
    let (files_open, files_close) = (files_open.as_str(), files_close.as_str());
    let t = text.trim();
    match t.rfind(files_open) {
        Some(open) if t.ends_with(files_close) => {
            let names = t[open + files_open.len()..t.len() - files_close.len()]
                .lines()
                .filter_map(|l| l.strip_prefix("- ").filter(|n| !n.is_empty()).map(String::from))
                .collect();
            (t[..open].trim().to_string(), names)
        }
        _ => (text.to_string(), vec![]),
    }
}

/// What the user typed, then a block naming the files they sent.
fn wrap_files(provider: Provider, typed: &str, names: &[String]) -> String {
    let text = typed.trim();
    // (Names come only from a kit's server: without one nothing is uploaded.)
    let Some(kit) = provider.kit().filter(|_| !names.is_empty()) else { return text.to_string() };
    let (files_open, files_close) = kit.tags().files;
    let (cli, server) = (kit.cli(), kit.server_name());
    let one = names.len() == 1;
    format!(
        "{}{files_open}\n{}. {} on {server}; fetch {} with `{cli} pull <name>`:\n{}{files_close}",
        if text.is_empty() { String::new() } else { format!("{text}\n\n") },
        if one { "The user sent you this file" } else { "The user sent you these files" },
        if one { "It is" } else { "They are" },
        if one { "it" } else { "each one" },
        names.iter().map(|n| format!("- {n}\n")).collect::<String>()
    )
}

const CARRY_NOTE: &str = "Continued from an earlier session whose account reached its usage limit.";

impl Chat {
    fn changed(&mut self) -> bool {
        self.live.busy = self.working;
        self.live.status = if !self.status.is_empty() { self.status.clone() } else { "Working…".into() };
        true
    }

    /// Adds a message of Oak blocks.
    fn push(&mut self, role: &str, content: Vec<Value>) {
        self.conv.messages.push(json!({"role": role, "content": content}));
    }

    fn note(&mut self, text: &str, bad: bool) {
        self.notes.push((text.to_string(), bad));
    }

    /// Applies one stream frame; true when anything shown changed.
    fn apply(&mut self, event: &str, id: &str, data: &str) -> bool {
        if event == "catch_up_truncated" {
            self.note("Some earlier activity was too long to replay.", false);
            return self.changed();
        }
        if event != "client_event" {
            return false;
        }
        let Ok(frame) = serde_json::from_str::<Value>(data) else { return false };
        let at = id.parse::<u64>().ok().or(frame["sequence_num"].as_u64()).unwrap_or(0);
        if at > 0 && at <= self.sequence {
            return false;
        }
        self.sequence = self.sequence.max(at);
        let p = &frame["payload"];
        let source = frame["source"].as_str().unwrap_or("");
        let changed = match p["type"].as_str().unwrap_or("") {
            "user" => self.user(p, source),
            "assistant" => self.assistant(p),
            "result" => self.result(p),
            "control_request" if source == "worker" && p["request"]["subtype"] == "can_use_tool" => {
                let r = &p["request"];
                self.pending = Some((p["request_id"].as_str().unwrap_or("").into(), r["tool_name"].as_str().unwrap_or("").into(), r["input"].clone()));
                true
            }
            "control_response" => self.settled(p["response"]["request_id"].as_str()),
            "control_cancel_request" => self.settled(p["request_id"].as_str()),
            "rate_limit_event" => {
                let info = &p["rate_limit_info"];
                if info["status"] == "rejected" {
                    let resets = info["resetsAt"].as_f64().unwrap_or(0.0);
                    if resets > 0.0 {
                        self.limited_until = (resets * 1000.0) as i64;
                        note_limit(&self.rid, self.limited_until);
                    }
                    true
                } else if info["status"].is_string() {
                    std::mem::replace(&mut self.limited_until, 0) > 0
                } else {
                    false
                }
            }
            "env_manager_log" => {
                let line = p["data"]["content"].as_str().unwrap_or("").trim();
                if line.is_empty() {
                    false
                } else {
                    self.status = one_line(line, 160);
                    self.working
                }
            }
            _ => false,
        };
        if changed { self.changed() } else { false }
    }

    fn settled(&mut self, request: Option<&str>) -> bool {
        if self.pending.as_ref().is_some_and(|p| Some(p.0.as_str()) == request) {
            self.pending = None;
            return true;
        }
        false
    }

    /// A message to the agent. The worker's own user frames are tool
    /// results, which go with their calls.
    fn user(&mut self, p: &Value, source: &str) -> bool {
        let content = &p["message"]["content"];
        if source != "client" {
            if !p["parent_tool_use_id"].is_null() {
                return false;
            }
            let results: Vec<Value> = content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result").map(oak_block).collect();
            if results.is_empty() {
                return false;
            }
            self.push("user", results);
            return true;
        }
        if p["isMeta"] == true {
            return false;
        }
        if let Some(u) = p["uuid"].as_str()
            && !self.uuids.insert(u.to_string())
        {
            return false;
        }
        let provider = Provider::from_rid(&self.rid);
        let tags = provider.tags();
        let (carry_open, carry_close) = (tags.carry.0.as_str(), tags.carry.1.as_str());
        let mut typed = text_of(content);
        typed = strip_block(&typed, &tags.setup.0, &tags.setup.1).unwrap_or(typed);
        // A first message someone else started the session with (a
        // scheduled job, say), shown as the kit's note about it.
        for (open, close, note) in provider.kit().map(|k| k.notes()).unwrap_or_default() {
            if let Some(t) = strip_block(&typed, &open, &close) {
                typed = t;
                self.note(&note, false);
            }
        }
        let carried = strip_block(&typed, carry_open, carry_close);
        typed = carried.clone().unwrap_or(typed);
        let (text, files) = split_files_for(provider, &typed);
        let mut blocks: Vec<Value> = if carried.is_some() {
            vec![]
        } else {
            content.as_array().into_iter().flatten().filter(|b| b["type"] == "image").map(oak_block).collect()
        };
        for f in &files {
            blocks.push(json!({"type": "text", "text": format!("[file {f}]"), "_oak_file": f}));
        }
        if !text.trim().is_empty() {
            blocks.push(json!({"type": "text", "text": text.trim()}));
        }
        if carried.is_some() {
            self.note(CARRY_NOTE, false);
        }
        if !blocks.is_empty() {
            self.push("user", blocks);
        }
        self.working = true;
        self.status.clear();
        true
    }

    fn assistant(&mut self, p: &Value) -> bool {
        // A subagent's inner turns would bury the main thread.
        if !p["parent_tool_use_id"].is_null() {
            return false;
        }
        self.working = true;
        self.status.clear();
        let blocks: Vec<Value> = p["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| matches!(b["type"].as_str(), Some("text" | "thinking" | "tool_use")))
            .map(oak_block)
            .collect();
        if !blocks.is_empty() {
            self.push("assistant", blocks);
        }
        true
    }

    fn result(&mut self, p: &Value) -> bool {
        self.working = false;
        self.status.clear();
        self.pending = None;
        match p["subtype"].as_str() {
            Some("error_during_execution") => self.note("Stopped.", false),
            Some("error_max_turns") => self.note("Stopped at the turn limit.", false),
            sub if p["is_error"] == true || sub.is_some_and(|s| s != "success") => {
                let said = p["result"].as_str().unwrap_or("").trim().to_string();
                // A usage limit arrives as the agent's words and then as the result.
                let last = self.conv.messages.last().and_then(|m| m["content"].as_array()).and_then(|c| c.last()).and_then(|b| b["text"].as_str());
                if said.is_empty() || last != Some(said.as_str()) {
                    self.note(&if said.is_empty() { "The turn ended with an error.".into() } else { one_line(&said, 400) }, true);
                }
            }
            _ => {}
        }
        true
    }

    /// The user sent this from here: shown at once.
    fn sent(&mut self, id: &str, text: &str, images: &[Value], names: &[String]) {
        self.uuids.insert(id.into());
        let mut blocks: Vec<Value> = images.to_vec();
        for f in names {
            blocks.push(json!({"type": "text", "text": format!("[file {f}]"), "_oak_file": f}));
        }
        if !text.trim().is_empty() {
            blocks.push(json!({"type": "text", "text": text.trim()}));
        }
        if !blocks.is_empty() {
            self.push("user", blocks);
        }
        self.working = true;
        self.status.clear();
        self.changed();
    }
}

// Talking to a session
/// A picture goes in the message; any other file goes to the kit's server,
/// named in the message for the session's tool to pull (without a kit, a
/// session takes pictures only). Returns the pictures and the names.
fn upload_attachments(provider: Provider, attachments: &[Value]) -> Result<(Vec<Value>, Vec<String>), String> {
    use base64::Engine;
    let mut images = vec![];
    let mut names = vec![];
    for b in attachments {
        if b["type"] == "image" {
            images.push(b.clone());
            continue;
        }
        let name = b["_oak_file"].as_str().or(b["title"].as_str()).unwrap_or("file").to_string();
        if !provider.has_kit() {
            return Err(format!("{name} can't go to this session: it takes pictures only."));
        }
        let bytes = match (b["media_type"].as_str(), b["data"].as_str()) {
            (Some("text/plain"), Some(d)) => d.as_bytes().to_vec(),
            (Some(_), Some(d)) => base64::engine::general_purpose::STANDARD.decode(d).map_err(|e| e.to_string())?,
            // Too big to attach: OAK only kept its path, which the text names.
            _ => {
                let path = b["_oak_path"].as_str().map(String::from);
                match path.map(std::fs::read) {
                    Some(Ok(bytes)) => bytes,
                    _ => return Err(format!("{name} is too big to send to a cloud session from here")),
                }
            }
        };
        names.push(upload(provider, &name, bytes)?);
    }
    Ok((images, names))
}

/// Puts a file on the kit's server under its name, or the first numbered
/// one free ("notes (2).pdf").
fn upload(provider: Provider, name: &str, bytes: Vec<u8>) -> Result<String, String> {
    let base: String = name.replace(['/', '\\', '\0'], "_").trim_start_matches('.').to_string();
    let base = if base.is_empty() { "file".to_string() } else { base };
    for n in 1..100 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            match base.rfind('.') {
                Some(dot) if dot > 0 => format!("{} ({n}){}", &base[..dot], &base[dot..]),
                _ => format!("{base} ({n})"),
            }
        };
        let path = format!("/files/{}", crate::web::percent(&candidate));
        match provider.raw(reqwest::Method::PUT, &path, Some((bytes.clone(), "application/octet-stream")), Duration::from_secs(600)) {
            Ok(_) => {
                STATE.lock().unwrap().remote_mut(provider).files_at = None;
                return Ok(candidate);
            }
            Err(e) if e.contains("HTTP 409") => continue,
            Err(e) => return Err(format!("Couldn't upload {name}: {e}")),
        }
    }
    Err(format!("Couldn't find a free name for {name}"))
}

/// Sends a message to the session behind `key`, in the background.
pub fn send(key: &str, text: String, attachments: Vec<Value>, ctx: &eframe::egui::Context) {
    let Some(chat) = chat(key) else { return };
    if !accessible(key) {
        chat.lock().unwrap().note(&format!("Not sent: {}", inaccessible(key)), true);
        return;
    }
    let provider = provider(key);
    let ctx = ctx.clone();
    chat.lock().unwrap().sending = true;
    std::thread::spawn(move || {
        let r = upload_attachments(provider, &attachments).and_then(|(images, names)| {
            let id = api::new_uuid();
            let message = wrap_files(provider, &text, &names);
            let (rid, sid) = {
                let mut c = chat.lock().unwrap();
                c.sent(&id, &text, &images, &names);
                (c.rid.clone(), c.id.clone())
            };
            ctx.request_repaint();
            post_events(&rid, &sid, vec![mode_request(), thinking_request(), user_event(&id, &message, &images, &cse(&sid))])
        });
        let mut c = chat.lock().unwrap();
        c.sending = false;
        if let Err(e) = r {
            c.working = false;
            c.note(&format!("Not sent: {e}"), true);
            c.changed();
        }
        drop(c);
        stale_list();
        ctx.request_repaint();
    });
}

/// Stops the turn under way.
pub fn interrupt(key: &str) {
    let Some(chat) = chat(key) else { return };
    let (rid, id) = {
        let c = chat.lock().unwrap();
        (c.rid.clone(), c.id.clone())
    };
    std::thread::spawn(move || {
        if let Err(e) = post_events(&rid, &id, vec![control_request(json!({"subtype": "interrupt"}))]) {
            chat.lock().unwrap().note(&format!("Couldn't stop it: {e}"), true);
        }
    });
}

/// Answers the agent's request to use a tool.
pub fn answer(key: &str, allow: bool) {
    let Some(chat) = chat(key) else { return };
    let (rid, id, pending) = {
        let mut c = chat.lock().unwrap();
        let Some(p) = c.pending.take() else { return };
        c.changed();
        (c.rid.clone(), c.id.clone(), p)
    };
    std::thread::spawn(move || {
        let response = if allow {
            json!({"behavior": "allow", "updatedInput": pending.2})
        } else {
            json!({"behavior": "deny", "message": "Denied from Oak."})
        };
        let p = json!({"type": "control_response", "response": {"subtype": "success", "request_id": pending.0, "response": response}, "uuid": api::new_uuid()});
        if let Err(e) = post_events(&rid, &id, vec![p]) {
            chat.lock().unwrap().note(&format!("Couldn't answer: {e}"), true);
        }
    });
}

/// Archives the session: it leaves every OAK's list.
pub fn archive(key: &str, ctx: &eframe::egui::Context) {
    let Some((rid, id)) = parse_key(key) else { return };
    let (key, ctx) = (key.to_string(), ctx.clone());
    std::thread::spawn(move || {
        let r = call(&rid, reqwest::Method::POST, &format!("/v1/code/sessions/{}/archive", cse(&id)), false, false, Some(&json!({})));
        let mut s = STATE.lock().unwrap();
        match r {
            Ok(_) => {
                s.remote_mut(provider(&key)).list.retain(|x| x.key != key);
                s.chats.remove(&key);
            }
            Err(e) if e.contains("HTTP 409") => s.remote_mut(provider(&key)).list.retain(|x| x.key != key),
            Err(e) => {
                if let Some(c) = s.chats.get(&key) {
                    c.lock().unwrap().note(&format!("Couldn't archive: {e}"), true);
                }
            }
        }
        drop(s);
        ctx.request_repaint();
    });
}

// New sessions
/// The first account, in the provider's order, not known to be out (and
/// not `except`).
fn pick_account(provider: Provider, except: Option<&str>) -> Result<String, String> {
    let accounts = provider.accounts();
    if accounts.is_empty() {
        return Err(provider.unavailable().unwrap_or_else(|| NO_ACCOUNTS.into()));
    }
    let now = api::now_ms();
    let open: Vec<_> = accounts.iter().filter(|a| Some(a.rid.as_str()) != except).collect();
    if let Some(a) = open.iter().find(|a| a.blocked_until <= now) {
        return Ok(a.rid.clone());
    }
    let soonest = open.iter().map(|a| a.blocked_until).min().unwrap_or(0);
    Err(format!(
        "{} at its usage limit{}.",
        if except.is_some() { "Every other account is" } else { "Every account is" },
        if soonest > now { format!(" until {}", crate::time::clock(soonest)) } else { String::new() }
    ))
}

/// Whether the session on `rid` can move: its provider has a kit (the
/// conversation travels through its server), and another account than
/// `rid` isn't known to be out.
pub fn can_move(rid: &str) -> bool {
    let now = api::now_ms();
    let provider = Provider::from_rid(rid);
    let accounts = provider.accounts();
    provider.has_kit() && accounts.iter().any(|a| a.rid == rid) && accounts.iter().any(|a| a.rid != rid && a.blocked_until <= now)
}

/// A session is being started.
pub fn starting() -> bool {
    STATE.lock().unwrap().starting
}

/// How the last start went, once: the new session's key, or why not.
pub fn take_started() -> Option<Result<String, String>> {
    STATE.lock().unwrap().started.take()
}

/// A conversation carried over: its block, the pictures it brings, and the
/// account it leaves.
struct Carry {
    block: String,
    pictures: Vec<Value>,
    except: String,
}

/// Claims the start (one at a time), forgetting how the last went; false
/// when one is under way.
fn claim_start() -> bool {
    let mut s = STATE.lock().unwrap();
    if s.starting {
        return false;
    }
    s.starting = true;
    s.started = None;
    true
}

/// Runs `op` on its own thread, records its result as the last start and
/// repaints. The caller has already claimed the start (`claim_start`).
fn record_started(op: impl FnOnce() -> Result<String, String> + Send + 'static, ctx: &eframe::egui::Context) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let r = op();
        let mut s = STATE.lock().unwrap();
        s.starting = false;
        s.started = Some(r);
        drop(s);
        ctx.request_repaint();
    });
}

/// Starts a new session with the message, in the background; `take_started`
/// says how it went.
pub fn start(provider: Provider, text: String, attachments: Vec<Value>, ctx: &eframe::egui::Context) {
    if !claim_start() {
        return;
    }
    record_started(move || {
        upload_attachments(provider, &attachments).and_then(|(images, names)| {
            let title = title_for(if text.trim().is_empty() { names.first().map(String::as_str).unwrap_or("Picture") } else { &text });
            create(provider, &title, &text, &images, &names, None)
        })
    }, ctx);
}

/// Makes the session: on an account with room (not `carry`'s), in its OAK
/// environment, its first message the setup block, `carry`'s block and
/// what was typed.
fn create(provider: Provider, title: &str, typed: &str, images: &[Value], names: &[String], carry: Option<Carry>) -> Result<String, String> {
    let rid = pick_account(provider, carry.as_ref().map(|c| c.except.as_str()))?;
    let env = environment(&rid)?;
    let id = api::new_uuid();
    let mut message = wrap_files(provider, typed, names);
    let mut pictures: Vec<Value> = vec![];
    if let Some(c) = &carry {
        message = if message.is_empty() { c.block.clone() } else { format!("{}\n\n{message}", c.block) };
        pictures.extend(c.pictures.iter().cloned());
    }
    pictures.extend(images.iter().cloned());
    // What a new session is told first, when its kit has something to say.
    let setup = provider.kit().map(|k| k.setup_block()).unwrap_or_default();
    let first = match (setup.is_empty(), message.is_empty()) {
        (true, _) => message,
        (false, true) => setup,
        (false, false) => format!("{setup}\n\n{message}"),
    };
    let made = call(
        &rid,
        reqwest::Method::POST,
        "/v1/sessions",
        true,
        true,
        Some(&json!({
            "title": title,
            "events": [
                {"type": "event", "data": mode_request()},
                {"type": "event", "data": thinking_request()},
                {"type": "event", "data": user_event(&id, &first, &pictures, "")},
            ],
            "session_context": {"sources": [], "outcomes": []},
            "environment_id": env,
        })),
    )?;
    let sid = made["id"].as_str().unwrap_or("").to_string();
    if !valid_id(&sid) {
        return Err("The cloud returned an unexpected session id.".into());
    }
    let k = key(&rid, &sid);
    let mut chat = Chat::new(&rid, &sid, title);
    if carry.is_some() {
        chat.note(CARRY_NOTE, false);
    }
    if carry.is_none() || !typed.trim().is_empty() || !images.is_empty() || !names.is_empty() {
        chat.sent(&id, typed, images, names);
    } else {
        chat.uuids.insert(id.clone());
        chat.working = true;
        chat.changed();
    }
    let account = provider.accounts().into_iter().find(|a| a.rid == rid).map(|a| a.label).unwrap_or_default();
    let mut s = STATE.lock().unwrap();
    s.chats.insert(k.clone(), Arc::new(Mutex::new(chat)));
    s.remote_mut(provider).list.insert(0, Listed { key: k.clone(), account, title: title.into(), status: "running".into(), updated: api::now_ms() });
    Ok(k)
}

/// Moves the session behind `key`, whose account is out, to a new session on
/// another account, with what was typed; `take_started` says how it went.
pub fn move_chat(key: &str, text: String, attachments: Vec<Value>, ctx: &eframe::egui::Context) {
    let provider = provider(key);
    let Some(chat) = chat(key) else { return };
    if !claim_start() {
        return;
    }
    let (rid, id, title, limited) = {
        let mut c = chat.lock().unwrap();
        c.moving = true;
        (c.rid.clone(), c.id.clone(), c.title.clone(), c.limited_until)
    };
    if limited > api::now_ms() {
        note_limit(&rid, limited);
    }
    record_started(move || {
        let r = (|| {
            let (images, names) = upload_attachments(provider, &attachments)?;
            let (text_out, pictures) = carry_text(&title, &history(&rid, &id)?);
            let stored = provider.raw(reqwest::Method::POST, "/conversations", Some((text_out.into_bytes(), "text/plain; charset=utf-8")), Duration::from_secs(120))?;
            let v: Value = stored.json()?;
            let cid = v["id"].as_str().ok_or("The conversation wasn't stored.")?.to_string();
            create(provider, &title, &text, &images, &names, Some(Carry { block: carry_block(provider, &title, &cid), pictures, except: rid.clone() }))
        })()
        .map_err(|e: String| format!("Couldn't move the session: {e}"));
        chat.lock().unwrap().moving = false;
        r
    }, ctx);
}

// Files
const FILES_FRESH: Duration = Duration::from_secs(15);

fn want_files(provider: Provider, ctx: &eframe::egui::Context) {
    if !provider.has_kit() {
        return;
    }
    let Some(generation) = start_fetch(provider, FILES_FRESH, |s| (&mut s.files_loading, s.files_at)) else { return };
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let r = provider.json(reqwest::Method::GET, "/files");
        update_remote(provider, Some(generation), &ctx, |s| {
            match r {
                Ok(v) => {
                    s.files = v
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|f| File { name: f["name"].as_str().unwrap_or("").into(), size: f["size"].as_u64().unwrap_or(0), modified: instant(&f["modified"]) })
                        .filter(|f| !f.name.is_empty())
                        .collect();
                    s.files_error = None;
                }
                Err(e) => s.files_error = Some(e),
            }
            s.files_loading = false;
            s.files_at = Some(Instant::now());
        });
    });
}

/// The files on the kit's server, newest first, and why listing failed.
pub fn files(provider: Provider) -> (Vec<File>, Option<String>) {
    let state = STATE.lock().unwrap();
    let s = state.remote(provider);
    (s.files.clone(), s.files_error.clone())
}

/// Saves a file from the kit's server to `dest`, in the background.
pub fn save_file(provider: Provider, name: &str, dest: std::path::PathBuf, ctx: &eframe::egui::Context) {
    let (name, ctx) = (name.to_string(), ctx.clone());
    std::thread::spawn(move || {
        let r = provider.raw(reqwest::Method::GET, &format!("/files/{}", crate::web::percent(&name)), None, Duration::from_secs(600))
            .and_then(|r| r.bytes())
            .and_then(|b| std::fs::write(&dest, &b).map_err(|e| e.to_string()));
        update_remote(provider, None, &ctx, |s| s.files_error = r.err().map(|e| format!("Couldn't save {name}: {e}")));
    });
}

/// Deletes a file from the kit's server, in the background.
pub fn delete_file(provider: Provider, name: &str, ctx: &eframe::egui::Context) {
    let (name, ctx) = (name.to_string(), ctx.clone());
    std::thread::spawn(move || {
        let r = provider.raw(reqwest::Method::DELETE, &format!("/files/{}", crate::web::percent(&name)), None, SHORT);
        update_remote(provider, None, &ctx, |s| {
            match r {
                Ok(_) => s.files.retain(|f| f.name != name),
                Err(e) => s.files_error = Some(format!("Couldn't delete {name}: {e}")),
            }
            s.files_at = None;
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64, source: &str, payload: Value) -> String {
        json!({"sequence_num": seq, "source": source, "payload": payload}).to_string()
    }

    /// A kit's setup block, in the tags sessions without a kit are read with.
    const SETUP_BLOCK: &str = "<oak-setup>\nYou were started from Oak. Run `oak help` first.\n</oak-setup>";

    #[test]
    fn events_build_the_transcript() {
        let mut c = Chat::new("r", "session_x", "T");
        let first = format!("{SETUP_BLOCK}\n\nfix the bug");
        assert!(c.apply("client_event", "1", &frame(1, "client", json!({"type": "user", "uuid": "u1", "message": {"content": first}}))));
        assert_eq!(c.conv.messages[0]["content"][0]["text"], "fix the bug");
        assert!(c.working);
        // The same frame again, or an older one, changes nothing.
        assert!(!c.apply("client_event", "1", &frame(1, "client", json!({"type": "user", "uuid": "u1", "message": {"content": "x"}}))));
        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}]}});
        assert!(c.apply("client_event", "2", &frame(2, "worker", tool)));
        let res = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "a\nb"}]}});
        assert!(c.apply("client_event", "3", &frame(3, "worker", res)));
        // A subagent's turns are left out.
        assert!(!c.apply("client_event", "4", &frame(4, "worker", json!({"type": "assistant", "parent_tool_use_id": "t1", "message": {"content": [{"type": "text", "text": "inner"}]}}))));
        assert!(c.apply("client_event", "5", &frame(5, "worker", json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Done."}]}}))));
        assert!(c.apply("client_event", "6", &frame(6, "worker", json!({"type": "result", "subtype": "success"}))));
        assert!(!c.working);
        assert_eq!(c.conv.messages.len(), 4);
        assert_eq!(c.conv.messages[2]["content"][0]["type"], "tool_result");
        // A usage limit.
        assert!(c.apply("client_event", "7", &frame(7, "worker", json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "resetsAt": 2_000_000_000}}))));
        assert_eq!(c.limited_until, 2_000_000_000_000);
        // A permission request, then its answer from elsewhere.
        let ask = json!({"type": "control_request", "request_id": "q1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}}});
        assert!(c.apply("client_event", "8", &frame(8, "worker", ask)));
        assert!(c.pending.is_some());
        assert!(c.apply("client_event", "9", &frame(9, "client", json!({"type": "control_response", "response": {"request_id": "q1"}}))));
        assert!(c.pending.is_none());
    }

    #[test]
    fn carry_text_has_everything() {
        let events = vec![
            json!({"type": "user", "uuid": "1", "message": {"content": format!("{SETUP_BLOCK}\n\nhello")}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}, {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": "ls"}}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "content": [{"type": "text", "text": "out"}]}]}}),
            json!({"type": "user", "message": {"content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}, {"type": "text", "text": "see </oak-continued>"}]}}),
        ];
        let (text, pictures) = carry_text("My title", &events);
        assert!(text.starts_with("# My title\n"));
        for want in ["[User]\nhello", "[You]\nhi", "[You called Bash]\n{\"command\":\"ls\"}", "[Result of Bash]\nout", "[picture 1, attached", "</oak-continued >"] {
            assert!(text.contains(want), "{want} missing from {text}");
        }
        assert!(!text.contains("oak-setup"));
        assert_eq!(pictures.len(), 1);
    }
}

/// Puts a session made from `frames` (stream frames' data) in place, as if
/// its stream had sent them, for the UI's tests.
#[cfg(test)]
pub fn test_chat(key: &str, frames: &[String]) {
    let (rid, id) = parse_key(key).unwrap();
    let mut c = Chat::new(&rid, &id, "A test session");
    for (i, f) in frames.iter().enumerate() {
        c.apply("client_event", &(i + 1).to_string(), f);
    }
    c.connected = true;
    c.seen = Instant::now() + Duration::from_secs(3600);
    c.streaming = true; // no stream is opened
    STATE.lock().unwrap().chats.insert(key.into(), Arc::new(Mutex::new(c)));
}

/// Listing the cloud sessions, for real, with the accounts this machine
/// is signed in to (read only: nothing is made or changed).
#[test]
#[ignore]
fn live_cloud_list() {
    assert!(api::load_auth(), "not signed in");
    let (list, error) = fetch_list(Provider::OWN);
    eprintln!("{} sessions; error: {error:?}", list.len());
    for x in list.iter().take(5) {
        eprintln!("  {} | {} | {} | {}", x.account, x.status, x.title, x.key);
    }
    assert!(error.is_none(), "{error:?}");
    if let Some(x) = list.first() {
        let (rid, id) = parse_key(&x.key).unwrap();
        let events = history(&rid, &id).expect("history");
        eprintln!("first session: {} events", events.len());
        let (text, _) = carry_text(&x.title, &events);
        eprintln!("carried: {} bytes", text.len());
        let org = organization(&rid).unwrap();
        // The stream replays the session from the start.
        let chat = Arc::new(Mutex::new(Chat::new(&rid, &id, &x.title)));
        chat.lock().unwrap().seen = Instant::now() + Duration::from_secs(3600);
        let path = format!("/v1/code/sessions/{}/events/stream", cse(&id));
        let resp = request(&rid, reqwest::Method::GET, &path, None, Some(&org), None, Duration::from_secs(8)).unwrap();
        assert!(resp.ok(), "{}", resp.status);
        read_events(resp, &chat, &eframe::egui::Context::default());
        let c = chat.lock().unwrap();
        eprintln!("streamed: {} messages, up to event {}, working {}", c.conv.messages.len(), c.sequence, c.working);
        assert!(!c.conv.messages.is_empty());
    }
}
