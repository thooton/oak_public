// Codex: the GPT models a ChatGPT plan includes, through the backend the
// Codex CLI uses. Signing in is the CLI's OAuth (PKCE, a browser, a callback
// on localhost:1455); the login is kept in codex.json. Requests are the
// Responses API (POST chatgpt.com/backend-api/codex/responses): an Oak
// request is converted to one on the way out, and the Responses events back
// into Oak events. How they look follows pi (github.com/earendil-works/pi).
//
// Connected to a vault, the login is kept there instead: it alone
// renews it, and every OAK asks it for an access token (POST /codex/token).
//
// The reasoning comes back encrypted (store is off): each reasoning item goes
// in its reasoning block's `sig`, as JSON, and goes back as it came.

use crate::api::{self, Limit, Sse};
use crate::oak::{self, Event, Request};
use crate::provider;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PORT: u16 = 1455;
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// Who the requests say they come from: the Codex CLI, as the backend
/// expects of a Codex login.
const ORIGINATOR: &str = "codex_cli_rs";
/// Where the login server and the Codex backend are: OpenAI's, unless
/// OAK_CODEX_AUTH_URL and OAK_CODEX_URL say otherwise (for testing against
/// a stand-in).
fn auth_base() -> String {
    std::env::var("OAK_CODEX_AUTH_URL").unwrap_or_else(|_| "https://auth.openai.com".into())
}

fn backend() -> String {
    std::env::var("OAK_CODEX_URL").unwrap_or_else(|_| "https://chatgpt.com/backend-api".into())
}
/// The access token's claim with the ChatGPT account and plan.
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// How long before it expires an access token is renewed (ms).
const RENEW_EARLY_MS: i64 = 5 * 60_000;
/// How long a browser sign-in waits for the callback.
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);

/// A model a ChatGPT plan includes for Codex.
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
    /// The context window (tokens).
    pub context: u64,
    pub max_output: u64,
    /// Offered when starting a conversation (the others serve only
    /// conversations and subagents that already use them).
    pub listed: bool,
    /// The reasoning efforts it takes (`agent::efforts`).
    pub efforts: &'static [&'static str],
}

const fn m(id: &'static str, name: &'static str, context: u64, listed: bool, efforts: &'static [&'static str]) -> Model {
    Model { id, name, context, max_output: 128_000, listed, efforts }
}

