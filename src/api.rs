// Claude: logins (a pasted OAuth code, or an API key) and the accounts
// kept in a vault (api/vault.rs, when a plugin brings one), rotation between accounts,
// their usage, the model list, and streaming POST /v1/messages. Also what
// every provider's requests share: the HTTP client, sending, retry waits
// and reading an SSE stream.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ops::ControlFlow;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::oak::Event;
use crate::time::parse_time;
pub use vault::{move_to_vault, sync_vault, take_accounts_changed, vault_changed, vault_name, vault_on};
pub(crate) use vault::{capital, signing_in_remotely, vault_add, vault_cannot_keep, vault_failed, vault_list, vault_remove, vault_token, vault_unreachable};
use vault::{add_remote, remote_token};

mod vault;
mod models;
pub mod resets;
pub use models::{Model, Provider, claude_efforts, claude_models_error, context_window, fable_offer, known_models, latest_models, max_output, model_name};

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// A subscription token is scoped to Claude Code, so the Messages gateway only
// serves requests the official client would have sent: its identity prompt,
// its betas, its billing header and its user agent. Without them a perfectly
// good account is answered with a bare HTTP 429, or with a demand for a newer
// client than the one we claim to be. These values were captured from the
// installed client (2.1.303); keep the version in step with the one
// Anthropic requires.
const CLAUDE_CODE_VERSION: &str = "2.1.303";
const CLAUDE_CODE_BUILD: &str = "2.1.303.914";
const CLAUDE_CODE_ENTRYPOINT: &str = "cli";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Exactly the betas the installed client puts on a Messages call: the Claude
/// Code tag, adaptive thinking, cache scoping and `output_config.effort`.
const CLAUDE_CODE_BETAS: &[&str] = &[
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "effort-2025-11-24",
];
/// The beta the client adds to a Messages call whose breakpoints ask for the
/// one-hour TTL (`cache_control`).
const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    Oauth { access: String, refresh: String, expires_at: i64 },
    ApiKey { key: String },
    /// An account kept in a vault (`vault.rs`): its id there, and the last
    /// access token it handed out (so OAK rides out an outage while it
    /// lasts). The refresh token never leaves the vault, which
    /// alone renews the login.
    Remote { id: String, access: String, expires_at: i64 },
}

/// A signed-in account: its credentials and what is known about it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Stored {
    pub id: u64,
    pub auth: Auth,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub plan: String,
    /// Refused for its usage limit until then (ms since 1970); 0 when not.
    #[serde(default)]
    pub limited_until: i64,
    /// Its plan limits as last fetched, and why fetching failed (not saved).
    #[serde(skip)]
    pub limits: Vec<Limit>,
    #[serde(skip)]
    pub error: Option<String>,
    #[serde(skip)]
    pub fetched: bool,
    /// When `limits` were fetched (ms since 1970); 0 before.
    #[serde(skip)]
    pub usage_at: i64,
    /// Fetching usage failed this many times in a row; it isn't tried
    /// again before `usage_retry_at` (ms since 1970).
    #[serde(skip)]
    pub usage_fails: u32,
    #[serde(skip)]
    pub usage_retry_at: i64,
    /// A vault account whose token couldn't be had (the vault is down, or refused) and whose cached one ran out: skipped
    /// until then (ms since 1970), so requests fall back to another.
    #[serde(skip)]
    pub unreachable_until: i64,
    /// The usage-limit resets Anthropic offers it (`resets`), as last read
    /// (None: it lists none), and when (ms since 1970; 0 before).
    #[serde(skip)]
    pub resets: Option<resets::Resets>,
    #[serde(skip)]
    pub resets_read: i64,
}

impl Stored {
    fn new(id: u64, auth: Auth) -> Stored {
        let none = String::new;
        Stored { id, auth, email: none(), name: none(), plan: none(), limited_until: 0, limits: vec![], error: None, fetched: false, usage_at: 0, usage_fails: 0, usage_retry_at: 0, unreachable_until: 0, resets: None, resets_read: 0 }
    }

    fn api_key(&self) -> bool {
        matches!(self.auth, Auth::ApiKey { .. })
    }

    /// Kept on the vault (not a login on this machine).
    fn remote(&self) -> bool {
        matches!(self.auth, Auth::Remote { .. })
    }

    /// Its id on the vault.
    fn remote_id(&self) -> Option<&str> {
        match &self.auth {
            Auth::Remote { id, .. } => Some(id),
            _ => None,
        }
    }

    /// How the transcript names it: the email, without "@gmail.com".
    pub fn label(&self) -> String {
        if self.api_key() {
            return "API key".into();
        }
        match (self.email.as_str(), self.name.as_str()) {
            ("", "") => format!("account {}", self.id + 1),
            ("", name) => name.to_string(),
            (email, _) => email.strip_suffix("@gmail.com").unwrap_or(email).to_string(),
        }
    }

    /// The 5-hour and weekly limits (the ones that stop every request).
    fn main_limits(&self) -> impl Iterator<Item = &Limit> {
        self.limits.iter().filter(|l| l.label == "5h" || l.label == "Week")
    }

    /// Until when it can't make requests (ms since 1970), or 0 when it can:
    /// it was refused for its limit, or its usage shows one used up.
    fn blocked_until(&self, now: i64) -> i64 {
        let used_up = self.main_limits().filter(|l| l.percent >= 100.0).filter_map(|l| l.resets_at).filter(|&at| at > now).max();
        let at = used_up.unwrap_or(0).max(self.limited_until).max(self.unreachable_until);
        if at > now { at } else { 0 }
    }

    /// How much usage it has left, in percent of its tightest limit. API
    /// keys pay per token, so they come after every subscription.
    fn headroom(&self, now: i64) -> f32 {
        if self.api_key() {
            return -1.0;
        }
        self.main_limits()
            .map(|l| if l.resets_at.is_some_and(|at| at <= now) { 100.0 } else { 100.0 - l.percent })
            .reduce(f32::min)
            .unwrap_or(0.0)
    }

    /// Close to its limit: the 5-hour one at 95%, the weekly one at 99%.
    fn near_limit(&self, now: i64) -> bool {
        self.main_limits()
            .any(|l| l.resets_at.is_none_or(|at| at > now) && l.percent >= if l.label == "5h" { SOON_5H } else { SOON_WEEK })
    }
}

/// "rotating soon" from this much of the 5-hour limit used...
const SOON_5H: f32 = 95.0;
/// ...or this much of the weekly one.
const SOON_WEEK: f32 = 99.0;

/// The account with the most usage left that can make requests, other than
/// `skip`. On a tie, the first after `skip` in the list. Logins on this
/// machine are only a fallback once the vault has accounts: they
/// are picked only when none of its accounts can make requests.
fn pick(list: &[Stored], skip: u64, now: i64) -> Option<u64> {
    let usable = |s: &Stored| s.id != skip && s.blocked_until(now) == 0;
    let remote_usable = list.iter().any(|s| s.remote() && usable(s));
    let start = list.iter().position(|s| s.id == skip).map_or(0, |i| i + 1);
    let best = (0..list.len())
        .map(|i| &list[(start + i) % list.len()])
        .filter(|s| usable(s) && (!remote_usable || s.remote()))
        .reduce(|b, s| if s.headroom(now) > b.headroom(now) { s } else { b });
    best.map(|s| s.id)
}

#[derive(Default, Serialize, Deserialize)]
struct Accounts {
    list: Vec<Stored>,
    /// The account requests go through.
    current: u64,
    next_id: u64,
}

impl Accounts {
    fn get(&self, id: u64) -> Option<&Stored> {
        self.list.iter().find(|s| s.id == id)
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Stored> {
        self.list.iter_mut().find(|s| s.id == id)
    }

    fn save(&self) {
        let _ = crate::store::write_private_json(&accounts_path(), self);
    }

    /// Lists a new account; its id.
    fn push(&mut self, auth: Auth) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Stored::new(id, auth));
        id
    }

}

static ACCOUNTS: Mutex<Accounts> = Mutex::new(Accounts { list: Vec::new(), current: 0, next_id: 0 });

/// Held while a token is renewed: refresh tokens rotate, so two renewals
/// of one at once would spend it twice.
static REFRESHING: Mutex<()> = Mutex::new(());

/// When the account last rotated, and how many times it has.
static ROTATED: Mutex<(Option<std::time::Instant>, u64)> = Mutex::new((None, 0));

/// Account rotation is on (a setting). Off, requests stay on the current
/// account, and when it is used up the session waits for it to reset.
static ROTATION: AtomicBool = AtomicBool::new(true);

/// Turns account rotation on or off. Turning it on wakes sessions waiting
/// on a usage limit, as a sign-in does, so they can go through another account.
pub fn set_rotation(on: bool) {
    let was = ROTATION.swap(on, Ordering::SeqCst);
    if on && !was {
        note_sign_in();
    }
}

pub fn rotation_on() -> bool {
    ROTATION.load(Ordering::SeqCst)
}

/// Brings the Codex logins and the OpenRouter, NodeClub and Freebuff keys
/// in step with the vault's (they are kept there too).
fn sync_other_logins() {
    crate::codex::sync();
    crate::openrouter::KEYS.sync();
    crate::nodeclub::KEYS.sync();
    crate::freebuff::sync();
}

/// Counts sign-ins (not token refreshes or sign-outs), so a session
/// waiting out the usage limits can tell another account came in.
static SIGN_INS: AtomicU64 = AtomicU64::new(0);

pub fn sign_ins() -> u64 {
    SIGN_INS.load(Ordering::SeqCst)
}

