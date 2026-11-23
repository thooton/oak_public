//! Antigravity OAuth 2.0 + PKCE sign-in, token renewal, project/tier discovery,
//! multi-account storage (`antigravity.json`), vault sync and account rotation.

use super::{catalog, default_project_id, endpoint_candidates, env_var, error_text, with_headers};
use crate::api::{self, Limit, Sse};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CALLBACK_PORT: u16 = 51121;
const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/aicode",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

// Google's public Antigravity desktop OAuth client (not a private server secret),
// matching pi-antigravity. Overridable via ANTIGRAVITY_CLIENT_ID / ANTIGRAVITY_CLIENT_SECRET.
const DEFAULT_CLIENT_ID_B64: &str = "MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ==";
const DEFAULT_CLIENT_SECRET_B64: &str = "R09DU1BYLUs1OEZXUjQ4NkxkTEoxbUxCOHNYQzR6NnFEQWY=";

/// How long before expiry an access token is renewed (ms).
const RENEW_EARLY_MS: i64 = 5 * 60_000;
/// How long a browser sign-in waits for the local callback.
const LOGIN_WAIT: Duration = Duration::from_secs(5 * 60);
/// Timeout for metadata discovery calls (loadCodeAssist / userinfo).
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

fn decode_b64(s: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default();
    String::from_utf8(bytes).unwrap_or_default()
}

pub(crate) fn client_id() -> String {
    env_var("CLIENT_ID").unwrap_or_else(|| decode_b64(DEFAULT_CLIENT_ID_B64))
}

pub(crate) fn client_secret() -> String {
    env_var("CLIENT_SECRET").unwrap_or_else(|| decode_b64(DEFAULT_CLIENT_SECRET_B64))
}

fn auth_url() -> String {
    std::env::var("OAK_ANTIGRAVITY_AUTH_URL").unwrap_or_else(|_| "https://accounts.google.com/o/oauth2/v2/auth".into())
}

fn token_url() -> String {
    std::env::var("OAK_ANTIGRAVITY_TOKEN_URL").unwrap_or_else(|_| "https://oauth2.googleapis.com/token".into())
}

fn userinfo_url() -> String {
    std::env::var("OAK_ANTIGRAVITY_USERINFO_URL").unwrap_or_else(|_| "https://www.googleapis.com/oauth2/v1/userinfo?alt=json".into())
}

// ---------------------------------------------------------------- stored accounts

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Cred {
    pub(crate) access: String,
    /// Empty for a login kept on the vault.
    #[serde(default)]
    pub(crate) refresh: String,
    /// When the access token expires (ms since 1970).
    pub(crate) expires: i64,
    /// Unique account key (lowercased email, or hash of refresh token).
    pub(crate) account_id: String,
    #[serde(default)]
    pub(crate) project_id: String,
    #[serde(default)]
    pub(crate) email: String,
    #[serde(default)]
    pub(crate) plan: String,
    /// The login is kept on the vault: tokens come from there.
    #[serde(default)]
    pub(crate) remote: bool,
}

#[derive(Clone)]
struct Acct {
    cred: Cred,
    limits: Vec<Limit>,
    fetched: bool,
    error: Option<String>,
    usage_at: i64,
    /// The account turned a request down for its quota limit until then (ms).
    limited_until: i64,
}

impl Acct {
    fn new(cred: Cred) -> Self {
        Self { cred, limits: vec![], fetched: false, error: None, usage_at: 0, limited_until: 0 }
    }
}

struct State {
    list: Vec<Acct>,
    /// Why a login stayed on this machine when a vault is connected.
    note: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State { list: Vec::new(), note: None });
/// Serializes token refreshes so concurrent requests don't race.
static RENEWING: Mutex<()> = Mutex::new(());

fn path() -> std::path::PathBuf {
    crate::store::dir().join("antigravity.json")
}

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

/// Reads saved Antigravity logins from `antigravity.json` and hydrates any
/// persisted dynamic model catalog. True when at least one login is present.
pub fn load() -> bool {
    catalog::load_cached();
    let creds = read_file();
    let mut s = STATE.lock().unwrap();
    let old = std::mem::take(&mut s.list);
    s.list = creds
        .into_iter()
        .map(|c| {
            old.iter()
                .find(|a| a.cred.account_id == c.account_id)
                .map_or_else(|| Acct::new(c.clone()), |a| Acct { cred: c.clone(), ..a.clone() })
        })
        .collect();
    !s.list.is_empty()
}