/// Low to max, and ultra for the bigger models: each model's
/// `supported_reasoning_levels` in the backend's catalog
/// (GET /codex/models, October 2026). The Lunas stop at max.
const TO_ULTRA: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
const TO_MAX: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// The Codex models, as pi's catalog has them (October 2026).
pub const MODELS: &[Model] = &[
    m("gpt-6-astra", "GPT-6 Astra", 272_000, true, TO_ULTRA),
    m("gpt-6.1-sol", "GPT-6.1 Sol", 500_000, true, TO_ULTRA),
    m("gpt-6.1-terra", "GPT-6.1 Terra", 500_000, true, TO_ULTRA),
    m("gpt-6-luna", "GPT-6 Luna", 272_000, true, TO_MAX),
    m("gpt-6-sol", "GPT-6 Sol", 272_000, false, TO_ULTRA),
    m("gpt-5.6-sol", "GPT-5.6 Sol", 272_000, true, TO_ULTRA),
    m("gpt-5.6-terra", "GPT-5.6 Terra", 272_000, false, TO_ULTRA),
    m("gpt-5.6-luna", "GPT-5.6 Luna", 272_000, false, TO_MAX),
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// Whether `id` is a Codex model (requests for it go through the Codex login).
pub fn serves(id: &str) -> bool {
    model(id).is_some()
}

// ---------------------------------------------------------------- the login

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Cred {
    access: String,
    /// Empty for a login on the vault.
    #[serde(default)]
    refresh: String,
    /// When the access token expires (ms since 1970).
    expires: i64,
    account_id: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    plan: String,
    /// The login is on the vault: tokens come from there.
    #[serde(default)]
    remote: bool,
}

/// A login and what was learned of its plan's usage.
#[derive(Clone)]
struct Acct {
    cred: Cred,
    limits: Vec<Limit>,
    /// The usage has been fetched (or failed to be: `error`).
    fetched: bool,
    error: Option<String>,
    usage_at: i64,
    /// Its plan turned a request down for its limit until then (ms).
    limited_until: i64,
}

impl Acct {
    fn new(cred: Cred) -> Acct {
        Acct { cred, limits: vec![], fetched: false, error: None, usage_at: 0, limited_until: 0 }
    }
}

struct State {
    list: Vec<Acct>,
    /// Why a login is on this machine although a vault is
    /// connected, when that was not asked for.
    note: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State { list: Vec::new(), note: None });
/// Held while an access token is renewed: the refresh token changes with
/// each renewal, so only one may go at a time.
static RENEWING: Mutex<()> = Mutex::new(());

fn path() -> std::path::PathBuf {
    crate::store::dir().join("codex.json")
}

/// codex.json: the logins (one, from before there could be more).
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum File {
    Logins { logins: Vec<Cred> },
    One(Cred),
}

fn read_file() -> Vec<Cred> {
    match crate::store::read_json(&path()) {
        Some(File::Logins { logins }) => logins,
        Some(File::One(c)) => vec![c],
        None => vec![],
    }
}

fn save(s: &State) {
    if s.list.is_empty() {
        let _ = std::fs::remove_file(path());
        return;
    }
    let file = File::Logins { logins: s.list.iter().map(|a| a.cred.clone()).collect() };
    let _ = crate::store::write_private_json(&path(), &file);
}

/// Reads the logins from codex.json. True when there is one.
pub fn load() -> bool {
    let creds = read_file();
    let mut s = STATE.lock().unwrap();
    let old = std::mem::take(&mut s.list);
    s.list = creds.into_iter().map(|c| old.iter().find(|a| a.cred.account_id == c.account_id).map_or_else(|| Acct::new(c.clone()), |a| Acct { cred: c.clone(), ..a.clone() })).collect();
    !s.list.is_empty()
}

/// Signed in to Codex.
pub fn on() -> bool {
    !STATE.lock().unwrap().list.is_empty()
}

/// Keeps login `c` (in place of one of the same ChatGPT account).
fn keep(c: Cred) {
    let mut s = STATE.lock().unwrap();
    match s.list.iter_mut().find(|a| a.cred.account_id == c.account_id) {
        Some(a) => a.cred = c,
        None => s.list.push(Acct::new(c)),
    }
    save(&s);
}

fn cred_of(id: &str) -> Option<Cred> {
    STATE.lock().unwrap().list.iter().find(|a| a.cred.account_id == id).map(|a| a.cred.clone())
}

/// Signs login `id` out: one on the vault is removed there, for
/// every OAK (when it can't be reached, it stays, and Err says why).
pub fn sign_out(id: &str) -> Result<(), String> {
    if cred_of(id).is_some_and(|c| c.remote) {
        api::vault_remove("codex", id)?;
    }
    forget(id);
    Ok(())
}

/// Drops login `id` on this machine.
fn forget(id: &str) {
    let mut s = STATE.lock().unwrap();
    s.list.retain(|a| a.cred.account_id != id);
    s.note = None;
    save(&s);
    drop(s);
    crate::rotation::forget_account(api::Provider::Codex, id);
}

/// Disconnected from the vault: its logins go.
pub fn forget_remote() {
    let mut s = STATE.lock().unwrap();
    let n = s.list.len();
    s.list.retain(|a| !a.cred.remote);
    if s.list.len() != n {
        save(&s);
    }
}

/// A login on the vault as this machine keeps it: who it is,
/// and the access token it handed out, if any.
fn remote_cred(info: &Value, access: Option<(String, i64)>) -> Cred {
    let field = |k: &str| info[k].as_str().unwrap_or("").to_string();
    let (access, expires) = access.unwrap_or_default();
    Cred { access, refresh: String::new(), expires, account_id: field("account_id"), email: field("email"), plan: field("plan"), remote: true }
}

/// The vault's logins (who each is): Err when it can't say.
fn remote_logins() -> Result<Vec<Value>, String> {
    api::vault_list("codex")
}

/// Brings the logins in step with the vault's: its logins are
/// used (one on this machine too becomes its), and ones removed there go.
pub fn sync() {
    let Ok(logins) = remote_logins() else { return };
    let ids: Vec<&str> = logins.iter().filter_map(|i| i["account_id"].as_str()).filter(|a| !a.is_empty()).collect();
    let mut s = STATE.lock().unwrap();
    let before: Vec<(String, bool)> = s.list.iter().map(|a| (a.cred.account_id.clone(), a.cred.remote)).collect();
    s.list.retain(|a| !a.cred.remote || ids.contains(&a.cred.account_id.as_str()));
    for info in &logins {
        let Some(id) = info["account_id"].as_str().filter(|a| !a.is_empty()) else { continue };
        match s.list.iter_mut().find(|a| a.cred.account_id == id) {
            Some(a) if a.cred.remote => (a.cred.email, a.cred.plan) = (info["email"].as_str().unwrap_or("").into(), info["plan"].as_str().unwrap_or("").into()),
            Some(a) => a.cred = remote_cred(info, None),
            None => s.list.push(Acct::new(remote_cred(info, None))),
        }
    }
    if s.list.iter().map(|a| (a.cred.account_id.clone(), a.cred.remote)).collect::<Vec<_>>() != before {
        save(&s);
    }
}

/// A login is the vault's.
pub fn remote() -> bool {
    STATE.lock().unwrap().list.iter().any(|a| a.cred.remote)
}

/// Login `id` is on this machine while a vault is connected: it
/// can move there.
pub fn movable(id: &str) -> bool {
    api::vault_on() && cred_of(id).is_some_and(|c| !c.remote)
}

/// Why a login stayed on this machine, when it was meant for the vault.
pub fn note() -> Option<String> {
    STATE.lock().unwrap().note.clone()
}

/// Puts a login (its tokens) on the vault; what it keeps of it.
fn upload(tokens: &Value) -> Result<Cred, String> {
    let info = api::vault_add("codex", tokens)?;
    if info["account_id"].as_str().is_none_or(str::is_empty) {
        return Err(format!("{} didn't say which Codex account it took.", api::capital(&api::vault_name())));
    }
    let access = tokens["access_token"].as_str().map(|a| (a.to_string(), tokens["expires_at"].as_i64().unwrap_or(0)));
    Ok(remote_cred(&info, access))
}

/// Moves login `id` on this machine to the vault, which from then
/// on is the only one to renew it.
pub fn move_to_vault(id: &str) -> Result<(), String> {
    // No renewal may start while it moves.
    let _renewing = RENEWING.lock().unwrap();
    let c = cred_of(id).filter(|c| !c.remote).ok_or("There is no such Codex login on this machine.")?;
    let moved = upload(&json!({"refresh_token": c.refresh, "access_token": c.access, "expires_at": c.expires, "email": c.email}))?;
    keep(moved);
    STATE.lock().unwrap().note = None;
    Ok(())
}

/// A JWT's claims (Null when it isn't one).
fn claims(token: &str) -> Value {
    let Some(part) = token.split('.').nth(1) else { return Value::Null };
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or(Value::Null)
}

/// "plus" → "Plus".
fn plan_name(p: &str) -> String {
    let mut c = p.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// The login a token response makes (`old`: the one it renews).
fn cred_from(tokens: &Value, old: Option<&Cred>) -> Result<Cred, String> {
    let access = tokens["access_token"].as_str().filter(|t| !t.is_empty()).ok_or("The sign-in gave no access token")?.to_string();
    let refresh = tokens["refresh_token"].as_str().map(String::from).or(old.map(|o| o.refresh.clone())).unwrap_or_default();
    let at = claims(&access);
    let id = tokens["id_token"].as_str().map(claims).unwrap_or(Value::Null);
    let expires = match tokens["expires_in"].as_i64() {
        Some(s) => api::now_ms() + s * 1000,
        None => at["exp"].as_i64().map_or(api::now_ms() + 3_600_000, |s| s * 1000),
    };
    let pick = |vals: [&Value; 2], old: Option<&str>| {
        vals.iter().find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from)).or(old.map(String::from)).unwrap_or_default()
    };
    let account_id = pick([&at[AUTH_CLAIM]["chatgpt_account_id"], &id[AUTH_CLAIM]["chatgpt_account_id"]], old.map(|o| o.account_id.as_str()));
    if account_id.is_empty() {
        return Err("The sign-in has no ChatGPT account (sign in with a ChatGPT plan that includes Codex)".into());
    }
    let email = pick([&id["email"], &at["https://api.openai.com/profile"]["email"]], old.map(|o| o.email.as_str()));
    let plan = pick([&id[AUTH_CLAIM]["chatgpt_plan_type"], &at[AUTH_CLAIM]["chatgpt_plan_type"]], old.map(|o| o.plan.as_str()));
    Ok(Cred { access, refresh, expires, account_id, email, plan: plan_name(&plan), remote: false })
}

/// `pairs` form-encoded (for a body, or a query).
fn form(pairs: &[(&str, &str)]) -> String {
    reqwest::Url::parse_with_params("http://x/", pairs).map(|u| u.query().unwrap_or("").to_string()).unwrap_or_default()
}