fn note_sign_in() {
    SIGN_INS.fetch_add(1, Ordering::SeqCst);
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn accounts_path() -> std::path::PathBuf {
    crate::store::dir().join("accounts.json")
}

/// Loads the signed-in accounts. A single sign-in from before accounts
/// (auth.json) becomes the first (tests sign in that way). False when there are none.
pub fn load_auth() -> bool {
    let path = accounts_path();
    let mut a: Accounts = crate::store::read_json(&path).unwrap_or_default();
    let old = crate::store::dir().join("auth.json");
    if !path.exists()
        && let Some(auth) = crate::store::read_json::<Auth>(&old)
    {
        a.list.push(Stored::new(0, auth));
        a.next_id = 1;
        a.save();
        let _ = std::fs::remove_file(old);
    }
    if a.get(a.current).is_none() {
        a.current = a.list.first().map_or(0, |s| s.id);
    }
    let openrouter = crate::openrouter::KEYS.load();
    let nodeclub = crate::nodeclub::KEYS.load();
    let codex = crate::codex::load();
    // Freebuff's token, and its balances in the background: it is a separate
    // service, so it does not count as being signed in to Oak.
    if crate::freebuff::load() {
        std::thread::spawn(crate::freebuff::refresh_session);
    }
    // The local server: its model and context again, if it is running.
    let local = crate::local::load();
    if local {
        std::thread::spawn(crate::local::refresh);
    }
    let ok = !a.list.is_empty() || openrouter || nodeclub || codex || local;
    *ACCOUNTS.lock().unwrap() = a;
    ok
}

/// Adds an account (or, signed in again, renews its credentials). The
/// first one becomes the current account; later ones join the rotation.
fn add_account(auth: Auth) {
    // Who it is, so signing in twice doesn't list it twice.
    let profile = match &auth {
        Auth::Oauth { .. } => profile_with(&auth).ok(),
        Auth::ApiKey { .. } | Auth::Remote { .. } => None,
    };
    let mut a = ACCOUNTS.lock().unwrap();
    let same = |s: &Stored| match (&s.auth, &auth) {
        (Auth::ApiKey { key: k1 }, Auth::ApiKey { key: k2 }) => k1 == k2,
        (Auth::Oauth { .. }, Auth::Oauth { .. }) => profile.as_ref().is_some_and(|p| !p.1.is_empty() && p.1 == s.email),
        _ => false,
    };
    let id = match a.list.iter().position(same) {
        Some(i) => {
            a.list[i].auth = auth;
            a.list[i].limited_until = 0;
            a.list[i].id
        }
        None => a.push(auth),
    };
    if let (Some((name, email, plan)), Some(s)) = (profile, a.get_mut(id)) {
        (s.name, s.email, s.plan) = (name, email, plan);
    }
    if a.get(a.current).is_none() || a.list.len() == 1 {
        a.current = id;
    }
    a.save();
    note_sign_in();
}

/// Signs an account out: one kept in the vault is removed there, for
/// every machine (when the vault can't be reached it stays, and
/// `vault_error` says why). False when none is left and no vault is
/// connected.
pub fn remove_account(id: u64) -> bool {
    let rid = ACCOUNTS.lock().unwrap().get(id).and_then(|s| s.remote_id().map(String::from));
    if let Some(rid) = rid
        && vault_remove("claude", &rid).is_err()
    {
        return true;
    }
    let mut a = ACCOUNTS.lock().unwrap();
    a.list.retain(|s| s.id != id);
    if a.get(a.current).is_none() {
        let now = now_ms();
        a.current = pick(&a.list, id, now).or(a.list.first().map(|s| s.id)).unwrap_or(0);
    }
    a.save();
    // `any_login` takes the lock again: held here, it would wait forever.
    drop(a);
    any_login()
}

/// Whether anything at all is signed in, so Home stays signed in: a Claude
/// account here or in a vault, or another provider's login.
/// Any login or key is on this machine, or a vault is connected
/// (`oak_sdk::Host::signed_in`).
pub fn signed_in() -> bool {
    crate::providers::Kind::ALL.iter().any(|p| p.on())
}

fn any_login() -> bool {
    !ACCOUNTS.lock().unwrap().list.is_empty() || vault_on() || crate::openrouter::on() || crate::nodeclub::on() || crate::codex::on()
}

/// Why there is no account to send a request through.
pub const NO_ACCOUNT: &str = "No Claude account: add one with + Add account on Home.";

/// The account requests go through.
fn current_id() -> Result<u64, String> {
    let a = ACCOUNTS.lock().unwrap();
    a.get(a.current).map(|s| s.id).ok_or_else(|| NO_ACCOUNT.to_string())
}

/// The accounts as rotation weighs them (`crate::rotation`): logins on
/// this machine are only a fallback once a vault has accounts.
fn candidates(list: &[Stored], now: i64) -> Vec<crate::rotation::Candidate> {
    let remote = list.iter().any(Stored::remote);
    list.iter()
        .map(|s| crate::rotation::Candidate {
            id: s.id.to_string(),
            left: s.headroom(now) as f64,
            usable: s.blocked_until(now) == 0,
            resets_at: s.main_limits().filter(|l| l.label == "Week").find_map(|l| l.resets_at).unwrap_or(i64::MAX),
            preferred: s.remote() || !remote,
        })
        .collect()
}

/// The account conversation `conv`'s next request goes through (its seat,
/// see `crate::rotation`; `conv` empty for a one-off). When none can make
/// requests, or with rotation off its own can't, Err says when the first
/// comes free.
fn account_for(conv: &str, now: i64) -> Result<u64, Fail> {
    let mut a = ACCOUNTS.lock().unwrap();
    if a.list.is_empty() {
        return Err(Fail::err(NO_ACCOUNT.into()));
    }
    let picked = crate::rotation::pick(Provider::Claude, conv, &candidates(&a.list, now), now, !rotation_on()).and_then(|id| id.parse::<u64>().ok());
    let first_free = |a: &Accounts| a.list.iter().map(|s| s.blocked_until(now)).filter(|&t| t > 0).min().unwrap_or(now + 60_000);
    match picked.and_then(|id| a.get(id)) {
        Some(s) if s.blocked_until(now) > 0 => Err(Fail::Report(Sse::Limit(s.blocked_until(now)))),
        Some(s) => {
            let id = s.id;
            // The account Home shows as in use: the last one a request went through.
            if a.current != id {
                a.current = id;
                a.save();
            }
            Ok(id)
        }
        None => Err(Fail::Report(Sse::Limit(first_free(&a)))),
    }
}

/// A request through account `id` was refused for its usage limit, which
/// resets at `at`: no request goes through it before then.
fn note_limited(id: u64, at: i64) {
    let mut a = ACCOUNTS.lock().unwrap();
    if let Some(s) = a.get_mut(id) {
        s.limited_until = s.limited_until.max(at);
        a.save();
    }
}

/// Account `id`'s name, as the transcript shows it.
fn label_of(id: u64) -> String {
    ACCOUNTS.lock().unwrap().get(id).map_or_else(|| format!("account {}", id + 1), Stored::label)
}

fn note_rotation() {
    let mut r = ROTATED.lock().unwrap();
    *r = (Some(std::time::Instant::now()), r.1 + 1);
}

/// How many times the account has rotated (to fetch usage again after).
pub fn rotations() -> u64 {
    ROTATED.lock().unwrap().1
}

/// The grey note on the current account's row: "rotated" for 30 seconds
/// after a rotation; near its limit, "rotating soon", or "cannot rotate"
/// when no other account has usage left; "rotation off" there with the
/// setting off and other accounts signed in.
pub fn rotation_status() -> Option<&'static str> {
    let rotated = ROTATED.lock().unwrap().0;
    let a = ACCOUNTS.lock().unwrap();
    status_for(&a.list, a.current, now_ms(), rotated.map(|t| t.elapsed()), rotation_on())
}

fn status_for(list: &[Stored], current: u64, now: i64, since_rotation: Option<Duration>, on: bool) -> Option<&'static str> {
    if on && since_rotation.is_some_and(|d| d < Duration::from_secs(30)) {
        return Some("rotated");
    }
    let cur = list.iter().find(|s| s.id == current)?;
    if !cur.near_limit(now) && cur.blocked_until(now) == 0 {
        return None;
    }
    if !on {
        return (list.len() > 1).then_some("rotation off");
    }
    Some(if pick(list, current, now).is_some() { "rotating soon" } else { "cannot rotate" })
}

/// One Claude Code process is one session, and it labels every request with a
/// UUID that stays put for the life of the run.
static SESSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(new_uuid);

/// A random (version 4) UUID.
pub(crate) fn new_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid_text(&b)
}

/// Bytes as lowercase hex digits.
pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 16 bytes written as a UUID is ("8-4-4-4-12" hex digits).
pub(crate) fn uuid_text(b: &[u8]) -> String {
    let hex = hex(&b[..b.len().min(16)]);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

pub(crate) fn http() -> reqwest::blocking::Client {
    // One client for everything: a clone shares its connection pool, so
    // requests reuse a kept-alive connection instead of paying a new
    // connection and TLS handshake each time (about 50 ms a request).
    // `OAK_SHARED_CLIENT=0` makes a new client per request, for comparison.
    static SHARED: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    if crate::bench::shared_client() {
        return SHARED.get_or_init(new_client).clone();
    }
    new_client()
}

fn new_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(None)
        .connect_timeout(Duration::from_secs(30))
        // Idle connections are dropped before the server or a NAT is likely
        // to close them; keepalive probes notice a dead one sooner.
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// A failure that a fresh connection is likely to fix: a kept-alive one the
/// server or the network dropped while it sat idle (reset or closed before
/// the response). Not a failure to connect, or a timeout.
pub(crate) fn stale_connection(e: &reqwest::Error) -> bool {
    crate::bench::shared_client() && !e.is_connect() && !e.is_timeout() && !e.is_status()
}

/// "Network error: …", for a request that got no response.
pub(crate) fn network_error(e: reqwest::Error) -> String {
    format!("Network error: {}", error_chain(&e))
}

/// Sends the request `build` makes, again right away, once, after a stale
/// kept-alive connection (the pool then hands out a new one).
pub(crate) fn send_fresh(build: impl Fn() -> reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response, String> {
    crate::provider::send(build).map_err(network_error)
}

/// The seconds a response's retry-after header asks to wait.
pub(crate) fn retry_after(h: &reqwest::header::HeaderMap) -> Option<u64> {
    h.get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse().ok())
}

/// A response's JSON, when it says it succeeded (else `error_text`).
pub(crate) fn json_of(resp: reqwest::blocking::Response) -> Result<Value, String> {
    if !resp.status().is_success() {
        return Err(error_text(resp));
    }
    resp.json().map_err(|e| e.to_string())
}

/// `json_of` with a provider's own wording of an error reply: `error`
/// takes its status and body.
pub(crate) fn json_or(resp: reqwest::blocking::Response, error: fn(u16, &str) -> String) -> Result<Value, String> {
    if !resp.status().is_success() {
        return Err(error(resp.status().as_u16(), &resp.text().unwrap_or_default()));
    }
    resp.json().map_err(|e| e.to_string())
}

/// An error with its causes ("error sending request: …: connection
/// reset"): reqwest's own message leaves out why.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let c_text = c.to_string();
        if !out.contains(&c_text) {
            out += &format!(": {c_text}");
        }
        cur = c.source();
    }
    out
}