/// Signed in to Antigravity.
pub fn on() -> bool {
    !STATE.lock().unwrap().list.is_empty()
}

fn keep(c: Cred) {
    let mut s = STATE.lock().unwrap();
    match s.list.iter_mut().find(|a| a.cred.account_id == c.account_id) {
        Some(a) => {
            a.cred = c;
            a.limited_until = 0;
        }
        None => s.list.push(Acct::new(c)),
    }
    save(&s);
}

pub(crate) fn cred_of(id: &str) -> Option<Cred> {
    STATE.lock().unwrap().list.iter().find(|a| a.cred.account_id == id).map(|a| a.cred.clone())
}

/// Signs login `id` out: one on the vault is removed there too.
pub fn sign_out(id: &str) -> Result<(), String> {
    if cred_of(id).is_some_and(|c| c.remote) {
        api::vault_remove("antigravity", id)?;
    }
    forget(id);
    Ok(())
}

fn forget(id: &str) {
    let mut s = STATE.lock().unwrap();
    s.list.retain(|a| a.cred.account_id != id);
    s.note = None;
    save(&s);
    drop(s);
    crate::rotation::forget_account(api::Provider::Antigravity, id);
}

/// Disconnected from the vault: drop its logins from this machine.
pub fn forget_remote() {
    let mut s = STATE.lock().unwrap();
    let n = s.list.len();
    s.list.retain(|a| !a.cred.remote);
    if s.list.len() != n {
        save(&s);
    }
}

fn remote_cred(info: &Value, access: Option<(String, i64)>) -> Cred {
    let field = |k: &str| info[k].as_str().unwrap_or("").to_string();
    let (access, expires) = access.unwrap_or_default();
    let email = field("email");
    let account_id = {
        let id = field("account_id");
        if id.is_empty() { email.to_ascii_lowercase() } else { id }
    };
    let project_id = {
        let p = field("project_id");
        if p.is_empty() { default_project_id(if email.is_empty() { &account_id } else { &email }) } else { p }
    };
    Cred { access, refresh: String::new(), expires, account_id, project_id, email, plan: field("plan"), remote: true }
}

/// Brings logins in step with a connected vault.
pub fn sync() {
    let Ok(logins) = api::vault_list("antigravity") else { return };
    let ids: Vec<&str> = logins.iter().filter_map(|i| i["account_id"].as_str()).filter(|a| !a.is_empty()).collect();
    let mut s = STATE.lock().unwrap();
    let before: Vec<(String, bool)> = s.list.iter().map(|a| (a.cred.account_id.clone(), a.cred.remote)).collect();
    s.list.retain(|a| !a.cred.remote || ids.contains(&a.cred.account_id.as_str()));
    for info in &logins {
        let Some(id) = info["account_id"].as_str().filter(|a| !a.is_empty()) else { continue };
        match s.list.iter_mut().find(|a| a.cred.account_id == id) {
            Some(a) if a.cred.remote => {
                if let Some(e) = info["email"].as_str().filter(|s| !s.is_empty()) {
                    a.cred.email = e.to_string();
                }
                if let Some(p) = info["plan"].as_str().filter(|s| !s.is_empty()) {
                    a.cred.plan = p.to_string();
                }
                if let Some(p) = info["project_id"].as_str().filter(|s| !s.is_empty()) {
                    a.cred.project_id = p.to_string();
                }
            }
            Some(a) => a.cred = remote_cred(info, None),
            None => s.list.push(Acct::new(remote_cred(info, None))),
        }
    }
    if s.list.iter().map(|a| (a.cred.account_id.clone(), a.cred.remote)).collect::<Vec<_>>() != before {
        save(&s);
    }
}

pub fn remote() -> bool {
    STATE.lock().unwrap().list.iter().any(|a| a.cred.remote)
}

pub fn movable(id: &str) -> bool {
    api::vault_on() && cred_of(id).is_some_and(|c| !c.remote)
}

pub fn note() -> Option<String> {
    STATE.lock().unwrap().note.clone()
}

fn upload(tokens: &Value) -> Result<Cred, String> {
    let info = api::vault_add("antigravity", tokens)?;
    if info["account_id"].as_str().is_none_or(str::is_empty) {
        return Err(format!("{} didn't say which Antigravity account it took.", api::capital(&api::vault_name())));
    }
    let access = tokens["access_token"].as_str().map(|a| (a.to_string(), tokens["expires_at"].as_i64().unwrap_or(0)));
    Ok(remote_cred(&info, access))
}