fn token_request(pairs: &[(&str, &str)]) -> Result<Value, String> {
    json_reply(
        api::http()
            .post(format!("{}/oauth/token", auth_base()))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(form(pairs)),
    )
}

/// Sends `req` and reads its JSON reply (an error reply as `error_text`).
fn json_reply(req: reqwest::blocking::RequestBuilder) -> Result<Value, String> {
    api::json_or(req.send().map_err(api::network_error)?, error_text)
}

/// Login `id`'s access token and ChatGPT account, renewed first when it is
/// about to expire, or (`force`) when the server refused it.
fn access(id: &str, force: bool) -> Result<(String, String), String> {
    const OUT: &str = "Not signed in to Codex: add the Codex account on Home";
    let fresh = |c: &Cred| c.expires - api::now_ms() > RENEW_EARLY_MS;
    let seen = cred_of(id).ok_or(OUT)?;
    if !force && fresh(&seen) {
        return Ok((seen.access, seen.account_id));
    }
    if seen.remote {
        return remote_access(&seen, force);
    }
    let _renewing = RENEWING.lock().unwrap();
    let mut c = cred_of(id).ok_or(OUT)?;
    // Renewed meanwhile, here or by another OAK (whose renewal made this
    // one's refresh token useless): that one does.
    if let Some(f) = read_file().into_iter().find(|f| f.account_id == id && f.access != c.access) {
        keep(f.clone());
        c = f;
    }
    if c.access != seen.access && fresh(&c) {
        return Ok((c.access, c.account_id));
    }
    let tokens = token_request(&[("grant_type", "refresh_token"), ("refresh_token", &c.refresh), ("client_id", CLIENT_ID)])
        .map_err(|e| format!("Couldn't renew the Codex sign-in (add the Codex account on Home again): {e}"))?;
    let new = cred_from(&tokens, Some(&c))?;
    keep(new.clone());
    Ok((new.access, new.account_id))
}

/// An access token from the vault: the one it has, or (`force`) a
/// new one when the one held here was refused.
fn remote_access(seen: &Cred, force: bool) -> Result<(String, String), String> {
    let refused = (force && !seen.access.is_empty()).then_some(seen.access.as_str());
    let id = seen.account_id.as_str();
    let Some(v) = api::vault_token("codex", id, refused)? else {
        forget(id);
        return Err(format!("The Codex login was removed from {}: add it again on Home", api::vault_name()));
    };
    let access = v["access_token"].as_str().filter(|a| !a.is_empty()).ok_or_else(|| format!("{} sent no Codex token.", api::capital(&api::vault_name())))?.to_string();
    let mut c = seen.clone();
    c.access = access;
    c.expires = v["expires_at"].as_i64().unwrap_or(0);
    if let Some(a) = v["account_id"].as_str().filter(|a| !a.is_empty()) {
        c.account_id = a.to_string();
    }
    keep(c.clone());
    Ok((c.access, c.account_id))
}

/// A browser sign-in under way: the page to open, and a local server
/// waiting for the browser to come back to it.
pub struct Login {
    pub url: String,
    verifier: String,
    state: String,
    /// What the callback brought: None while waiting.
    result: Arc<Mutex<Option<Result<(), String>>>>,
    stop: Arc<AtomicBool>,
    /// The callback server is listening (else the address bar's URL must
    /// be pasted).
    pub listening: bool,
}

impl Drop for Login {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts a sign-in: the URL to open in the browser, with a server on
/// localhost:1455 for the browser to come back to.
pub fn start_login() -> Login {
    let bytes: [u8; 32] = rand::random();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state: String = api::hex(&rand::random::<[u8; 16]>());
    let query = form(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ]);
    let mut login = Login {
        url: format!("{}/oauth/authorize?{query}", auth_base()),
        verifier,
        state,
        result: Arc::default(),
        stop: Arc::default(),
        listening: false,
    };
    if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        && listener.set_nonblocking(true).is_ok()
    {
        login.listening = true;
        let (verifier, state, result, stop) = (login.verifier.clone(), login.state.clone(), login.result.clone(), login.stop.clone());
        std::thread::spawn(move || serve_callback(listener, &verifier, &state, &result, &stop));
    }
    login
}

/// Answers the browser's requests until one brings the sign-in back (and
/// it is finished), the login is dropped, or the wait runs out.
fn serve_callback(listener: std::net::TcpListener, verifier: &str, state: &str, result: &Mutex<Option<Result<(), String>>>, stop: &AtomicBool) {
    let start = Instant::now();
    while !stop.load(Ordering::SeqCst) && start.elapsed() < LOGIN_WAIT {
        let Ok((mut conn, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let _ = conn.set_nonblocking(false);
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let mut reader = BufReader::new(&conn);
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        // The rest of the request (headers) is left unread.
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else { continue };
        if url.path() != "/auth/callback" {
            let _ = conn.write_all(page(404, "Not found", "").as_bytes());
            continue;
        }
        let get = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let outcome = match (get("code"), get("state"), get("error")) {
            (_, _, Some(e)) => Err(format!("The sign-in was refused: {}", get("error_description").unwrap_or(e))),
            (Some(_), Some(s), _) if s != state => Err("The sign-in came back for another request (state mismatch): try again".into()),
            (Some(code), _, _) => exchange(&code, verifier),
            (None, ..) => Err("The sign-in came back without a code".into()),
        };
        let html = match &outcome {
            Ok(()) => page(200, "Signed in to Codex", "You can close this tab and go back to Oak."),
            Err(e) => page(400, "Couldn't sign in", e),
        };
        let _ = conn.write_all(html.as_bytes());
        let _ = conn.flush();
        *result.lock().unwrap() = Some(outcome);
        return;
    }
}

fn page(status: u16, title: &str, text: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Oak</title></head>\
         <body style=\"font-family:sans-serif;max-width:32em;margin:5em auto\"><h2>{}</h2><p>{}</p></body></html>",
        esc(title),
        esc(text)
    );
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

/// How the browser sign-in ended, once it has (taken: asked again, None).
pub fn login_result(login: &Login) -> Option<Result<(), String>> {
    login.result.lock().unwrap().take()
}

/// Finishes a sign-in with what the user pasted: the URL the browser was
/// sent back to (when nothing listened for it), or the code in it.
pub fn finish_login(login: &Login, pasted: &str) -> Result<(), String> {
    let (code, state) = parse_pasted(pasted).ok_or("Paste the whole address the browser ended at (http://localhost:1455/auth/callback?code=…)")?;
    if state.as_deref().is_some_and(|s| s != login.state) {
        return Err("That address is from another sign-in: start again".into());
    }
    exchange(&code, &login.verifier)
}

/// The code (and state) in a pasted callback URL, query, "code#state" or
/// bare code.
fn parse_pasted(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let query = if let Ok(u) = reqwest::Url::parse(text) {
        u.query().unwrap_or("").to_string()
    } else if text.contains("code=") {
        text.trim_start_matches('?').to_string()
    } else {
        let (code, state) = text.split_once('#').map_or((text, None), |(c, s)| (c, Some(s.to_string())));
        return Some((code.to_string(), state));
    };
    let u = reqwest::Url::parse(&format!("http://x/?{query}")).ok()?;
    let get = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    Some((get("code").filter(|c| !c.is_empty())?, get("state")))
}

fn exchange(code: &str, verifier: &str) -> Result<(), String> {
    let tokens = token_request(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ])?;
    let local = cred_from(&tokens, None)?;
    // Connected to a vault, the login goes there; one from
    // before it kept Codex logins leaves it here.
    let mut note = None;
    let cred = if api::signing_in_remotely() {
        let up = json!({"refresh_token": local.refresh, "access_token": local.access, "expires_at": local.expires, "id_token": tokens["id_token"], "email": local.email});
        match upload(&up) {
            Ok(c) => c,
            Err(e) if api::vault_cannot_keep(&e) => {
                note = Some(format!("Kept on this machine: {e}"));
                local
            }
            Err(e) => return Err(e),
        }
    } else {
        local
    };
    keep(cred);
    STATE.lock().unwrap().note = note;
    Ok(())
}

// ---------------------------------------------------------------- usage

/// The Codex logins as the UI shows them, in the boxes Claude accounts go
/// in (`id` is unused: `key` has the login's).
pub fn accounts() -> Vec<(String, api::Account)> {
    let paired = api::vault_on();
    let s = STATE.lock().unwrap();
    s.list
        .iter()
        .map(|a| {
            let c = &a.cred;
            let view = api::Account {
                provider: api::Provider::Codex,
                name: "Codex".into(),
                email: c.email.clone(),
                plan: c.plan.clone(),
                limits: a.limits.clone(),
                fetched: a.fetched,
                error: a.error.clone(),
                usage_at: a.usage_at,
                fallback: paired && !c.remote,
                movable: paired && !c.remote,
                ..Default::default()
            };
            (c.account_id.clone(), view)
        })
        .collect()
}

/// The first login, for what shows one.
pub fn account() -> Option<api::Account> {
    accounts().into_iter().next().map(|(_, a)| a)
}

/// A window's label by its length.
fn window_label(secs: i64) -> &'static str {
    match secs {
        s if s < 86_400 => "5h",
        s if s < 20 * 86_400 => "Week",
        _ => "Month",
    }
}