fn random_string() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A new PKCE verifier, and its S256 challenge.
pub(crate) fn pkce() -> (String, String) {
    let verifier = random_string();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub struct Login {
    pub url: String,
    verifier: String,
    state: String,
}

pub fn start_login() -> Login {
    let ((verifier, challenge), state) = (pkce(), random_string());
    let url = reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URL),
            ("scope", SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ],
    )
    .unwrap()
    .to_string();
    Login { url, verifier, state }
}

/// Trades the "<code>#<state>" the sign-in page shows for tokens.
pub fn finish_login(login: &Login, pasted: &str) -> Result<(), String> {
    let pasted = pasted.trim();
    let (code, state) = pasted.split_once('#').unwrap_or((pasted, ""));
    if code.is_empty() {
        return Err("That is not a code.".into());
    }
    if !state.is_empty() && state != login.state {
        return Err("That code is from a different login. Start again.".into());
    }
    let tokens = token_request(json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": REDIRECT_URL,
        "client_id": CLIENT_ID,
        "code_verifier": login.verifier,
        "state": login.state,
    }))?;
    // With a vault that takes new logins, the login goes there and this
    // machine keeps only the access tokens it hands out.
    if signing_in_remotely() {
        return add_remote(tokens);
    }
    add_account(tokens);
    Ok(())
}

fn token_request(body: Value) -> Result<Auth, String> {
    let resp = http()
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| format!("Couldn't reach the login server: {e}"))?;
    let status = resp.status();
    let v: Value = resp.json().unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("The login server refused (HTTP {status}). Start the login again."));
    }
    let access = v["access_token"].as_str().unwrap_or_default().to_string();
    let refresh = v["refresh_token"].as_str().unwrap_or_default().to_string();
    if access.is_empty() {
        return Err("The login server sent no token.".into());
    }
    let expires_at = now_ms() + (v["expires_in"].as_f64().unwrap_or(3600.0) * 1000.0) as i64;
    Ok(Auth::Oauth { access, refresh, expires_at })
}

/// Account `id`'s credentials, renewing its OAuth access token when it is
/// close to expiring.
fn credentials(id: u64, force_refresh: bool) -> Result<Auth, String> {
    let _renewing = REFRESHING.lock().unwrap();
    let auth = ACCOUNTS.lock().unwrap().get(id).map(|s| s.auth.clone()).ok_or("Not signed in.")?;
    let now = now_ms();
    // Fresh enough, and not sent again after a refusal.
    let in_date = |access: &String, at: &i64| !force_refresh && !access.is_empty() && *at - now >= 5 * 60 * 1000;
    if let Auth::Remote { id: rid, access, expires_at } = &auth {
        if in_date(access, expires_at) {
            return Ok(auth);
        }
        // From the vault, which renews the login if need be; a
        // token Anthropic turned down goes back as refused.
        let fresh = remote_token(rid, force_refresh.then_some(access.as_str()));
        let mut a = ACCOUNTS.lock().unwrap();
        let Some(s) = a.get_mut(id) else { return Err("Not signed in.".into()) };
        return match fresh {
            Ok((access, expires_at)) => {
                s.auth = Auth::Remote { id: rid.clone(), access, expires_at };
                s.unreachable_until = 0;
                let auth = s.auth.clone();
                a.save();
                Ok(auth)
            }
            // Out of reach, but the token it last gave still works.
            Err(_) if !force_refresh && !access.is_empty() && *expires_at > now + 30_000 => Ok(auth),
            Err(e) => {
                s.unreachable_until = now + 60_000;
                Err(e)
            }
        };
    }
    if let Auth::Oauth { refresh, expires_at, .. } = &auth
        && (force_refresh || *expires_at - now < 5 * 60 * 1000)
    {
        let mut fresh = token_request(json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh,
            "scope": SCOPES.replace("org:create_api_key ", ""),
        }))?;
        if let Auth::Oauth { refresh: new_refresh, .. } = &mut fresh
            && new_refresh.is_empty()
        {
            *new_refresh = refresh.clone();
        }
        let mut a = ACCOUNTS.lock().unwrap();
        if let Some(s) = a.get_mut(id) {
            s.auth = fresh.clone();
        }
        a.save();
        return Ok(fresh);
    }
    Ok(auth)
}

fn request(
    method: reqwest::Method,
    path: &str,
    betas: &[&str],
    streaming: bool,
    auth: &Auth,
) -> reqwest::blocking::RequestBuilder {
    let mut list: Vec<&str> = Vec::new();
    let mut req = http()
        .request(method, format!("{}{path}", crate::bench::api_base()))
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION} (external, {CLAUDE_CODE_ENTRYPOINT})"))
        .header("x-app", "cli")
        .header("x-claude-code-session-id", SESSION.as_str())
        .header("anthropic-dangerous-direct-browser-access", "true");
    let subscription = match auth {
        Auth::Oauth { access, .. } | Auth::Remote { access, .. } => {
            req = req.header("Authorization", format!("Bearer {access}"));
            true
        }
        Auth::ApiKey { key } => {
            req = req.header("x-api-key", key);
            false
        }
    };
    if streaming {
        // Only a Messages call is a Claude Code request; the model list is not
        // one, so it gets no client betas that endpoint has never seen.
        list.extend_from_slice(CLAUDE_CODE_BETAS);
        // The system prompt is always cached for an hour (`claude_body`).
        list.push(EXTENDED_CACHE_TTL_BETA);
        list.extend_from_slice(betas);
        req = req.header("Accept", "application/json");
    }
    if subscription {
        list.push(OAUTH_BETA);
    }
    if !list.is_empty() {
        req = req.header("anthropic-beta", list.join(","));
    }
    req
}

/// Sends a request through account `id`, renewing its login once if the
/// server says it expired (a 401). `build` makes it each try, as the
/// renewed token changes.
fn send_as(
    id: u64,
    method: reqwest::Method,
    path: &str,
    betas: &[&str],
    streaming: bool,
    body: Option<&Value>,
) -> Result<reqwest::blocking::Response, String> {
    as_account(id, |auth| {
        send_fresh(|| {
            let req = request(method.clone(), path, betas, streaming, auth);
            if let Some(b) = body { req.json(b) } else { req }
        })
    })
}

/// What `send` gets with account `id`'s credentials, sent again with
/// renewed ones once when the server says a login expired (HTTP 401).
fn as_account(id: u64, send: impl Fn(&Auth) -> Result<reqwest::blocking::Response, String>) -> Result<reqwest::blocking::Response, String> {
    let auth = credentials(id, false)?;
    let resp = send(&auth)?;
    if resp.status() == 401 && matches!(auth, Auth::Oauth { .. } | Auth::Remote { .. }) {
        return send(&credentials(id, true)?);
    }
    Ok(resp)
}

/// The prompt cache's lifetime (a setting): 0 auto (per conversation and
/// request, `cache_ttl`), 1 five minutes, 2 an hour.
static CACHE_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_cache_mode(mode: &str) {
    CACHE_MODE.store(match mode { "5m" => 1, "1h" => 2, _ => 0 }, Ordering::Relaxed);
}

/// The lifetime the setting fixes, when it isn't auto.
pub fn cache_fixed() -> Option<crate::oak::Ttl> {
    match CACHE_MODE.load(Ordering::Relaxed) {
        1 => Some(crate::oak::Ttl::FiveMinutes),
        2 => Some(crate::oak::Ttl::OneHour),
        _ => None,
    }
}

/// The one-hour cache is the setting (for estimates that assume one).
pub fn cache_1h() -> bool {
    cache_fixed() == Some(crate::oak::Ttl::OneHour)
}

/// A prompt-cache breakpoint, as the client writes them: the default TTL,
/// or the one-hour one (with `EXTENDED_CACHE_TTL_BETA` on the request).
fn cache_control_for(one_hour: bool) -> Value {
    if one_hour { json!({"type": "ephemeral", "ttl": "1h"}) } else { json!({"type": "ephemeral"}) }
}

/// What a request gives the model besides the conversation, as its provider
/// sends it, for the lines at the top of the transcript: the billing header
/// and identity (only the Claude API gets them), the system prompt, and the
/// tool definitions in the provider's own form.
pub struct Preamble {
    pub identity: Option<[String; 2]>,
    pub system: String,
    pub tools: Vec<Value>,
}

/// The preamble of a request to `model` with OAK's `system` prompt and
/// `tools`, provider by provider in the order `stream` picks them.
pub fn preamble(model: &str, system: &str, tools: &Value) -> Preamble {
    let plain = |system: String, tools: Vec<Value>| Preamble { identity: None, system, tools };
    let oak_tools = || tools.as_array().cloned().unwrap_or_default();
    match Provider::of(model) {
        // OpenRouter is asked first: it also serves the session-title models.
        Provider::OpenRouter | Provider::Local => plain(system.into(), oak_tools()),
        Provider::Codex => plain(crate::codex::instructions(system), crate::codex::wire_tools(&oak_tools())),
        Provider::Freebuff => {
            let (system, tools) = crate::freebuff::preamble(model);
            plain(system, tools)
        }
        Provider::NodeClub => plain(system.into(), crate::nodeclub::chat_tools(&oak_tools())),
        // Claude: the billing header and identity go before the prompt.
        Provider::Claude => Preamble { identity: Some(system_preamble()), system: system.into(), tools: oak_tools() },
    }
}

/// What goes before the app's own system prompt in a request to Claude:
/// the billing header and the identity, in order.
pub fn system_preamble() -> [String; 2] {
    [billing_header(), CLAUDE_CODE_IDENTITY.to_string()]
}

fn billing_header() -> String {
    format!("x-anthropic-billing-header: cc_version={CLAUDE_CODE_BUILD}; cc_entrypoint={CLAUDE_CODE_ENTRYPOINT};")
}