pub fn move_to_vault(id: &str) -> Result<(), String> {
    let _renewing = RENEWING.lock().unwrap();
    let c = cred_of(id).filter(|c| !c.remote).ok_or("There is no such Antigravity login on this machine.")?;
    let moved = upload(&json!({
        "account_id": c.account_id,
        "refresh_token": c.refresh,
        "access_token": c.access,
        "expires_at": c.expires,
        "project_id": c.project_id,
        "email": c.email,
        "plan": c.plan,
    }))?;
    keep(moved);
    STATE.lock().unwrap().note = None;
    Ok(())
}

// ---------------------------------------------------------------- OAuth & discovery

fn form(pairs: &[(&str, &str)]) -> String {
    reqwest::Url::parse_with_params("http://x/", pairs).map(|u| u.query().unwrap_or("").to_string()).unwrap_or_default()
}

fn token_request(pairs: &[(&str, &str)]) -> Result<Value, String> {
    let resp = api::http()
        .post(token_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form(pairs))
        .send()
        .map_err(api::network_error)?;
    api::json_or(resp, error_text)
}

fn fetch_user_email(token: &str) -> Option<String> {
    let resp = api::http()
        .get(userinfo_url())
        .bearer_auth(token)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().ok()?;
    v["email"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// Extracts a project id from a `loadCodeAssist` or `listCloudAICompanionProjects` payload.
pub(crate) fn extract_project_id(data: &Value) -> Option<String> {
    let obj = data.as_object()?;
    for key in [
        "antigravityProjectId",
        "projectId",
        "backendProjectId",
        "userDefinedCloudaicompanionProject",
        "cloudaicompanionProject",
        "project",
    ] {
        if let Some(v) = obj.get(key) {
            if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
                return Some(s.to_string());
            }
            if let Some(s) = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                return Some(s.to_string());
            }
        }
    }
    for key in ["projects", "projectIds", "cloudaicompanionProjects"] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            for item in arr {
                if let Some(s) = extract_project_id(item) {
                    return Some(s);
                }
                if let Some(s) = item.as_str().filter(|s| !s.is_empty()) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// Extracts the user's subscription tier label from `loadCodeAssist`.
/// Google returns `currentTier: free-tier` even on paid plans; `paidTier` holds
/// the real subscription (e.g. "Google AI Pro", "Google AI Ultra").
pub(crate) fn extract_plan(data: &Value) -> Option<String> {
    for key in ["paidTier", "currentTier"] {
        let tier = &data[key];
        if let Some(name) = tier["name"].as_str().filter(|s| !s.is_empty()) {
            return Some(name.to_string());
        }
        if let Some(id) = tier["id"].as_str().filter(|s| !s.is_empty()) {
            return Some(id.to_string());
        }
    }
    None
}

fn list_companion_projects(token: &str) -> Option<String> {
    for endpoint in endpoint_candidates() {
        let Ok(resp) = with_headers(
            api::http()
                .post(format!("{endpoint}/v1internal:listCloudAICompanionProjects"))
                .timeout(DISCOVERY_TIMEOUT)
                .json(&json!({})),
            token,
        )
        .send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Ok(v) = resp.json::<Value>()
            && let Some(id) = extract_project_id(&v)
        {
            return Some(id);
        }
    }
    None
}

/// Calls `v1internal:loadCodeAssist`, returning `(project_id, plan)`.
pub(crate) fn load_code_assist(token: &str) -> (Option<String>, Option<String>) {
    let body = json!({
        "metadata": {
            "ideType": "ANTIGRAVITY",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        }
    });
    for endpoint in endpoint_candidates() {
        let Ok(resp) = with_headers(
            api::http()
                .post(format!("{endpoint}/v1internal:loadCodeAssist"))
                .timeout(DISCOVERY_TIMEOUT)
                .json(&body),
            token,
        )
        .send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Ok(v) = resp.json::<Value>() {
            let plan = extract_plan(&v);
            let project = extract_project_id(&v).or_else(|| list_companion_projects(token));
            return (project, plan);
        }
    }
    (None, None)
}

fn account_id_for(email: &str, refresh: &str) -> String {
    let e = email.trim().to_ascii_lowercase();
    if !e.is_empty() {
        return e;
    }
    format!("account-{}", api::hex(&Sha256::digest(refresh.as_bytes())[..8]))
}

/// Returns `(access_token, project_id)` for login `id`, renewing the token first
/// when close to expiry or when `force` is set (after HTTP 401).
pub(crate) fn access(id: &str, force: bool) -> Result<(String, String), String> {
    const OUT: &str = "Not signed in to Antigravity: add an Antigravity account on Home";
    let fresh = |c: &Cred| !c.access.is_empty() && c.expires - api::now_ms() > RENEW_EARLY_MS;
    let seen = cred_of(id).ok_or(OUT)?;
    if !force && fresh(&seen) {
        let project = if seen.project_id.is_empty() { default_project_id(&seen.email) } else { seen.project_id };
        return Ok((seen.access, project));
    }
    if seen.remote {
        return remote_access(&seen, force);
    }
    let _renewing = RENEWING.lock().unwrap();
    let mut c = cred_of(id).ok_or(OUT)?;
    if let Some(f) = read_file().into_iter().find(|f| f.account_id == id && f.access != c.access) {
        keep(f.clone());
        c = f;
    }
    if !force && c.access != seen.access && fresh(&c) {
        let project = if c.project_id.is_empty() { default_project_id(&c.email) } else { c.project_id };
        return Ok((c.access, project));
    }
    if c.refresh.is_empty() {
        return Err("Antigravity login has no refresh token (sign in again on Home)".into());
    }
    let cid = client_id();
    let csec = client_secret();
    let tokens = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &c.refresh),
        ("client_id", &cid),
        ("client_secret", &csec),
    ])
    .map_err(|e| format!("Couldn't renew the Antigravity sign-in (add the account on Home again): {e}"))?;
    let access_tok = tokens["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Antigravity token refresh returned no access token")?
        .to_string();
    let expires_in = tokens["expires_in"].as_i64().unwrap_or(3600);
    let mut updated = c.clone();
    updated.access = access_tok;
    if let Some(r) = tokens["refresh_token"].as_str().filter(|s| !s.is_empty()) {
        updated.refresh = r.to_string();
    }
    updated.expires = api::now_ms() + expires_in * 1000;
    if updated.project_id.is_empty() {
        let (proj, plan) = load_code_assist(&updated.access);
        updated.project_id = proj.unwrap_or_else(|| default_project_id(if updated.email.is_empty() { "antigravity-default" } else { &updated.email }));
        if let Some(p) = plan {
            updated.plan = p;
        }
    }
    keep(updated.clone());
    Ok((updated.access, updated.project_id))
}