/// The limits in a wham/usage reply.
fn read_usage(v: &Value) -> Vec<Limit> {
    let r = &v["rate_limit"];
    ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|k| {
            let w = &r[*k];
            let percent = w["used_percent"].as_f64()? as f32;
            let secs = w["limit_window_seconds"].as_i64().unwrap_or(if *k == "primary_window" { 18_000 } else { 604_800 });
            let resets_at = w["reset_at"].as_i64().map(|s| s * 1000).or(w["reset_after_seconds"].as_i64().map(|s| api::now_ms() + s * 1000));
            Some(Limit { label: window_label(secs), percent, resets_at })
        })
        .collect()
}

/// The limits in a reply's headers (x-codex-primary-used-percent and so
/// on), when it has them.
fn limits_in(h: &reqwest::header::HeaderMap) -> Vec<Limit> {
    let get = |k: String| h.get(k).and_then(|v| v.to_str().ok()?.trim().parse::<f64>().ok());
    ["primary", "secondary"]
        .iter()
        .filter_map(|w| {
            let percent = get(format!("x-codex-{w}-used-percent"))? as f32;
            let mins = get(format!("x-codex-{w}-window-minutes")).map_or(if *w == "primary" { 300 } else { 10_080 }, |m| m as i64);
            let resets_at = get(format!("x-codex-{w}-reset-at"))
                .map(|s| s as i64 * 1000)
                .or(get(format!("x-codex-{w}-reset-after-seconds")).map(|s| api::now_ms() + s as i64 * 1000));
            Some(Limit { label: window_label(mins * 60), percent, resets_at })
        })
        .collect()
}

fn note_limits(id: &str, limits: Vec<Limit>) {
    if limits.is_empty() {
        return;
    }
    let mut s = STATE.lock().unwrap();
    if let Some(a) = s.list.iter_mut().find(|a| a.cred.account_id == id) {
        a.limits = limits;
        a.fetched = true;
        a.error = None;
        a.usage_at = api::now_ms();
    }
}

/// Fetches each plan's usage, for the account boxes.
pub fn refresh() {
    let ids: Vec<String> = STATE.lock().unwrap().list.iter().map(|a| a.cred.account_id.clone()).collect();
    for id in ids {
        let r = access(&id, false).and_then(|(token, account)| {
            json_reply(
                api::http()
                    .get(format!("{}/wham/usage", backend()))
                    .bearer_auth(token)
                    .header("chatgpt-account-id", account)
                    .header("originator", ORIGINATOR)
                    .header("Accept", "application/json"),
            )
        });
        let mut s = STATE.lock().unwrap();
        let Some(a) = s.list.iter_mut().find(|a| a.cred.account_id == id) else { continue };
        a.fetched = true;
        match r {
            Ok(v) => {
                a.limits = read_usage(&v);
                a.error = None;
                a.usage_at = api::now_ms();
                if let Some(p) = v["plan_type"].as_str().filter(|p| !p.is_empty()) {
                    a.cred.plan = plan_name(p);
                }
            }
            Err(e) => a.error = Some(e),
        }
    }
}

/// The logins as rotation weighs them: by their tightest limit; ones on
/// this machine are a fallback once the vault has logins.
fn candidates(now: i64) -> Vec<crate::rotation::Candidate> {
    let s = STATE.lock().unwrap();
    let remote = s.list.iter().any(|a| a.cred.remote);
    s.list
        .iter()
        .map(|a| {
            let live = |l: &&Limit| l.resets_at.is_none_or(|r| r > now);
            let used = a.limits.iter().filter(live).map(|l| l.percent).fold(0.0f32, f32::max);
            let spent = a.limits.iter().filter(live).filter(|l| l.percent >= 100.0).filter_map(|l| l.resets_at).max().unwrap_or(0);
            crate::rotation::Candidate {
                id: a.cred.account_id.clone(),
                left: (100.0 - used.min(100.0)) as f64,
                usable: a.limited_until.max(spent) <= now,
                resets_at: a.limits.iter().filter(|l| l.label == "Week").find_map(|l| l.resets_at).unwrap_or(i64::MAX),
                preferred: a.cred.remote || !remote,
            }
        })
        .collect()
}