/// An Oak request as a /v1/messages body, the way the Claude Code client
/// sends one.
///
/// The gateway checks the payload, not just the headers, and the official
/// identity has to be there, or the request is refused with a misleading 429
/// even when the account has usage left. The client's layout, in order, is:
/// a billing header block, the identity, then the agent's own prompt, with
/// cache breakpoints on the identity and the prompt and on the newest user
/// turn. Its tool list carries no breakpoint.
///
/// The system prompt's breakpoints are always the one-hour kind: the API
/// takes one-hour breakpoints only before five-minute ones, and the prompt
/// outlives a conversation's pauses either way. The conversation's own
/// breakpoint has the request's lifetime (`oak::Ttl`).
pub fn claude_body(req: &crate::oak::Request) -> Value {
    let mut body = crate::wire_anthropic::body(req, "claude", true);
    let tail = crate::wire_anthropic::cache_mark(req.ttl);
    let head = if tail.is_null() { Value::Null } else { cache_control_for(true) };
    let mut system = json!([
        {"type": "text", "text": billing_header()},
        {"type": "text", "text": CLAUDE_CODE_IDENTITY},
        {"type": "text", "text": req.system},
    ]);
    if !head.is_null() {
        system[1]["cache_control"] = head.clone();
        system[2]["cache_control"] = head;
    }
    body["system"] = system;
    // The client ships this with adaptive thinking: it never clears earlier
    // thinking out of the context.
    if req.thinking {
        body["context_management"] = json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]});
    }
    if tail.is_null() {
        return body;
    }
    // The last one-hour entry, kept warm under five-minute ones (one-hour
    // breakpoints come first, as the API requires): where that request's
    // conversation breakpoint was.
    // (Mid-conversation system messages, `crate::prefix`, come after the
    // user turn a breakpoint goes on.)
    let user_at = |msgs: &[Value], i: usize| (0..=i.min(msgs.len().saturating_sub(1))).rev().find(|&k| msgs[k]["role"] != "system");
    if req.ttl == crate::oak::Ttl::FiveMinutes
        && let Some(msgs) = body["messages"].as_array_mut()
        && let Some(i) = req.anchor.and_then(|i| user_at(msgs, i))
        && i + 1 < msgs.len()
        && msgs[i]["role"] == "user"
        && let Some(block) = msgs[i]["content"].as_array_mut().and_then(|c| c.last_mut())
        && matches!(block["type"].as_str(), Some("text") | Some("tool_result"))
    {
        block["cache_control"] = cache_control_for(true);
    }
    // The conversation breakpoint goes on the last block of the newest user
    // turn, which is either the question or the tool result just produced,
    // unless that turn has one already (a summary request keeps the one of
    // the request it copies).
    if let Some(msgs) = body["messages"].as_array_mut()
        && let Some(at) = user_at(msgs, msgs.len().saturating_sub(1))
        && let Some(last) = msgs.get_mut(at)
        && last["role"] == "user"
        && let Some(content) = last["content"].as_array_mut()
        && !content.iter().any(|b| b.get("cache_control").is_some())
        && let Some(block) = content.last_mut()
        && matches!(block["type"].as_str(), Some("text") | Some("tool_result"))
    {
        block["cache_control"] = tail;
    }
    body
}

pub(crate) fn error_text(resp: reqwest::blocking::Response) -> String {
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or(body);
    format!("HTTP {status}: {msg}")
}

/// One of the plan's usage limits.
#[derive(Clone, Debug)]
pub struct Limit {
    pub label: &'static str,
    pub percent: f32,
    pub resets_at: Option<i64>,
}

/// A signed-in account as the UI shows it: who, and how much of the plan's
/// limits is used.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub id: u64,
    /// Whose account it is (Claude, or Codex: `crate::codex`).
    pub provider: Provider,
    pub name: String,
    pub email: String,
    pub plan: String,
    pub limits: Vec<Limit>,
    /// Signed in with an API key: pay as you go, no plan limits.
    pub api_key: bool,
    /// Requests go through it.
    pub current: bool,
    /// Its usage has been fetched (or failed to be: `error`).
    pub fetched: bool,
    /// Why the last fetch failed. With `limits` from an earlier one, those
    /// still show, as of `usage_at` (ms since 1970).
    pub error: Option<String>,
    pub usage_at: i64,
    /// A login on this machine while a vault is connected: used
    /// only when none of the vault's accounts can be.
    pub fallback: bool,
    /// A Claude login on this machine (not an API key), which can move to
    /// the vault.
    pub movable: bool,
    /// The usage-limit resets Anthropic offers it, as last read (None: it
    /// lists none): only the person uses one (`crate::resets_view`).
    pub resets: Option<resets::Resets>,
}


/// The signed-in accounts, in order.
pub fn accounts() -> Vec<Account> {
    let paired = vault_on();
    let a = ACCOUNTS.lock().unwrap();
    a.list
        .iter()
        .map(|s| Account {
            id: s.id,
            provider: Provider::Claude,
            name: s.name.clone(),
            email: s.email.clone(),
            plan: s.plan.clone(),
            limits: s.limits.clone(),
            api_key: s.api_key(),
            current: s.id == a.current,
            fetched: s.fetched,
            error: s.error.clone(),
            usage_at: s.usage_at,
            fallback: paired && !s.remote(),
            movable: matches!(s.auth, Auth::Oauth { .. }),
            resets: s.resets.clone(),
        })
        .collect()
}


/// The name, email and plan in an OAuth profile.
fn read_profile(p: &Value) -> (String, String, String) {
    let a = &p["account"];
    // Max comes in two sizes, told apart by the rate-limit tier
    // ("default_claude_max_20x"), as Claude Code reads it.
    let tier = p["organization"]["rate_limit_tier"].as_str().unwrap_or("");
    let org = p["organization"]["organization_type"].as_str().unwrap_or("");
    let plan = match (a["has_claude_max"] == true, a["has_claude_pro"] == true) {
        _ if org == "claude_team" => team_seat(p["organization"]["seat_tier"].as_str().unwrap_or(""), tier),
        (true, _) if tier.ends_with("20x") => "Max 20x",
        (true, _) if tier.ends_with("5x") => "Max 5x",
        (true, _) => "Max",
        (_, true) => "Pro",
        _ => p["organization"]["organization_type"].as_str().unwrap_or(""),
    };
    (
        a["display_name"].as_str().or(a["full_name"].as_str()).unwrap_or("").to_string(),
        a["email"].as_str().unwrap_or("").to_string(),
        plan.to_string(),
    )
}

/// A Team seat's kind, from the profile's `seat_tier` ("team_tier_1" on a
/// Premium seat) or, without one, its rate-limit tier (a Premium seat's is
/// "default_claude_max_5x", a Standard one's "default_raven").
pub(crate) fn team_seat(seat: &str, tier: &str) -> &'static str {
    let seat = seat.to_ascii_lowercase();
    match () {
        _ if seat.contains("premium") || seat == "team_tier_1" => "Team Premium",
        _ if seat.contains("standard") => "Team Standard",
        _ if seat.is_empty() && tier.contains("claude_max") => "Team Premium",
        _ => "Team Standard",
    }
}

/// The profile of credentials that aren't stored yet (a fresh sign-in).
fn profile_with(auth: &Auth) -> Result<(String, String, String), String> {
    let resp = request(reqwest::Method::GET, "/api/oauth/profile", &[], false, auth).send().map_err(network_error)?;
    Ok(read_profile(&json_of(resp)?))
}

/// An account's plan limits and the usage-limit resets it is offered.
type UsageReport = (Vec<Limit>, Option<resets::Resets>);

/// The plan limits, as /api/oauth/usage reports them (the client's /usage),
/// and the usage-limit resets on offer (`resets::USAGE_PATH`). Err carries
/// the retry-after a refusal came with (seconds), if any.
fn usage_report(id: u64) -> Result<UsageReport, (String, Option<u64>)> {
    let mut resp = send_as(id, reqwest::Method::GET, resets::USAGE_PATH, &[], false, None).map_err(|e| (e, None))?;
    if resp.status() == 400 {
        // Should the report ever refuse the resets' query, the limits
        // still come, without them.
        resp = send_as(id, reqwest::Method::GET, "/api/oauth/usage", &[], false, None).map_err(|e| (e, None))?;
    }
    if !resp.status().is_success() {
        let after = retry_after(resp.headers());
        return Err((error_text(resp), after));
    }
    let u: Value = resp.json().map_err(|e| (e.to_string(), None))?;
    Ok((read_limits(&u), resets::read_resets(&u)))
}

fn read_limits(u: &Value) -> Vec<Limit> {
    let mut out = vec![];
    for (key, label) in [("five_hour", "5h"), ("seven_day", "Week"), ("seven_day_opus", "Opus wk"), ("seven_day_sonnet", "Sonnet wk")] {
        let l = &u[key];
        if let Some(p) = l["utilization"].as_f64() {
            out.push(Limit { label, percent: p as f32, resets_at: l["resets_at"].as_str().and_then(parse_time) });
        }
    }
    out
}

/// The 5-hour and weekly limits in a reply's headers (utilization 0 to 1,
/// reset in seconds since 1970): every reply through a Claude account
/// carries them, as Claude Code's own usage display reads them.
fn limits_in(h: &reqwest::header::HeaderMap) -> Vec<Limit> {
    let get = |k: &str| h.get(format!("anthropic-ratelimit-unified-{k}")).and_then(|v| v.to_str().ok()?.trim().parse::<f64>().ok());
    [("5h", "5h"), ("7d", "Week")]
        .into_iter()
        .filter_map(|(key, label)| {
            let used = get(&format!("{key}-utilization"))?;
            let resets_at = get(&format!("{key}-reset")).map(|s| s as i64 * 1000);
            Some(Limit { label, percent: (used * 100.0) as f32, resets_at })
        })
        .collect()
}

/// Takes account `id`'s usage from a reply's headers: fresh at no cost, so
/// the account in use needs no polling of the usage endpoint. Limits the
/// headers lack (the per-model weeks) stay as last polled.
fn note_usage(id: u64, h: &reqwest::header::HeaderMap) {
    let new = limits_in(h);
    if new.is_empty() {
        return;
    }
    let mut a = ACCOUNTS.lock().unwrap();
    let Some(s) = a.get_mut(id) else { return };
    let mut limits = new;
    limits.extend(s.limits.iter().filter(|l| l.label != "5h" && l.label != "Week").cloned());
    limits.sort_by_key(|l| ["5h", "Week", "Opus wk", "Sonnet wk"].iter().position(|x| *x == l.label));
    (s.limits, s.error, s.usage_at, s.usage_fails, s.usage_retry_at, s.fetched) = (limits, None, now_ms(), 0, 0, true);
}

/// Usage this fresh (ms) isn't fetched again: it came with a reply.
const USAGE_FRESH: i64 = 100_000;