fn remote_access(seen: &Cred, force: bool) -> Result<(String, String), String> {
    let refused = (force && !seen.access.is_empty()).then_some(seen.access.as_str());
    let id = seen.account_id.as_str();
    let Some(v) = api::vault_token("antigravity", id, refused)? else {
        forget(id);
        return Err(format!("The Antigravity login was removed from {}: add it again on Home", api::vault_name()));
    };
    let access = v["access_token"]
        .as_str()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("{} sent no Antigravity token.", api::capital(&api::vault_name())))?
        .to_string();
    let mut c = seen.clone();
    c.access = access;
    c.expires = v["expires_at"].as_i64().unwrap_or(0);
    if let Some(p) = v["project_id"].as_str().filter(|p| !p.is_empty()) {
        c.project_id = p.to_string();
    }
    if c.project_id.is_empty() {
        c.project_id = default_project_id(if c.email.is_empty() { &c.account_id } else { &c.email });
    }
    keep(c.clone());
    Ok((c.access, c.project_id))
}

// ---------------------------------------------------------------- browser login

/// An Antigravity browser sign-in under way: the Google OAuth URL to open and
/// a loopback callback server on `localhost:51121/oauth-callback`.
pub struct Login {
    pub url: String,
    verifier: String,
    state: String,
    result: Arc<Mutex<Option<Result<(), String>>>>,
    stop: Arc<AtomicBool>,
    /// True when the local callback server is listening on port 51121.
    pub listening: bool,
}