/// The login conversation `conv`'s request goes through; with every one
/// used up, Err has when the first comes free.
fn login_for(conv: &str) -> Result<String, Option<Sse>> {
    let now = api::now_ms();
    let cands = candidates(now);
    if cands.is_empty() {
        return Err(Some(Sse::Err("Not signed in to Codex: add the Codex account on Home".into())));
    }
    match crate::rotation::pick(api::Provider::Codex, conv, &cands, now, !api::rotation_on()) {
        Some(id) if cands.iter().any(|c| c.id == id && c.usable) => Ok(id),
        _ => {
            let s = STATE.lock().unwrap();
            let first = s.list.iter().map(|a| a.limited_until).filter(|&t| t > now).min().unwrap_or(now + 15 * 60_000);
            Err(Some(Sse::Limit(first)))
        }
    }
}

/// Login `id` is used up until `until`. True when another can go on.
fn limited(id: &str, until: i64) -> bool {
    let mut s = STATE.lock().unwrap();
    if let Some(a) = s.list.iter_mut().find(|a| a.cred.account_id == id) {
        a.limited_until = a.limited_until.max(until);
    }
    drop(s);
    let now = api::now_ms();
    candidates(now).iter().any(|c| c.usable)
}

fn label_of(id: &str) -> String {
    cred_of(id).map_or_else(|| "Codex".into(), |c| if c.email.is_empty() { "Codex".into() } else { c.email })
}

// ---------------------------------------------------------------- requests

/// "HTTP 400: message", from an error reply's body.
fn error_text(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = provider::error_msg(&v, body, None);
    // So the agent makes room (summarizes) and sends again.
    match v["error"]["code"].as_str() {
        Some("context_length_exceeded") => format!("HTTP {status}: prompt is too long: {msg}"),
        _ => format!("HTTP {status}: {msg}"),
    }
}

/// An Oak user block as a Responses input part.
fn input_part(b: &Value) -> Option<Value> {
    match b["type"].as_str()? {
        "text" => Some(json!({"type": "input_text", "text": b["text"]})),
        "image" => Some(json!({"type": "input_image", "detail": "auto", "image_url": provider::data_url(b)?})),
        "document" => {
            let title = b["title"].as_str().unwrap_or("document");
            if b["media_type"] == "text/plain" {
                Some(json!({"type": "input_text", "text": format!("{title}:\n{}", b["data"].as_str().unwrap_or(""))}))
            } else {
                Some(json!({"type": "input_file", "filename": title, "file_data": provider::data_url(b)?}))
            }
        }
        _ => None,
    }
}

/// A tool result's content, as a function_call_output's output: its text,
/// or parts when it has images.
fn tool_output(b: &Value) -> Value {
    let error = b["is_error"] == true;
    let mark = |t: String| if error { format!("Error: {t}") } else { t };
    match &b["content"] {
        Value::String(s) => Value::String(mark(s.clone())),
        Value::Array(a) => {
            if a.iter().all(|p| p["type"] == "text") {
                return Value::String(mark(a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")));
            }
            let mut parts: Vec<Value> = a.iter().filter_map(input_part).collect();
            if error {
                parts.insert(0, json!({"type": "input_text", "text": "Error:"}));
            }
            Value::Array(parts)
        }
        _ => Value::String(mark(String::new())),
    }
}

/// The conversation as Responses input items. Items go without ids (the
/// server keeps nothing to refer to); Codex's own reasoning goes back as it
/// came, from the reasoning blocks' `sig`; other providers' is left out.
fn input_items(messages: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    for m in messages {
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        if m["role"] == "system" {
            // A change since the prefix was cached (`crate::prefix`): its
            // tools in an `additional_tools` item, its text in a developer
            // message, both of which add to the cache.
            let defs: Vec<Value> = blocks.iter().filter(|b| b["type"] == "tool_definition").map(|b| b["definition"].clone()).collect();
            if !defs.is_empty() {
                out.push(json!({"type": "additional_tools", "role": "developer", "tools": wire_tools(&defs)}));
            }
            let text: Vec<Value> = blocks.iter().filter(|b| b["type"] == "text").map(|b| json!({"type": "input_text", "text": b["text"]})).collect();
            if !text.is_empty() {
                out.push(json!({"role": "developer", "content": text}));
            }
            continue;
        }
        if m["role"] == "assistant" {
            for b in &blocks {
                match b["type"].as_str().unwrap_or("") {
                    "reasoning" if b["by"] == "codex" => {
                        let item = b["sig"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok());
                        if let Some(mut item) = item.filter(|i| i["type"] == "reasoning") {
                            if let Some(o) = item.as_object_mut() {
                                o.remove("id");
                                o.remove("status");
                            }
                            out.push(item);
                        }
                    }
                    "text" if b["text"].as_str().is_some_and(|t| !t.is_empty()) => out.push(json!({
                        "type": "message", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": b["text"], "annotations": []}],
                    })),
                    "tool_call" => out.push(json!({
                        "type": "function_call", "call_id": b["id"], "name": b["name"],
                        "arguments": if b["input"].is_null() { "{}".to_string() } else { b["input"].to_string() },
                    })),
                    _ => {}
                }
            }
            continue;
        }
        let mut parts = vec![];
        let flush = |parts: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !parts.is_empty() {
                out.push(json!({"role": "user", "content": std::mem::take(parts)}));
            }
        };
        for b in &blocks {
            if b["type"] == "tool_result" {
                flush(&mut parts, &mut out);
                out.push(json!({"type": "function_call_output", "call_id": b["call_id"], "output": tool_output(b)}));
            } else if let Some(p) = input_part(b) {
                parts.push(p);
            }
        }
        flush(&mut parts, &mut out);
    }
    out
}

/// A key that stays the same for a conversation's requests: its id, else
/// its model and first message. For the prompt cache.
fn cache_key(req: &Request) -> String {
    let mut h = Sha256::new();
    match &req.conv {
        Some(id) => h.update(id.as_bytes()),
        None => {
            h.update(req.model.as_bytes());
            h.update(req.messages.first().map(Value::to_string).unwrap_or_default().as_bytes());
        }
    }
    api::uuid_text(&h.finalize())
}

/// The system prompt as Codex gets it (it refuses none).
pub(crate) fn instructions(system: &str) -> String {
    if system.trim().is_empty() { "You are a helpful assistant.".into() } else { system.to_string() }
}

/// OAK's tool definitions as Responses functions.
pub(crate) fn wire_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|t| t["name"].is_string() && t["input_schema"].is_object())
        .map(|t| json!({"type": "function", "name": t["name"], "description": t["description"], "parameters": t["input_schema"], "strict": false}))
        .collect()
}