/// How long the resets an account was offered are taken as current: the
/// account in use, whose usage comes with each reply, has its usage
/// report read this often for them.
const RESETS_FRESH: i64 = 15 * 60_000;

/// How long to leave an account's usage alone after `fails` failures in a
/// row (ms): what a refusal's retry-after asks, or else 2, 4, 8... minutes,
/// at most half an hour. The usage endpoint answers few requests per
/// account, and every OAK on the vault shares its accounts.
fn usage_backoff(fails: u32, retry_after: Option<u64>) -> i64 {
    let doubling = 60_000i64 << fails.clamp(1, 5);
    // (The usage endpoint's 429s say "retry-after: 0", which is no help.)
    let ms = match retry_after.filter(|&s| s > 0) {
        Some(s) => (s as i64 * 1000).max(30_000),
        None => doubling,
    };
    ms.min(1_800_000)
}

/// Fetches account `id`'s usage, and its profile the first time (an API
/// key has neither), and keeps them for `accounts()` and for picking where
/// to rotate. After a failure (often HTTP 429: the usage endpoint answers
/// few requests) the account is left alone for a while (`usage_backoff`),
/// and the usage fetched before keeps showing, marked as of then.
pub fn refresh_account(id: u64) {
    let (api_key, profile_known, wait) = {
        let a = ACCOUNTS.lock().unwrap();
        let Some(s) = a.get(id) else { return };
        let fresh = s.usage_retry_at > now_ms() || (now_ms() - s.usage_at < USAGE_FRESH && now_ms() - s.resets_read < RESETS_FRESH);
        (s.api_key(), !s.email.is_empty() && !matches!(s.plan.as_str(), "Max" | "claude_team"), fresh)
    };
    if api_key {
        if let Some(s) = ACCOUNTS.lock().unwrap().get_mut(id) {
            (s.fetched, s.error) = (true, None);
        }
        return;
    }
    if wait {
        return;
    }
    // A Max plan saved before OAK told 5x from 20x (or a Team plan before
    // it told Standard seats from Premium ones): its profile is read again,
    // once a run.
    static RECHECKED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let profile_known = profile_known || {
        let mut r = RECHECKED.lock().unwrap();
        r.contains(&id) || {
            r.push(id);
            false
        }
    };
    let profile = if profile_known { Ok(None) } else { send_as(id, reqwest::Method::GET, "/api/oauth/profile", &[], false, None).and_then(json_of).map(|p| Some(read_profile(&p))) };
    let limits = match &profile {
        Ok(_) => usage_report(id),
        Err(e) => Err((e.clone(), None)),
    };
    let mut a = ACCOUNTS.lock().unwrap();
    let Some(s) = a.get_mut(id) else { return };
    s.fetched = true;
    let mut changed = false;
    if let Ok(Some((name, email, plan))) = profile {
        changed = (&s.name, &s.email, &s.plan) != (&name, &email, &plan);
        (s.name, s.email, s.plan) = (name, email, plan);
    }
    let polled = limits.is_ok();
    match limits {
        Ok((limits, resets)) => {
            (s.limits, s.error, s.usage_at, s.usage_fails, s.usage_retry_at) = (limits, None, now_ms(), 0, 0);
            (s.resets, s.resets_read) = (resets, now_ms());
        }
        Err((e, after)) => {
            s.usage_fails += 1;
            s.usage_retry_at = now_ms() + usage_backoff(s.usage_fails, after);
            s.error = Some(e);
        }
    }
    if changed {
        a.save();
    }
    drop(a);
    if polled {
        note_polled(id);
    }
}

/// Fetches the vault's account list (accounts added or removed by another
/// machine), then every account's profile and usage, and the
/// OpenRouter credits left.
pub fn refresh_accounts() {
    if vault_on() {
        let _ = sync_vault();
        sync_other_logins();
    }
    crate::openrouter::refresh();
    crate::nodeclub::refresh();
    crate::codex::refresh();
    let ids: Vec<u64> = ACCOUNTS.lock().unwrap().list.iter().map(|s| s.id).collect();
    for id in ids {
        refresh_account(id);
    }
}

// Cloud sessions (cloud.rs) run on the Claude logins (this machine's, and
// the vault's when there is one) and are reached with their access tokens,
// straight from here.

pub use oak_sdk::cloud::Account as CloudAccount;

/// How a login on this machine is named as a cloud account: "local-" and
/// its id here (a vault's account is named by its id there).
const LOCAL_RID: &str = "local-";

/// The cloud account id of `s`, if sessions can run on it (a Claude
/// sign-in; not an API key).
fn cloud_rid(s: &Stored) -> Option<String> {
    match &s.auth {
        Auth::Remote { id, .. } => Some(id.clone()),
        Auth::Oauth { .. } => Some(format!("{LOCAL_RID}{}", s.id)),
        Auth::ApiKey { .. } => None,
    }
}

/// The accounts cloud sessions run on, the vault's first (every machine
/// connected to it sees their sessions), then this machine's own sign-ins.
pub fn cloud_accounts() -> Vec<CloudAccount> {
    let now = now_ms();
    let a = ACCOUNTS.lock().unwrap();
    let of = |s: &Stored| cloud_rid(s).map(|rid| CloudAccount { rid, label: s.label(), blocked_until: s.blocked_until(now) });
    let (remote, local): (Vec<&Stored>, Vec<&Stored>) = a.list.iter().partition(|s| s.remote());
    remote.into_iter().chain(local).filter_map(of).collect()
}

/// Notes that account `rid` hit its usage limit until `until` (from a cloud
/// session), so it is skipped for new sessions meanwhile.
pub fn note_cloud_limit(rid: &str, until: i64) {
    let mut a = ACCOUNTS.lock().unwrap();
    if let Some(s) = a.list.iter_mut().find(|s| cloud_rid(s).as_deref() == Some(rid))
        && until > s.limited_until
    {
        s.limited_until = until;
        a.save();
    }
}

/// The cloud API, as Claude Code calls it.
const CLOUD_API: &str = "https://api.anthropic.com";
const CLOUD_USER_AGENT: &str = "claude-code/2.1.263";