impl Drop for Login {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts a Google OAuth 2.0 + PKCE sign-in for Antigravity.
pub fn start_login() -> Login {
    let bytes: [u8; 32] = rand::random();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state_bytes: [u8; 32] = rand::random();
    let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state_bytes);
    let cid = client_id();
    let scope = SCOPES.join(" ");
    let query = form(&[
        ("client_id", &cid),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", &scope),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ]);
    let mut login = Login {
        url: format!("{}?{query}", auth_url()),
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
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else { continue };
        if url.path() != "/oauth-callback" {
            let _ = conn.write_all(page(404, "Not found", "").as_bytes());
            continue;
        }
        let get = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let outcome = match (get("code"), get("state"), get("error")) {
            (_, _, Some(e)) => Err(format!("The sign-in was refused: {}", get("error_description").unwrap_or(e))),
            (Some(_), Some(s), _) if s != state => Err("The sign-in came back for another request (state mismatch): try again".into()),
            (Some(_), None, _) => Err("The sign-in came back without a state parameter".into()),
            (Some(code), Some(_), _) => exchange(&code, verifier),
            (None, ..) => Err("The sign-in came back without a code".into()),
        };
        let html = match &outcome {
            Ok(()) => page(200, "Signed in to Antigravity", "You can close this tab and go back to Oak."),
            Err(e) => page(400, "Couldn't sign in", e),
        };
        let _ = conn.write_all(html.as_bytes());
        let _ = conn.flush();
        *result.lock().unwrap() = Some(outcome);
        return;
    }
}

fn page(status: u16, title: &str, text: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
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
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// How the browser sign-in ended, once it has (taken on read).
pub fn login_result(login: &Login) -> Option<Result<(), String>> {
    login.result.lock().unwrap().take()
}

/// Finishes a sign-in with a pasted callback URL or query string.
pub fn finish_login(login: &Login, pasted: &str) -> Result<(), String> {
    let (code, state) = parse_pasted(pasted).ok_or("Paste the full callback URL from your browser (http://localhost:51121/oauth-callback?state=…&code=…)")?;
    if let Some(s) = state
        && s != login.state
    {
        return Err("That address is from another sign-in: start again".into());
    }
    exchange(&code, &login.verifier)
}

/// Extracts `(code, state)` from a pasted callback URL, query string, or bare code.
pub(crate) fn parse_pasted(text: &str) -> Option<(String, Option<String>)> {
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
    if get("error").is_some() {
        return None;
    }
    Some((get("code").filter(|c| !c.is_empty())?, get("state")))
}

fn exchange(code: &str, verifier: &str) -> Result<(), String> {
    let cid = client_id();
    let csec = client_secret();
    let tokens = token_request(&[
        ("grant_type", "authorization_code"),
        ("client_id", &cid),
        ("client_secret", &csec),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ])?;
    let access_tok = tokens["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("The sign-in gave no access token")?
        .to_string();
    let refresh_tok = tokens["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("No refresh token received. Sign in again and allow offline access.")?
        .to_string();
    let expires_in = tokens["expires_in"].as_i64().unwrap_or(3600);
    let expires = api::now_ms() + expires_in * 1000;

    let email = fetch_user_email(&access_tok).unwrap_or_default();
    let (discovered_project, plan) = load_code_assist(&access_tok);
    let project_id = discovered_project.unwrap_or_else(|| default_project_id(if email.is_empty() { "antigravity-default" } else { &email }));
    let account_id = account_id_for(&email, &refresh_tok);
    let local = Cred {
        access: access_tok,
        refresh: refresh_tok,
        expires,
        account_id,
        project_id,
        email,
        plan: plan.unwrap_or_default(),
        remote: false,
    };

    let mut note = None;
    let cred = if api::signing_in_remotely() {
        let up = json!({
            "account_id": local.account_id,
            "refresh_token": local.refresh,
            "access_token": local.access,
            "expires_at": local.expires,
            "project_id": local.project_id,
            "email": local.email,
            "plan": local.plan,
        });
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
    let tok = cred.access.clone();
    let proj = cred.project_id.clone();
    keep(cred);
    STATE.lock().unwrap().note = note;
    api::note_signed_in();
    // Hydrate the live catalog right away (best effort, never fails login).
    let _ = catalog::refresh_catalog(&tok, &proj);
    Ok(())
}

// ---------------------------------------------------------------- accounts & usage

/// Antigravity logins formatted for Oak's account boxes and Home.
pub fn accounts() -> Vec<(String, api::Account)> {
    let paired = api::vault_on();
    let s = STATE.lock().unwrap();
    s.list
        .iter()
        .map(|a| {
            let c = &a.cred;
            let view = api::Account {
                provider: api::Provider::Antigravity,
                name: "Antigravity".into(),
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

/// The first Antigravity login, for Settings' connection summary.
pub fn account() -> Option<api::Account> {
    accounts().into_iter().next().map(|(_, a)| a)
}

/// Refreshes each Antigravity account's plan, quota limits, and dynamic model catalog.
pub fn refresh() {
    let ids: Vec<String> = STATE.lock().unwrap().list.iter().map(|a| a.cred.account_id.clone()).collect();
    let mut refreshed_catalog = false;
    for id in ids {
        let r = access(&id, false).and_then(|(token, project_id)| {
            let (discovered_proj, plan) = load_code_assist(&token);
            let effective_project = discovered_proj.unwrap_or(project_id);
            let (limits, catalog_ok) = catalog::fetch_usage_and_catalog(&token, &effective_project, !refreshed_catalog)?;
            if catalog_ok {
                refreshed_catalog = true;
            }
            Ok((effective_project, plan, limits))
        });
        let mut s = STATE.lock().unwrap();
        let mut changed = false;
        let mut freed = false;
        if let Some(a) = s.list.iter_mut().find(|a| a.cred.account_id == id) {
            a.fetched = true;
            match r {
                Ok((project_id, plan, limits)) => {
                    if !project_id.is_empty() && a.cred.project_id != project_id {
                        a.cred.project_id = project_id;
                        changed = true;
                    }
                    if let Some(p) = plan
                        && !p.is_empty()
                        && a.cred.plan != p
                    {
                        a.cred.plan = p;
                        changed = true;
                    }
                    a.limits = limits;
                    a.error = None;
                    a.usage_at = api::now_ms();
                    // Early reset recognition: if a newer reading shows room, clear limited_until.
                    let now = a.usage_at;
                    let any_full = a.limits.iter().any(|l| l.resets_at.is_none_or(|r| r > now) && l.percent >= 100.0);
                    if a.limited_until > now && !a.limits.is_empty() && !any_full {
                        a.limited_until = 0;
                        freed = true;
                    }
                }
                Err(e) => a.error = Some(e),
            }
        }
        if changed {
            save(&s);
        }
        drop(s);
        if freed {
            api::note_signed_in();
        }
    }
}

// ---------------------------------------------------------------- rotation

fn candidates(now: i64) -> Vec<crate::rotation::Candidate> {
    let s = STATE.lock().unwrap();
    let remote = s.list.iter().any(|a| a.cred.remote);
    s.list
        .iter()
        .map(|a| {
            let live = |l: &&Limit| l.resets_at.is_none_or(|r| r > now);
            let used = a.limits.iter().filter(live).map(|l| l.percent).fold(0.0f32, f32::max);
            let spent = a.limits.iter().filter(live).filter(|l| l.percent >= 100.0).filter_map(|l| l.resets_at).max().unwrap_or(0);
            let next_reset = a.limits.iter().filter(live).filter_map(|l| l.resets_at).min().unwrap_or(i64::MAX);
            crate::rotation::Candidate {
                id: a.cred.account_id.clone(),
                left: (100.0 - used.min(100.0)) as f64,
                usable: a.limited_until.max(spent) <= now,
                resets_at: next_reset,
                preferred: a.cred.remote || !remote,
            }
        })
        .collect()
}

/// Picks the Antigravity login for conversation `conv`, or returns an SSE error/limit.
pub(crate) fn login_for(conv: &str) -> Result<String, Option<Sse>> {
    let now = api::now_ms();
    let cands = candidates(now);
    if cands.is_empty() {
        return Err(Some(Sse::Err("Not signed in to Antigravity: add an Antigravity account on Home".into())));
    }
    match crate::rotation::pick(api::Provider::Antigravity, conv, &cands, now, !api::rotation_on()) {
        Some(id) if cands.iter().any(|c| c.id == id && c.usable) => Ok(id),
        _ => {
            let s = STATE.lock().unwrap();
            let first = s
                .list
                .iter()
                .map(|a| a.limited_until)
                .filter(|&t| t > now)
                .min()
                .unwrap_or(now + 15 * 60_000);
            Err(Some(Sse::Limit(first)))
        }
    }
}

/// Marks login `id` quota-limited until `until`. True when another login can take over.
pub(crate) fn limited(id: &str, until: i64) -> bool {
    let mut s = STATE.lock().unwrap();
    if let Some(a) = s.list.iter_mut().find(|a| a.cred.account_id == id) {
        a.limited_until = a.limited_until.max(until);
    }
    drop(s);
    candidates(api::now_ms()).iter().any(|c| c.usable)
}

pub(crate) fn label_of(id: &str) -> String {
    cred_of(id).map_or_else(|| "Antigravity".into(), |c| if c.email.is_empty() { "Antigravity".into() } else { c.email })
}