/// An Oak request as a Codex Responses request.
pub(crate) fn request_body(req: &Request) -> Value {
    let tools = wire_tools(&req.tools);
    // Titles go without thinking; Codex models always reason, so as
    // little as they can.
    let effort = if !req.thinking { "low" } else { req.effort.as_deref().unwrap_or("medium") };
    let mut body = json!({
        "model": req.model,
        "store": false,
        "stream": true,
        "instructions": instructions(&req.system),
        "input": input_items(&req.messages_with_instruction()),
        "text": {"verbosity": "low"},
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": cache_key(req),
        "reasoning": {"effort": effort, "summary": "auto"},
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!(if req.tools_off { "none" } else { "auto" });
        body["parallel_tool_calls"] = json!(true);
    }
    body
}

/// Whether `body` has the stable prefix's items (developer messages, an
/// `additional_tools` item).
fn native_items(body: &Value) -> bool {
    body["input"].as_array().is_some_and(|i| i.iter().any(|x| x["role"] == "developer"))
}

/// `body` as the backend takes it when it refuses those items: the tools
/// join the list (once, which rewrites the cache) and the text goes as the
/// user's.
fn plain_body(mut body: Value) -> Value {
    let Some(items) = body["input"].as_array_mut() else { return body };
    let mut added = vec![];
    items.retain_mut(|x| {
        if x["type"] == "additional_tools" {
            added.extend(x["tools"].as_array().cloned().unwrap_or_default());
            return false;
        }
        if x["role"] == "developer" {
            x["role"] = json!("user");
        }
        true
    });
    if let Some(tools) = body["tools"].as_array_mut() {
        for t in added {
            match tools.iter_mut().find(|x| x["name"] == t["name"]) {
                Some(x) => *x = t,
                None => tools.push(t),
            }
        }
    }
    body
}

/// What one Responses event becomes.
#[derive(Debug, PartialEq)]
enum Out {
    Event(Event),
    Done,
    Err(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Reasoning,
    Text,
    Tool,
}

struct Block {
    index: usize,
    kind: Part,
    /// Deltas came (else the whole item's content goes at its end).
    streamed: bool,
    /// Reasoning summary parts so far.
    parts: usize,
}

/// Turns Responses events into Oak events.
#[derive(Default)]
struct Translate {
    model: String,
    started: bool,
    next: usize,
    /// The open blocks, by output_index.
    blocks: BTreeMap<u64, Block>,
    tools: bool,
}

impl Translate {
    fn new(model: &str) -> Self {
        Translate { model: model.to_string(), ..Default::default() }
    }

    fn delta(&mut self, at: Option<u64>, kind: Part, text: String) -> Option<Out> {
        let b = self.blocks.get_mut(&at?).filter(|b| b.kind == kind)?;
        b.streamed = true;
        let index = b.index;
        Some(Out::Event(match kind {
            Part::Reasoning => Event::Reasoning { index, text },
            Part::Text => Event::Text { index, text },
            Part::Tool => Event::Input { index, json: text },
        }))
    }

    fn event(&mut self, ev: &Value) -> Vec<Out> {
        let mut out = vec![];
        let ty = ev["type"].as_str().unwrap_or("");
        if !self.started && !matches!(ty, "error" | "response.failed") {
            self.started = true;
            let r = &ev["response"];
            out.push(Out::Event(Event::Start { id: r["id"].as_str().unwrap_or("").into(), model: r["model"].as_str().unwrap_or(&self.model).into() }));
        }
        let at = ev["output_index"].as_u64();
        let text = |k: &str| ev[k].as_str().unwrap_or("").to_string();
        match ty {
            "response.output_item.added" => out.extend(self.item_added(at, &ev["item"])),
            "response.reasoning_summary_part.added" => {
                let Some(b) = at.and_then(|a| self.blocks.get_mut(&a)) else { return out };
                b.parts += 1;
                if b.parts > 1 {
                    out.extend(self.delta(at, Part::Reasoning, "\n\n".into()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" if !text("delta").is_empty() => {
                out.extend(self.delta(at, Part::Reasoning, text("delta")));
            }
            "response.output_text.delta" | "response.refusal.delta" if !text("delta").is_empty() => {
                out.extend(self.delta(at, Part::Text, text("delta")));
            }
            "response.function_call_arguments.delta" if !text("delta").is_empty() => {
                out.extend(self.delta(at, Part::Tool, text("delta")));
            }
            "response.output_item.done" => {
                let Some(b) = at.and_then(|a| self.blocks.remove(&a)) else { return out };
                out.extend(item_done(b, &ev["item"]));
            }
            "response.completed" | "response.incomplete" | "response.done" => out.extend(self.completed(&ev["response"])),
            "response.failed" => out.push(Out::Err(stream_error(&ev["response"]["error"]))),
            "error" => out.push(Out::Err(stream_error(if ev["error"].is_object() { &ev["error"] } else { ev }))),
            _ => {}
        }
        out
    }

    /// A new output item opens a block (None for kinds Oak doesn't show).
    fn item_added(&mut self, at: Option<u64>, item: &Value) -> Option<Out> {
        let (kind, block) = match item["type"].as_str() {
            Some("reasoning") => (Part::Reasoning, json!({"type": "reasoning", "text": "", "by": "codex"})),
            Some("message") => (Part::Text, oak::text("")),
            Some("function_call") => {
                self.tools = true;
                (Part::Tool, json!({"type": "tool_call", "id": item["call_id"], "name": item["name"], "input": {}}))
            }
            _ => return None,
        };
        let index = self.next;
        self.next += 1;
        self.blocks.insert(at.unwrap_or(index as u64), Block { index, kind, streamed: false, parts: 0 });
        Some(Out::Event(Event::Block { index, block }))
    }

    /// The response ended: the open blocks close, then come its usage and why it stopped.
    fn completed(&mut self, r: &Value) -> Vec<Out> {
        let mut out: Vec<Out> = std::mem::take(&mut self.blocks).into_values().map(|b| Out::Event(Event::BlockEnd { index: b.index })).collect();
        let u = &r["usage"];
        let input = u["input_tokens"].as_u64().unwrap_or(0);
        let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(input);
        let reason = match (r["status"].as_str(), r["incomplete_details"]["reason"].as_str()) {
            (Some("incomplete"), Some("max_output_tokens")) => oak::stop::MAX_TOKENS,
            (Some("incomplete"), Some("content_filter")) => oak::stop::REFUSAL,
            _ if self.tools => oak::stop::TOOL_CALLS,
            _ => oak::stop::END,
        };
        out.push(Out::Event(Event::Usage(oak::Usage {
            input: input - cached,
            cache_read: cached,
            output: u["output_tokens"].as_u64().unwrap_or(0),
            reasoning: u["output_tokens_details"]["reasoning_tokens"].as_u64(),
            ..Default::default()
        })));
        out.push(Out::Event(Event::Stop { reason: reason.into(), detail: None }));
        out.push(Out::Done);
        out
    }
}

/// Block `b`'s item is done: what didn't stream comes whole (a reasoning
/// item also leaves its signature), then the block ends.
fn item_done(b: Block, item: &Value) -> Vec<Out> {
    let index = b.index;
    let mut out = vec![];
    match b.kind {
        Part::Reasoning => {
            let summary = item["summary"].as_array().map(|s| s.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n")).unwrap_or_default();
            if !b.streamed && !summary.is_empty() {
                out.push(Out::Event(Event::Reasoning { index, text: summary }));
            }
            let mut keep = item.clone();
            if let Some(o) = keep.as_object_mut() {
                o.remove("id");
                o.remove("status");
            }
            out.push(Out::Event(Event::Sig { index, sig: keep.to_string() }));
        }
        Part::Text if !b.streamed => {
            let t: String = item["content"].as_array().map(|c| c.iter().filter_map(|p| p["text"].as_str().or(p["refusal"].as_str())).collect()).unwrap_or_default();
            if !t.is_empty() {
                out.push(Out::Event(Event::Text { index, text: t }));
            }
        }
        Part::Tool if !b.streamed => {
            if let Some(a) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                out.push(Out::Event(Event::Input { index, json: a.into() }));
            }
        }
        _ => {}
    }
    out.push(Out::Event(Event::BlockEnd { index }));
    out
}

fn stream_error(e: &Value) -> String {
    let msg = e["message"].as_str().filter(|m| !m.is_empty()).or(e["code"].as_str()).unwrap_or("unknown error");
    match e["code"].as_str() {
        Some("context_length_exceeded") => format!("API error: prompt is too long: {msg}"),
        _ => format!("API error: {msg}"),
    }
}

/// Streams one Oak request through Codex, sending its events to `tx`.
pub fn stream(req: Request, tx: Sender<Sse>) {
    let body = request_body(&req);
    let session = body["prompt_cache_key"].as_str().unwrap_or("").to_string();
    let conv = req.seat_conv();
    let resp = match open(&body, &session, &conv, &tx) {
        // The stable prefix's items refused: again without them, and
        // without them from now on.
        Err(Some(Sse::Err(e))) if e.starts_with("Codex HTTP 400") && native_items(&body) => {
            eprintln!("Codex refused developer messages or additional_tools items: {e}");
            crate::prefix::set_codex_native(false);
            open(&plain_body(body), &session, &conv, &tx)
        }
        r => r,
    };
    match resp {
        Ok(r) => forward(r, &req.model, &tx),
        Err(Some(s)) => drop(tx.send(s)),
        Err(None) => {}
    }
}

/// Codex's reply `resp`, translated, to `tx`.
fn forward(resp: reqwest::blocking::Response, model: &str, tx: &Sender<Sse>) {
    let mut t = Translate::new(model);
    for ev in provider::events(resp) {
        let ev = match ev {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(e) => return drop(tx.send(Sse::Err(e))),
        };
        for o in t.event(&ev) {
            match o {
                Out::Event(v) => {
                    if tx.send(Sse::Event(v)).is_err() {
                        return;
                    }
                }
                Out::Done => return drop(tx.send(Sse::Done)),
                Out::Err(e) => return drop(tx.send(Sse::Err(e))),
            }
        }
    }
    let _ = tx.send(Sse::Err("Stream ended before the reply was complete".into()));
}

/// When a used-up plan can be used again (ms), from a 429's error and
/// headers.
fn limit_reset(err: &Value, headers: &[Limit]) -> i64 {
    let now = api::now_ms();
    if let Some(s) = err["resets_at"].as_i64() {
        return s * 1000;
    }
    if let Some(s) = err["resets_in_seconds"].as_i64() {
        return now + s * 1000;
    }
    let spent = headers.iter().filter(|l| l.percent >= 100.0).filter_map(|l| l.resets_at).max();
    spent.unwrap_or(now + 15 * 60_000)
}

/// The request for Codex's responses, as login `token` of `account`.
fn responses_request(req: &Value, session: &str, token: &str, account: &str) -> reqwest::blocking::RequestBuilder {
    api::http()
        .post(format!("{}/codex/responses", backend()))
        .bearer_auth(token)
        .header("chatgpt-account-id", account)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", ORIGINATOR)
        .header("session-id", session)
        .header("x-client-request-id", session)
        .header("Accept", "text/event-stream")
        .json(req)
}

/// Sends `req` until a response comes that can be read as a stream. Err
/// carries what the receiver gets, or None when it is gone.
fn open(req: &Value, session: &str, conv: &str, tx: &Sender<Sse>) -> Result<reqwest::blocking::Response, Option<Sse>> {
    let mut tries: u32 = 0;
    let mut renewed = false;
    let mut rotations = 0;
    let mut last: Option<String> = crate::rotation::seat(api::Provider::Codex, conv);
    loop {
        let id = login_for(conv)?;
        if last.as_ref().is_some_and(|l| *l != id) && tx.send(Sse::Rotated(label_of(&id))).is_err() {
            return Err(None);
        }
        last = Some(id.clone());
        let (token, account) = access(&id, false).map_err(|e| Some(Sse::Err(e)))?;
        let r = match api::send_fresh(|| responses_request(req, session, &token, &account)) {
            Ok(r) if r.status().is_success() => {
                note_limits(&id, limits_in(r.headers()));
                crate::rotation::used(api::Provider::Codex, conv, &id, crate::oak::Ttl::FiveMinutes.ms(), api::now_ms());
                if !provider::say_headers(tx, r.status().as_u16()) {
                    return Err(None);
                }
                return Ok(r);
            }
            Ok(r) => r,
            Err(e) => {
                tries += 1;
                if tries > api::RETRIES {
                    return Err(Some(Sse::Err(e)));
                }
                if !provider::wait_between(tx, tries, api::retry_wait(tries), Some(e)) {
                    return Err(None);
                }
                continue;
            }
        };
        let status = r.status().as_u16();
        let limits = limits_in(r.headers());
        let after = api::retry_after(r.headers());
        let text = r.text().unwrap_or_default();
        let err = serde_json::from_str::<Value>(&text).map(|v| v["error"].clone()).unwrap_or(Value::Null);
        let code = err["code"].as_str().or(err["type"].as_str()).unwrap_or("");
        note_limits(&id, limits.clone());
        if status == 401 && !renewed {
            renewed = true;
            access(&id, true).map_err(|e| Some(Sse::Err(e)))?;
            continue;
        }
        if status == 429 && code == "usage_limit_reached" {
            // Another login may have some left: the request goes again
            // through it.
            let reset = limit_reset(&err, &limits);
            if limited(&id, reset) && rotations < 10 {
                rotations += 1;
                continue;
            }
            return Err(Some(Sse::Limit(reset)));
        }
        if code == "usage_not_included" {
            return Err(Some(Sse::Err(format!("Your ChatGPT plan doesn't include Codex: {}", error_text(status, &text)))));
        }
        tries += 1;
        if provider::retryable(Some(status)) && tries <= api::RETRIES {
            if !provider::wait_between(tx, tries, provider::retry_in(after, tries), Some(format!("Codex {}", error_text(status, &text)))) {
                return Err(None);
            }
            continue;
        }
        return Err(Some(Sse::Err(format!("Codex {}", error_text(status, &text)))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &mut Translate, events: &[Value]) -> Vec<Out> {
        events.iter().flat_map(|e| t.event(e)).collect()
    }

    fn events(out: &[Out]) -> Vec<Event> {
        out.iter().filter_map(|o| if let Out::Event(v) = o { Some(v.clone()) } else { None }).collect()
    }

    fn sample() -> Request {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "hm"}], "encrypted_content": "enc"});
        Request {
            provider: crate::providers::Kind::Codex,
            model: "gpt-6.1-sol".into(),
            max_tokens: 128000,
            system: "You are Oak.".into(),
            thinking: true,
            effort: Some("xhigh".into()),
            conv: Some("c1".into()),
            tools: vec![json!({"name": "READ", "description": "Read a file", "input_schema": {"type": "object", "properties": {}}})],
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image", "media_type": "image/png", "data": "AAA"}]}),
                json!({"role": "assistant", "content": [
                    {"type": "reasoning", "text": "hm", "sig": reasoning.to_string(), "by": "codex"},
                    {"type": "reasoning", "text": "a Claude one", "sig": "EqQBCkYIBxgCKkB", "by": "claude"},
                    {"type": "text", "text": "Reading."},
                    {"type": "tool_call", "id": "call_1", "name": "READ", "input": {"path": "a"}},
                ]}),
                json!({"role": "user", "content": [
                    {"type": "tool_result", "call_id": "call_1", "content": [{"type": "text", "text": "file"}], "is_error": false},
                    {"type": "text", "text": "and?"},
                ]}),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn request_conversion() {
        let req = sample();
        let r = request_body(&req);
        assert_eq!(r["instructions"], "You are Oak.");
        assert_eq!(r["store"], false);
        assert_eq!(r["reasoning"]["effort"], "xhigh");
        assert!(r.get("max_tokens").is_none() && r.get("max_output_tokens").is_none());
        assert_eq!(r["tools"][0], json!({"type": "function", "name": "READ", "description": "Read a file", "parameters": {"type": "object", "properties": {}}, "strict": false}));
        assert_eq!(r["tool_choice"], "auto");
        let input = r["input"].as_array().unwrap();
        assert_eq!(input[0]["content"][0], json!({"type": "input_text", "text": "hi"}));
        assert_eq!(input[0]["content"][1]["image_url"], "data:image/png;base64,AAA");
        // The reasoning item as it came, less its id; Claude's thinking is dropped.
        assert_eq!(input[1], json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "hm"}], "encrypted_content": "enc"}));
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["content"][0]["text"], "Reading.");
        assert_eq!(input[3], json!({"type": "function_call", "call_id": "call_1", "name": "READ", "arguments": "{\"path\":\"a\"}"}));
        assert_eq!(input[4], json!({"type": "function_call_output", "call_id": "call_1", "output": "file"}));
        assert_eq!(input[5], json!({"role": "user", "content": [{"type": "input_text", "text": "and?"}]}));
        assert_eq!(input.len(), 6);
        // The same conversation, later: the same cache key.
        let mut later = req.clone();
        later.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}));
        assert_eq!(request_body(&later)["prompt_cache_key"], r["prompt_cache_key"]);
        // A title: no thinking, no tools.
        let title = request_body(&Request { model: "gpt-6-luna".into(), system: "Title it.".into(), messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "x"}]})], ..Default::default() });
        assert_eq!(title["reasoning"]["effort"], "low");
        assert_eq!(title["instructions"], "Title it.");
        assert!(title.get("tools").is_none() && title.get("tool_choice").is_none());
        // A summary with tools it may not call, its instruction last.
        let mut none = req.clone();
        none.tools_off = true;
        none.instruction = Some("Summarize.".into());
        let r = request_body(&none);
        assert_eq!(r["tool_choice"], "none");
        assert_eq!(r["input"].as_array().unwrap().last().unwrap()["content"][1]["text"], "Summarize.");
    }

    #[test]
    fn event_conversion() {
        let mut t = Translate::new("gpt-6.1-sol");
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Plan"}], "encrypted_content": "enc"});
        let out = run(
            &mut t,
            &[
                json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-6.1-sol"}}),
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
                json!({"type": "response.reasoning_summary_part.added", "output_index": 0}),
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Plan"}),
                json!({"type": "response.reasoning_summary_part.added", "output_index": 0}),
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "More"}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
                json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1"}}),
                json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Hi"}),
                json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "content": [{"type": "output_text", "text": "Hi"}]}}),
                json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "call_id": "call_9", "name": "READ"}}),
                json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "call_id": "call_9", "name": "READ", "arguments": "{\"path\":\"x\"}"}}),
                json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 800}, "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 30}}}}),
            ],
        );
        assert_eq!(out.last(), Some(&Out::Done));
        let ev = events(&out);
        assert_eq!(ev.len(), 15);
        assert_eq!(ev[0], Event::Start { id: "resp_1".into(), model: "gpt-6.1-sol".into() });
        assert_eq!(ev[1], Event::Block { index: 0, block: json!({"type": "reasoning", "text": "", "by": "codex"}) });
        assert_eq!(ev[3], Event::Reasoning { index: 0, text: "\n\n".into() });
        assert_eq!(ev[4], Event::Reasoning { index: 0, text: "More".into() });
        let Event::Sig { sig, .. } = &ev[5] else { panic!("{:?}", ev[5]) };
        assert_eq!(serde_json::from_str::<Value>(sig).unwrap(), json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Plan"}], "encrypted_content": "enc"}));
        assert_eq!(ev[6], Event::BlockEnd { index: 0 });
        assert_eq!(ev[8], Event::Text { index: 1, text: "Hi".into() });
        assert_eq!(ev[10], Event::Block { index: 2, block: json!({"type": "tool_call", "id": "call_9", "name": "READ", "input": {}}) });
        assert_eq!(ev[11], Event::Input { index: 2, json: "{\"path\":\"x\"}".into() });
        assert_eq!(ev[13], Event::Usage(oak::Usage { input: 200, cache_read: 800, output: 50, reasoning: Some(30), ..Default::default() }));
        assert_eq!(ev[14], Event::Stop { reason: "tool_calls".into(), detail: None });
    }
}