/// A request to the cloud sessions API through account `rid`: an event
/// stream's Accept, the session beta and the organization when given, and
/// a 401 renewing the login once. `timeout` bounds the whole request.
pub fn cloud_request(
    rid: &str,
    method: reqwest::Method,
    path: &str,
    beta: Option<&str>,
    org: Option<&str>,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<reqwest::blocking::Response, String> {
    let id = ACCOUNTS
        .lock()
        .unwrap()
        .list
        .iter()
        .find(|s| cloud_rid(s).as_deref() == Some(rid))
        .map(|s| s.id)
        .ok_or("That account is no longer signed in.")?;
    as_account(id, |auth| {
        let (Auth::Remote { access, .. } | Auth::Oauth { access, .. }) = auth else { return Err("Cloud sessions need a Claude sign-in, not an API key.".into()) };
        let req = http()
            .request(method.clone(), format!("{CLOUD_API}{path}"))
            .bearer_auth(access)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-client-platform", "claude_code_cli")
            .header("user-agent", CLOUD_USER_AGENT)
            .timeout(timeout);
        cloud_headers(req, path, beta, org, body).send().map_err(network_error)
    })
}

/// What every cloud sessions request carries after its login (Claude's
/// or a plugin backend's): what it accepts, the beta and organization when given,
/// and its JSON body.
pub(crate) fn cloud_headers(
    mut req: reqwest::blocking::RequestBuilder,
    path: &str,
    beta: Option<&str>,
    org: Option<&str>,
    body: Option<&Value>,
) -> reqwest::blocking::RequestBuilder {
    req = req.header("Accept", if path.contains("/events/stream") { "text/event-stream" } else { "application/json" });
    if let Some(b) = beta {
        req = req.header("anthropic-beta", b);
    }
    if let Some(o) = org {
        req = req.header("x-organization-uuid", o);
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    req
}

pub enum Sse {
    Event(Event),
    Done,
    Err(String),
    /// Every account's usage limit was reached; the first resets then (ms
    /// since 1970).
    Limit(i64),
    /// The account hit its limit and requests now go through this one
    /// (named for the transcript); the request was sent again.
    Rotated(String),
}

/// When a rejected request hit the plan's usage limit (not an ordinary
/// rate limit, which is retried): the time it resets, in ms since 1970.
/// Waits at least 30 s, so a reset time already past isn't hammered.
pub fn usage_limit_reset(status: u16, headers: &reqwest::header::HeaderMap, now: i64) -> Option<i64> {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    if status != 429 || h("anthropic-ratelimit-unified-status") != Some("rejected") {
        return None;
    }
    let at = match h("anthropic-ratelimit-unified-reset").and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(secs) => secs * 1000,
        None => now + h("retry-after").and_then(|v| v.trim().parse::<i64>().ok()).map_or(300_000, |s| s * 1000),
    };
    Some(at.max(now + 30_000))
}

/// How many times a failed request is retried quickly (`retry_wait`).
pub const RETRIES: u32 = 4;

/// Seconds to wait before retry number `n` (from 1): 4, 8, 16 and 32 for
/// the quick retries, then every minute and a half for as long as it takes
/// (refusals, which are retried until the user interrupts).
pub fn retry_wait(n: u32) -> u64 {
    if n <= RETRIES { 2 << n } else { 90 }
}

/// Streams one request through its provider, sending its events to `tx`.
/// Returns early (dropping the connection) once the receiver is gone.
/// No server-side model fallback: a refusal comes back as one, and the
/// agent retries it on the model the user chose.
pub fn stream_messages(req: crate::oak::Request, tx: Sender<Sse>) {
    stream(req, None, false, tx)
}

/// `stream_messages` in a server-side thread (the `threads` setting, Claude
/// only): with `cont`, only the messages after the server's copy go, as a
/// continuation; without it, or when the server refuses one, the whole
/// body goes and starts a new thread.
pub fn stream_thread(req: crate::oak::Request, cont: Option<(String, usize)>, tx: Sender<Sse>) {
    stream(req, cont, true, tx)
}

/// How opening a stream ended, short of a response to read.
pub(crate) enum Fail {
    /// The receiver is gone: nothing to tell.
    Gone,
    /// What the receiver gets.
    Report(Sse),
    /// The server refused a thread continuation (the message).
    Refused(String),
}

impl Fail {
    /// The error the receiver gets.
    pub(crate) fn err(e: String) -> Fail {
        Fail::Report(Sse::Err(e))
    }

    /// Tells the receiver (a refused continuation as an error).
    pub(crate) fn tell(self, tx: &Sender<Sse>) {
        match self {
            Fail::Report(s) => drop(tx.send(s)),
            Fail::Refused(e) => drop(tx.send(Sse::Err(e))),
            Fail::Gone => {}
        }
    }
}

fn stream(req: crate::oak::Request, cont: Option<(String, usize)>, thread: bool, tx: Sender<Sse>) {
    // Any other provider's request goes to that provider, whole (none has
    // server threads).
    match req.provider {
        Provider::OpenRouter => return crate::openrouter::stream(req, tx),
        Provider::Codex => return crate::codex::stream(req, tx),
        Provider::Freebuff => return crate::freebuff::stream(req, tx),
        Provider::NodeClub => return crate::nodeclub::stream(req, tx),
        Provider::Local => return crate::local::stream(req, tx),
        Provider::Claude => {}
    }
    let kind = req.purpose.id().to_string();
    let one_hour = req.ttl == crate::oak::Ttl::OneHour;
    let mut body = claude_body(&req);
    let seat = Seat::of(&req);
    // The same betas for every request of a conversation, threaded or not
    // (a summary, which goes whole): betas may be part of what the cache
    // matches.
    let mut betas: Vec<&str> = if thread || threads() { vec![THREADS_BETA] } else { vec![] };
    betas.extend(crate::prefix::betas(&body));
    // Cache diagnostics: compared with the conversation's last main reply.
    let conv = req.conv.clone().unwrap_or_default();
    let diag = diagnostics() && !conv.is_empty() && matches!(req.purpose, crate::oak::Purpose::Main | crate::oak::Purpose::Summary);
    if diag {
        body["diagnostics"] = json!({"previous_message_id": last_reply(&conv)});
        // Only reports what happened to earlier thinking (`input_transformations`).
        betas.push(THINKING_BINDING_BETA);
    }
    let mut thr = "";
    // A continuation the server refuses starts a new thread instead.
    let resp = match cont {
        Some((id, held)) => match open_extras(&continuation(&body, &id, held), &betas, true, &seat, &tx) {
            Ok(r) => {
                thr = "cont";
                Some(r)
            }
            Err(Fail::Refused(reason)) => {
                if tx.send(Sse::Event(Event::ThreadRestart(reason))).is_err() {
                    return;
                }
                None
            }
            Err(f) => return f.tell(&tx),
        },
        None => None,
    };
    let (resp, id) = match resp {
        Some(r) => r,
        None => {
            if thread {
                body["thread"] = json!({"type": "create"});
                thr = "new";
            }
            match open_extras(&body, &betas, false, &seat, &tx) {
                Ok(r) => r,
                Err(f) => return f.tell(&tx),
            }
        }
    };
    // For the Usage page: the reply's tokens, and the account's
    // utilization before it.
    let util = crate::usage::Util::of(&limits_in(resp.headers()));
    let mut meter = crate::usage::Meter::default();
    let (mut reply_id, mut verdict, mut moved) = (String::new(), None, Value::Null);
    relay(
        resp,
        &tx,
        "claude",
        |ev| {
            if ev["type"] == "message_start" {
                verdict = ev["message"].get("diagnostics").cloned();
                moved = ev["message"]["input_transformations"].clone();
            }
            vec![]
        },
        |ev| {
            if let Event::Start { id, .. } = ev {
                reply_id = id.clone();
            }
            meter.see(ev)
        },
    );
    // Only a main reply is what the next request should match: a summary
    // adds its instruction where the next request has the reply.
    if diag && req.purpose == crate::oak::Purpose::Main && !reply_id.is_empty() {
        LAST_REPLY.lock().unwrap().insert(conv.clone(), reply_id);
    }
    if meter.seen {
        let (acct, plan) = usage_name(id);
        let (miss, missed) = match &verdict {
            Some(v) if diag => miss_reason(v),
            None if diag => ("absent".into(), 0),
            _ => (String::new(), 0),
        };
        let count = |t: &str| moved.as_array().map_or(0, |a| a.iter().filter(|x| x["type"] == t).count() as u64);
        let info = crate::usage::Info { conv, miss, missed, thr: thr.into(), think_dropped: count("thinking_dropped"), think_mismatched: count("thinking_mismatch_allowed") };
        crate::usage::record_claude(acct, plan, kind, &meter, util, one_hour, info);
    }
}

// Cache diagnostics (a setting, on by default): each request of a
// conversation names its last main reply (`diagnostics.previous_message_id`)
// and the API says why the request missed the cache, if it changed
// something the previous one had (`cache_miss_reason`), for the usage log.
// Turned off for the session if the API refuses the field.
switch!(DIAGNOSTICS, diagnostics, set_diagnostics, true);

// The stable prefix (`crate::prefix`; a setting, on by default): turned
// off for the session if the API refuses its system messages.
switch!(STABLE_PREFIX, stable_prefix, set_stable_prefix, true);

/// With cache diagnostics: replies list what the API did with earlier
/// thinking blocks that no longer match what came before them (Oak's
/// summaries, clearing, a new system prompt) or came from another account.
/// The header alone changes nothing the model gets.
const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Betas `open_extras` leaves out when the API refuses a request with them.
fn experimental(b: &str) -> bool {
    [crate::prefix::INLINE_TOOLS_BETA, crate::prefix::EFFORT_BETA, THINKING_BINDING_BETA].contains(&b)
}

// Keep-alive (a setting, on by default): see `keep_warm`.
switch!(KEEP_ALIVE, keep_alive, set_keep_alive, true);

/// `oak --measure-thinking --probe`: whether the token counter counts an
/// earlier turn's thinking at all. One real reply with thinking, then the
/// two-turn conversation counted with and without its thinking block.
pub fn probe_thinking(model: &str, tool: bool) -> Result<String, String> {
    let user = |t: &str| json!({"role": "user", "content": [{"type": "text", "text": t}]});
    let mut req = crate::oak::Request {
        model: model.into(),
        max_tokens: 8000,
        system: "Answer briefly.".into(),
        messages: vec![user(if tool { "Is 1000003 prime? Check carefully, then call the record tool with your answer." } else { "Is 1000003 prime? Check carefully before answering." })],
        tools: if tool { vec![json!({"name": "record", "description": "Records an answer.", "input_schema": {"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]}})] } else { vec![] },
        thinking: true,
        effort: Some("high".into()),
        conv: Some("probe-thinking".into()),
        ..Default::default()
    };
    let body = unstreamed(claude_body(&req));
    let id = account_for("probe-thinking", now_ms()).map_err(|_| NO_ACCOUNT.to_string())?;
    let r = send_as(id, reqwest::Method::POST, "/v1/messages", &[], true, Some(&body))?;
    if !r.status().is_success() {
        return Err(error_text(r));
    }
    let v: Value = r.json().map_err(|e| e.to_string())?;
    let out = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
    let content = v["content"].as_array().cloned().unwrap_or_default();
    let call = content.iter().find(|b| b["type"] == "tool_use").map(|b| b["id"].clone());
    let next = match &call {
        Some(id) => json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "Recorded."}]}),
        None => user("Thanks."),
    };
    let shown: usize = content.iter().filter(|b| b["type"] == "thinking").map(|b| b["thinking"].as_str().map_or(0, str::len)).sum();
    let then = |req: &mut crate::oak::Request, content: Vec<Value>| {
        req.messages.truncate(1);
        req.messages.extend(crate::oak::from_anthropic(&[json!({"role": "assistant", "content": content}), next.clone()], "claude"));
    };
    let mut count = |content: Vec<Value>| -> Result<u64, String> {
        then(&mut req, content);
        let b = claude_body(&req);
        let mut c = serde_json::Map::new();
        for k in ["model", "system", "messages", "thinking", "tools"] {
            if let Some(x) = b.get(k) {
                c.insert(k.into(), x.clone());
            }
        }
        let r = send_as(id, reqwest::Method::POST, "/v1/messages/count_tokens", &[], true, Some(&Value::Object(c)))?;
        if !r.status().is_success() {
            return Err(error_text(r));
        }
        let v: Value = r.json().map_err(|e| e.to_string())?;
        v["input_tokens"].as_u64().ok_or_else(|| format!("{v}"))
    };
    // (Oak's transcript keeps blocks as the API sent them; here they go back raw.)
    let with = count(content.clone())?;
    let without = count(content.iter().filter(|b| b["type"] != "thinking").cloned().collect())?;

    // And billed: the same two as real requests that generate nothing.
    let system = req.system.clone();
    let mut billed = |content: Vec<Value>, edited: bool| -> Result<u64, String> {
        req.system = if edited { format!("{system} Edited.") } else { system.clone() };
        then(&mut req, content);
        let b = generating_nothing(&req);
        let r = send_as(id, reqwest::Method::POST, "/v1/messages", &[], true, Some(&b))?;
        if !r.status().is_success() {
            return Err(error_text(r));
        }
        let v: Value = r.json().map_err(|e| e.to_string())?;
        let u = crate::wire_anthropic::usage(&v["usage"]);
        Ok(u.input + u.cache_read + u.write)
    };
    let bare: Vec<Value> = content.iter().filter(|b| b["type"] != "thinking").cloned().collect();
    let b_with = billed(content.clone(), false)?;
    let b_without = billed(bare.clone(), false)?;
    // With the system prompt edited since: what the prefix check does with
    // a block that no longer matches what came before it.
    let b_edited = billed(content.clone(), true)?;
    let b_edited_without = billed(bare, true)?;
    Ok(format!(
        "reply: {out} output tokens, {shown} chars of thinking shown\ncount_tokens: with its thinking {with}, without {without} (+{})\nbilled prompt: with {b_with}, without {b_without} (+{})\nsystem prompt edited, billed: with {b_edited}, without {b_edited_without} (+{})",
        with as i64 - without as i64,
        b_with as i64 - b_without as i64,
        b_edited as i64 - b_edited_without as i64
    ))
}

/// Keep-alive: `req` (a conversation's last main request, as it went) sent
/// again with `max_tokens: 0` and no stream, through the account that
/// holds its cache: the API reads the cache (0.05× input, Opus 5.5's
/// price) and generates nothing, and reading a five-minute entry restarts
/// its lifetime. Recorded as "keepalive". Returns the reply's usage.
pub fn keep_warm(req: &crate::oak::Request) -> Result<crate::oak::Usage, String> {
    let body = generating_nothing(req);
    // The betas the conversation's requests go with (`stream`).
    let mut betas: Vec<&str> = if threads() { vec![THREADS_BETA] } else { vec![] };
    betas.extend(crate::prefix::betas(&body));
    if diagnostics() {
        betas.push(THINKING_BINDING_BETA);
    }
    let conv = req.conv.clone().unwrap_or_default();
    // Only the conversation's account has its cache: no rotation.
    let id = crate::rotation::seat(Provider::Claude, &conv).and_then(|id| id.parse::<u64>().ok()).ok_or("no account holds the conversation")?;
    let r = send_as(id, reqwest::Method::POST, "/v1/messages", &betas, true, Some(&body))?;
    note_usage(id, r.headers());
    if !r.status().is_success() {
        return Err(error_text(r));
    }
    let util = crate::usage::Util::of(&limits_in(r.headers()));
    let v: Value = r.json().map_err(|e| format!("Bad keep-alive reply: {e}"))?;
    let u = crate::wire_anthropic::usage(&v["usage"]);
    let mut meter = crate::usage::Meter::default();
    meter.see(&crate::oak::Event::Start { id: String::new(), model: v["model"].as_str().unwrap_or(&req.model).to_string() });
    meter.see(&crate::oak::Event::Usage(u));
    let (acct, plan) = usage_name(id);
    let info = crate::usage::Info { conv, ..Default::default() };
    crate::usage::record_claude(acct, plan, "keepalive".into(), &meter, util, req.ttl == crate::oak::Ttl::OneHour, info);
    Ok(u)
}

/// A Messages body without `stream`.
fn unstreamed(mut body: Value) -> Value {
    if let Some(o) = body.as_object_mut() {
        o.remove("stream");
    }
    body
}

/// `req`'s body as a request that generates nothing (`max_tokens: 0`, no
/// stream): it only reads (or writes) the cache.
fn generating_nothing(req: &crate::oak::Request) -> Value {
    let mut body = claude_body(req);
    body["max_tokens"] = json!(0);
    unstreamed(body)
}

/// Each conversation's last main reply (its id).
static LAST_REPLY: std::sync::LazyLock<Mutex<std::collections::HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

fn last_reply(conv: &str) -> Value {
    LAST_REPLY.lock().unwrap().get(conv).map_or(Value::Null, |id| json!(id))
}

/// A reply's `diagnostics` as the usage log has it: the reason and the
/// tokens past the change ("none" when nothing changed; "absent" in the
/// log: the reply had none, e.g. the first of a conversation).
pub(crate) fn miss_reason(d: &Value) -> (String, u64) {
    match d.get("cache_miss_reason") {
        None => ("none".into(), 0),
        Some(Value::Null) => ("pending".into(), 0),
        Some(r) => (r["type"].as_str().unwrap_or("unknown").to_string(), r["cache_missed_input_tokens"].as_u64().unwrap_or(0)),
    }
}

/// `open`, and when the API refuses with a 400 a body with something
/// experimental (`diagnostics`, the stable-prefix betas: `crate::prefix::betas`),
/// once more without it, that turned off for the session: a new field or
/// beta the account's gateway won't take mustn't stop the conversation.
fn open_extras(body: &Value, betas: &[&str], continuing: bool, seat: &Seat, tx: &Sender<Sse>) -> Result<(reqwest::blocking::Response, u64), Fail> {
    let extras = body.get("diagnostics").is_some() || betas.iter().any(|b| experimental(b));
    match open(body, betas, continuing, seat, tx) {
        Err(Fail::Report(Sse::Err(e))) if extras && e.starts_with("HTTP 400") => {
            eprintln!("Retrying without cache diagnostics and the stable-prefix betas after: {e}");
            set_diagnostics(false);
            set_stable_prefix(false);
            let mut plain = body.clone();
            if let Some(o) = plain.as_object_mut() {
                o.remove("diagnostics");
            }
            let plain = crate::prefix::plain_body(plain);
            let betas: Vec<&str> = betas.iter().copied().filter(|b| !experimental(b)).collect();
            open(&plain, &betas, continuing, seat, tx)
        }
        r => r,
    }
}

/// Account `id` as the usage log names it: its email (or what else is
/// known), and its plan.
fn usage_name(id: u64) -> (String, String) {
    let a = ACCOUNTS.lock().unwrap();
    match a.get(id) {
        Some(s) if !s.email.is_empty() => (s.email.clone(), s.plan.clone()),
        Some(s) => (s.label(), s.plan.clone()),
        None => (format!("account {}", id + 1), String::new()),
    }
}

/// Account `id`'s polled usage, for the usage log.
fn note_polled(id: u64) {
    let (acct, plan) = usage_name(id);
    let limits = ACCOUNTS.lock().unwrap().get(id).map(|s| s.limits.clone()).unwrap_or_default();
    crate::usage::note_limits(acct, plan, crate::usage::Util::of(&limits));
}

/// Reads a response's SSE stream, handing each event's JSON to `on` until
/// it breaks off (it has told the receiver what it needs to know). A broken
/// stream, or one that ends first, is reported to `tx` as an error.
pub(crate) fn read_sse(resp: reqwest::blocking::Response, tx: &Sender<Sse>, mut on: impl FnMut(Value) -> ControlFlow<()>) {
    for event in crate::provider::events(resp) {
        match event {
            Ok(Some(event)) => {
                if on(event).is_break() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => return drop(tx.send(Sse::Err(e))),
        }
    }
    let _ = tx.send(Sse::Err("Stream ended before the reply was complete".into()));
}

/// Reads a Messages SSE stream, sending its Oak events to `tx` (`by`'s
/// reasoning; `extra` adds what a provider reads beyond the Messages
/// format) and showing each to `seen` first, until `message_stop`, an
/// error event, or the end.
/// Returns early (dropping the connection) once the receiver is gone.
pub(crate) fn relay(
    resp: reqwest::blocking::Response,
    tx: &Sender<Sse>,
    by: &str,
    mut extra: impl FnMut(&Value) -> Vec<Event>,
    mut seen: impl FnMut(&Event),
) {
    read_sse(resp, tx, |event| {
        if event["type"] == "error" {
            let msg = event["error"]["message"].as_str().unwrap_or("unknown error");
            drop(tx.send(Sse::Err(format!("API error: {msg}"))));
            return ControlFlow::Break(());
        }
        for ev in crate::wire_anthropic::events(&event, by).into_iter().chain(extra(&event)) {
            seen(&ev);
            if tx.send(Sse::Event(ev)).is_err() {
                return ControlFlow::Break(());
            }
        }
        if event["type"] == "message_stop" {
            drop(tx.send(Sse::Done));
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
}

/// Whose request it is, for rotation: the conversation (empty for a
/// one-off) and how long the cache it writes lives.
struct Seat {
    conv: String,
    ttl_ms: i64,
}

impl Seat {
    fn of(req: &crate::oak::Request) -> Seat {
        // A one-off (a title) doesn't hold an account.
        let conv = req.seat_conv();
        Seat { conv, ttl_ms: req.ttl.ms() }
    }
}

/// Sends `body` until a response comes that can be read as a stream:
/// rotating accounts at a usage limit and retrying failures. `continuing`:
/// a thread continuation, which the server may refuse (`Fail::Refused`).
fn open(body: &Value, betas: &[&str], continuing: bool, seat: &Seat, tx: &Sender<Sse>) -> Result<(reqwest::blocking::Response, u64), Fail> {
    let (mut tries, mut rotations): (u32, u32) = (0, 0);
    let mut last = crate::rotation::seat(Provider::Claude, &seat.conv).and_then(|id| id.parse::<u64>().ok());
    loop {
        // The conversation's account, or another when it can't make
        // requests (used up, or its token can't be had); with every account
        // used up, the wait is for the first to reset.
        let id = account_for(&seat.conv, now_ms())?;
        if last.is_some_and(|l| l != id) {
            note_rotation();
            if tx.send(Sse::Rotated(label_of(id))).is_err() {
                return Err(Fail::Gone);
            }
        }
        last = Some(id);
        let result = send_as(id, reqwest::Method::POST, "/v1/messages", betas, true, Some(body));
        let rotate = match &result {
            // No token from the vault for this account (it is skipped for
            // a while): go through another.
            Err(e) => vault_failed(e),
            Ok(r) => {
                note_usage(id, r.headers());
                // This account is used up: the request goes again, through
                // another.
                match usage_limit_reset(r.status().as_u16(), r.headers(), now_ms()) {
                    Some(at) => {
                        note_limited(id, at);
                        if rotations >= 20 {
                            return Err(Fail::Report(Sse::Limit(at)));
                        }
                        true
                    }
                    None => false,
                }
            }
        };
        if rotate && rotations < 20 {
            rotations += 1;
            continue;
        }
        tries += 1;
        let retry = match &result {
            Ok(r) => matches!(r.status().as_u16(), 429 | 500..=599),
            Err(e) => e.starts_with("Network error") || vault_unreachable(e),
        };
        if retry && tries <= RETRIES {
            let (after, reason) = match result {
                Ok(r) => (retry_after(r.headers()), error_text(r)),
                Err(e) => (None, e),
            };
            if !crate::provider::wait_between(tx, tries, crate::provider::retry_in(after, tries), Some(reason)) {
                return Err(Fail::Gone);
            }
            continue;
        }
        return match result {
            Ok(r) if r.status().is_success() => {
                crate::rotation::used(Provider::Claude, &seat.conv, &id.to_string(), seat.ttl_ms, now_ms());
                if crate::provider::say_headers(tx, r.status().as_u16()) { Ok((r, id)) } else { Err(Fail::Gone) }
            }
            // A continuation the server can't take (its thread is gone, or
            // it won't go on from that reply): the caller starts a new thread.
            Ok(r) if continuing && matches!(r.status().as_u16(), 400 | 404 | 409 | 410 | 422) => Err(Fail::Refused(error_text(r))),
            Ok(r) => Err(Fail::err(error_text(r))),
            Err(e) => Err(Fail::err(e)),
        };
    }
}

/// The server-side threads beta: a request continues the server's copy of
/// the conversation instead of sending all of it again.
pub const THREADS_BETA: &str = "message-threads-2026-08-12";

// Server-side threads (a setting, off by default).
switch!(THREADS, threads, set_threads, false);

/// A prepared body as a thread continuation from reply `id`: the server has
/// the system prompt, the tools and the first `held` messages already (as
/// the client does it, only the billing header goes with every request).
fn continuation(body: &Value, id: &str, held: usize) -> Value {
    let mut c = serde_json::Map::new();
    for k in ["model", "max_tokens", "stream", "thinking", "context_management", "output_config", "metadata", "diagnostics"] {
        if let Some(v) = body.get(k) {
            c.insert(k.into(), v.clone());
        }
    }
    c.insert("system".into(), json!([body["system"][0].clone()]));
    let messages = body["messages"].as_array().map(|m| m[held.min(m.len())..].to_vec()).unwrap_or_default();
    c.insert("messages".into(), json!(messages));
    c.insert("thread".into(), json!({"type": "continue", "previous_message_id": id}));
    Value::Object(c)
}

/// Where the server's copy of a conversation stands after a reply in a
/// thread: the reply's id, and a hash of the request it answered (its
/// messages, and everything else), so the next request can tell whether
/// it still only adds to that.
#[derive(Clone, Default)]
pub struct Thread {
    pub id: String,
    held: usize,
    messages: u64,
    rest: u64,
}

impl Thread {
    /// For a request about to go; the id comes with its reply.
    pub fn of(req: &crate::oak::Request) -> Thread {
        Thread { id: String::new(), held: req.messages.len(), messages: hash(&req.messages), rest: rest_hash(req) }
    }

    /// How many leading messages of `body` the server's copy already has,
    /// if `body` only adds to it: everything but the messages the same,
    /// the messages this answered unchanged, then the reply, then only
    /// new user turns. The server goes on from its own copy of the reply.
    pub fn continues(&self, req: &crate::oak::Request) -> Option<usize> {
        let m = &req.messages;
        let ok = !self.id.is_empty()
            && m.len() > self.held + 1
            && m[self.held]["role"] == "assistant"
            && m[self.held + 1..].iter().all(|x| x["role"] == "user" || x["role"] == "system")
            && rest_hash(req) == self.rest
            && hash(&&m[..self.held]) == self.messages;
        ok.then_some(self.held + 1)
    }
}

/// A hash of everything in a request but its messages that the server's
/// copy depends on.
fn rest_hash(req: &crate::oak::Request) -> u64 {
    hash(&(&req.model, req.max_tokens, &req.system, &req.tools, req.tools_off, req.thinking, &req.effort))
}

/// A hash of a value's JSON, written straight into the hasher.
fn hash<T: serde::Serialize>(v: &T) -> u64 {
    use std::hash::Hasher;
    struct W(std::collections::hash_map::DefaultHasher);
    impl std::io::Write for W {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.write(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = W(Default::default());
    let _ = serde_json::to_writer(&mut w, v);
    w.0.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(req: &reqwest::blocking::Request, name: &str) -> String {
        req.headers().get(name).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
    }

    fn sample() -> crate::oak::Request {
        crate::oak::Request {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 32000,
            system: "You are Oak.".into(),
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "hello"}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "ok"}]}),
            ],
            tools: vec![
                json!({"name": "READ", "description": "d", "input_schema": {"type": "object"}}),
                json!({"name": "WRITE", "description": "d", "input_schema": {"type": "object"}}),
            ],
            thinking: true,
            effort: Some("high".into()),
            ..Default::default()
        }
    }

    fn oauth() -> Auth {
        Auth::Oauth { access: "a".into(), refresh: "r".into(), expires_at: 0 }
    }


    /// An account with its 5-hour and weekly usage (in percent).
    fn acct(id: u64, email: &str, five: f32, week: f32, now: i64) -> Stored {
        let mut s = Stored::new(id, oauth());
        s.email = email.into();
        s.fetched = true;
        s.limits = vec![
            Limit { label: "5h", percent: five, resets_at: Some(now + 3_600_000) },
            Limit { label: "Week", percent: week, resets_at: Some(now + 5 * 86_400_000) },
            // Only the 5-hour and weekly limits stop every request.
            Limit { label: "Opus wk", percent: 100.0, resets_at: Some(now + 86_400_000) },
        ];
        s
    }


    /// Rotation weighs each account by its tightest limit: used-up and
    /// refused ones can't make requests; logins on this machine are a
    /// fallback once the vault has accounts; an API key comes last.
    #[test]
    fn accounts_as_rotation_weighs_them() {
        let now = 1_793_100_000_000;
        let mut list = vec![
            acct(0, "first@gmail.com", 100.0, 40.0, now),
            acct(1, "second@gmail.com", 20.0, 90.0, now), // 10% left this week
            acct(2, "third@work.dev", 60.0, 30.0, now),   // 40% left
            acct(3, "fourth@gmail.com", 10.0, 100.0, now), // used up this week
        ];
        list[2].limited_until = now + 60_000;
        let c = candidates(&list, now);
        assert_eq!(c.iter().map(|c| c.usable).collect::<Vec<_>>(), [false, true, false, false]);
        assert_eq!((c[1].left, c[1].resets_at), (10.0, now + 5 * 86_400_000));
        assert!(c.iter().all(|c| c.preferred));
        assert_eq!(list[0].label(), "first");
        assert_eq!(list[2].label(), "third@work.dev");
        list.push(Stored::new(4, Auth::Remote { id: "r".into(), access: String::new(), expires_at: 0 }));
        list.push(Stored::new(5, Auth::ApiKey { key: "k".into() }));
        let c = candidates(&list, now);
        assert!(c[4].preferred && !c[1].preferred);
        assert_eq!(c[5].left, -1.0);
        assert_eq!(list[5].label(), "API key");
        // A used-up limit whose reset has passed doesn't block.
        let mut s = acct(5, "e@x.y", 100.0, 0.0, now);
        s.limits[0].resets_at = Some(now - 1);
        assert_eq!(s.blocked_until(now), 0);
        assert_eq!(s.headroom(now), 100.0);
    }

    /// The Claude Code request shape is what the gateway checks, so it is worth
    /// failing loudly if it changes. Values were captured from the installed
    /// client (2.1.303) rather than guessed.
    #[test]
    fn request_shapes() {
        let auth = Auth::Oauth { access: "sk-ant-oat-test".into(), refresh: "refresh".into(), expires_at: now_ms() + 3_600_000 };

        let body = claude_body(&sample());

        assert_eq!(body["system"].as_array().unwrap().len(), 3);
        assert_eq!(
            body["system"][0]["text"],
            "x-anthropic-billing-header: cc_version=2.1.303.914; cc_entrypoint=cli;"
        );
        assert_eq!(body["system"][0].get("cache_control"), None);
        // The system prompt's breakpoints are the one-hour kind, the
        // conversation's the lifetime asked for (five minutes here).
        assert_eq!(body["system"][1]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(body["system"][1]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(body["system"][2]["text"], "You are Oak.");
        assert_eq!(body["system"][2]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        // The client puts no breakpoint on tools.
        assert_eq!(body["tools"][0].get("cache_control"), None);
        assert_eq!(body["tools"][1].get("cache_control"), None);
        assert_eq!(body["messages"][2]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["messages"][0]["content"][0].get("cache_control"), None);
        assert_eq!(body.get("cache_control"), None);
        // The user's own thinking/effort settings survive the reshape.
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["context_management"]["edits"][0]["type"], "clear_thinking_20251015");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "t1");
        // The hour-long conversation breakpoint; no cache at all.
        let hour = claude_body(&crate::oak::Request { ttl: crate::oak::Ttl::OneHour, ..sample() });
        assert_eq!(hour["messages"][2]["content"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        // Five minutes after an hour: the hour's entry keeps its breakpoint.
        let anchored = claude_body(&crate::oak::Request { anchor: Some(0), ..sample() });
        assert_eq!(anchored["messages"][0]["content"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(anchored["messages"][2]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
        let off = claude_body(&crate::oak::Request { ttl: crate::oak::Ttl::Off, ..sample() });
        assert!(!off.to_string().contains("cache_control"));

        let req = request(reqwest::Method::POST, "/v1/messages", &[], true, &auth)
            .body("{}")
            .build()
            .unwrap();
        assert_eq!(header(&req, "authorization"), "Bearer sk-ant-oat-test");
        assert_eq!(header(&req, "x-api-key"), "");
        assert_eq!(header(&req, "anthropic-version"), "2023-06-01");
        assert_eq!(header(&req, "anthropic-beta"), concat!(
            "claude-code-20250219,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,",
            "context-management-2025-06-27,prompt-caching-scope-2026-01-05,effort-2025-11-24,",
            "extended-cache-ttl-2025-04-11,oauth-2025-04-20"
        ));
        // Quick retries, then every minute and a half.
        assert_eq!((1..=6).map(retry_wait).collect::<Vec<_>>(), vec![4, 8, 16, 32, 90, 90]);
        assert_eq!(header(&req, "user-agent"), "claude-cli/2.1.303 (external, cli)");
        assert_eq!(header(&req, "x-app"), "cli");
        assert_eq!(header(&req, "accept"), "application/json");
        assert_eq!(header(&req, "anthropic-dangerous-direct-browser-access"), "true");
        let session = header(&req, "x-claude-code-session-id");
        assert_eq!(session.len(), 36);
        assert_eq!(session.chars().filter(|c| *c == '-').count(), 4);

        // An API key is the same client, minus the OAuth beta and the bearer.
        let auth = Auth::ApiKey { key: "sk-ant-api-test".into() };
        assert_eq!(claude_body(&sample())["system"][1]["text"], CLAUDE_CODE_IDENTITY);

        let req = request(reqwest::Method::POST, "/v1/messages", &[], true, &auth)
            .body("{}")
            .build()
            .unwrap();
        assert_eq!(header(&req, "x-api-key"), "sk-ant-api-test");
        assert_eq!(header(&req, "authorization"), "");
        let betas = header(&req, "anthropic-beta");
        assert!(betas.starts_with("claude-code-20250219,"));
        assert!(!betas.contains("oauth-2025-04-20"));

        // The model list is not a Claude Code call, so it gets no client betas.
        let req = request(reqwest::Method::GET, "/v1/models?limit=1000", &[], false, &auth)
            .build()
            .unwrap();
        assert_eq!(header(&req, "anthropic-beta"), "");
        assert_eq!(header(&req, "accept"), "");
        assert_eq!(header(&req, "user-agent"), "claude-cli/2.1.303 (external, cli)");
    }
}
