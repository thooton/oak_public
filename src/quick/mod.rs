//! AWS Quick: Amazon Quick's own agent, run for Oak as a model provider.
//!
//! Quick is not a pipe for tokens: it runs a separate agent, with its own
//! system prompt, its own agent loop and its own model (Claude Opus through
//! Bedrock, `turn_usage.resolved_model_id`). Oak talks to it the way Quick's
//! desktop app does, over its chat WebSocket, so the transcript Oak shows is
//! Quick's own work: its thinking, its text, and each tool it asks for.
//!
//! **Signing in** is OIDC against AWS IAM Identity Center, as Quick's app
//! does it: a public client is registered for Oak (`/client/register`), the
//! user approves it in the browser with PKCE, and the authorization code is
//! exchanged for an access token (`aoa…`, an hour) and a refresh token
//! (`aor…`). The refresh token is the one that lasts: every request spends an
//! access token and the refresh token is what buys the next one.
//!
//! **A conversation** is a Quick thread: its id is the conversation's, and
//! each request sends the newest user turn, since Quick keeps the thread's
//! history itself.
//!
//! **Tools.** Quick's agent asks for tools by name (`tool_call`) and waits
//! for the result on the socket (`tool_result` with its `tool_call_id`).
//! Quick's own sandbox runs file and shell work against copies under
//! `/outposts/<device>/…`, not the user's folder, so Oak answers every call
//! itself, in the conversation's working folder, with the same tools it runs
//! for every other model (`crate::agent::tools`). That is why Quick works on
//! the real project rather than a copy of it.
//!
//! Wire details are Quick's own: frames are JSON inside a JSON envelope with
//! the body base64'd (`rawInputEvent`), and the server answers in AWS
//! event-stream messages whose payload is base64 again.

use crate::api::{self, Sse};
use crate::oak::{self, Event, Request, Usage, stop};
use crate::provider;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::mpsc::Sender;
use std::time::Duration;

pub mod socket;
pub mod turn;
#[cfg(test)]
mod live_test;
#[cfg(test)]
mod models_test;

/// The surface Quick's desktop app reports; the agent tailors itself to it.
const SURFACE: &str = "QUICK_WORK_DESKTOP_WINDOWS";

/// What Oak reports as Quick's app version on every frame. The server's
/// feature gate reads it, so it is Quick's own version rather than Oak's.
const APP_VERSION: &str = "1.2.4660";

/// The chat agent a conversation goes to: Quick's system agent.
const CHAT_AGENT: &str = "arn:aws:quicksight:us-east-1:156891495215:agent/SYSTEM";

/// IAM Identity Center: the instance Quick's desktop app is registered
/// against, which the sign-in flow reports (`issuerUrl`).
const IDENTITY_STORE: &str = "ssoins-72233782ae72fb91";

/// The cell Quick's desktop app signs this identity store's accounts in to
/// (`tenantUrl` in its sign-in's callback). Quick's sign-in doesn't always
/// report it, and the chat socket's host is per workspace, so this is the
/// fallback; `OAK_QUICK_HOST` overrides it.
const DEFAULT_CELL: &str = "qbs-cellff9e0f5c-3614-471c-b8fe-9d3c8f3afff9.dp.appintegrations.us-east-1.prod.plato.ai.aws.dev";

/// The OIDC endpoints (region-scoped, as the app uses them).
const OIDC_AUTHORIZE: &str = "https://oidc.us-east-1.amazonaws.com/authorize";
const OIDC_TOKEN: &str = "https://oidc.us-east-1.amazonaws.com/token";
const OIDC_REGISTER: &str = "https://oidc.us-east-1.amazonaws.com/client/register";

/// The scope Quick's client asks for.
const SCOPE: &str = "quicksight:read_write";

/// The port the browser hands the code back to. Quick's app uses 18080; Oak
/// takes the first free port from here up (`login::listen`).
const CALLBACK_PORT: u16 = 18080;

/// A model OAK offers through Quick. Quick decides the real model
/// (`turn_usage.resolved_model_id`); these are the modes it runs in.
pub struct Model {
    /// The id Quick takes as `agent_mode`, and OAK's id for it.
    pub id: &'static str,
    pub name: &'static str,
    /// Its context window (tokens), as `turn_usage.context_window`.
    pub context: u64,
    /// The most a reply may write.
    pub max_output: u64,
    /// The effort levels Quick takes (`thinking_effort`).
    pub efforts: &'static [&'static str],
}

/// What each mode is for, in Quick's own words: `fast` is quickest,
/// `smart` balances quality and speed, `balanced` weighs cost.
pub const MODELS: &[Model] = &[
    Model { id: "smart", name: "Quick Smart", context: 1_000_000, max_output: 64_000, efforts: &["low", "medium", "high", "xhigh", "max"] },
    Model { id: "fast", name: "Quick Fast", context: 1_000_000, max_output: 64_000, efforts: &["low", "medium", "high"] },
    Model { id: "balanced", name: "Quick Balanced", context: 1_000_000, max_output: 64_000, efforts: &["low", "medium", "high"] },
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// A model in the picker served through Quick.
pub fn serves(id: &str) -> bool {
    model(id).is_some()
}

/// The effort Quick should think at for request `r` to `m`: none for a
/// title (nothing is shown) and the model's lowest for a summary, else the
/// request's own, fitted to the levels the mode takes.
fn effort(r: &Request, m: &Model) -> &'static str {
    if r.purpose == oak::Purpose::Title {
        return "off";
    }
    let asked = r.effort.as_deref().unwrap_or("medium");
    let pick = match asked {
        "off" | "none" => "off",
        "low" | "min" | "minimal" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "xhigh" | "max" | "ultra" => "max",
        other => other,
    };
    if m.efforts.contains(&pick) {
        return m.efforts.iter().find(|e| **e == pick).copied().unwrap_or("low");
    }
    // Past its levels: its highest that isn't above what was asked.
    m.efforts.last().copied().unwrap_or("low")
}

// ---------------------------------------------------------------- the login

pub mod login {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// quick.json: the registered client, the refresh token, who signed in,
    /// and what Quick's absence costs (the last error).
    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct Stored {
        /// The OIDC client Oak registered for itself (`client_id`).
        pub client_id: String,
        /// Its secret, as the registration gave it.
        pub client_secret: String,
        /// The token that lasts (`aor…`); each request buys an access token
        /// with it.
        pub refresh: String,
        /// The access token (`aoa…`), and when it stops working (ms).
        #[serde(default)]
        pub access: String,
        #[serde(default)]
        pub expires_at: i64,
        /// Who signed in, for Home.
        #[serde(default)]
        pub email: String,
        #[serde(default)]
        pub tenant: String,
        /// The socket's host (`<cell>.dp.appintegrations.<region>.prod.plato.ai.aws.dev`).
        #[serde(default)]
        pub host: String,
        /// The last sign-in or request error.
        #[serde(default)]
        pub error: String,
    }

    /// Where the sign-in is: idle, waiting for the browser, or done.
    #[derive(Clone, Debug, PartialEq)]
    pub enum State {
        Idle,
        /// The address to open, and the port Oak listens on for the code.
        Waiting { url: String, port: u16 },
        Done,
        Failed(String),
    }

    pub(super) static STORE: LazyLock<Mutex<Stored>> = LazyLock::new(|| Mutex::new(crate::store::read_json(&path()).unwrap_or_default()));
    pub(super) static STATE: Mutex<Option<State>> = Mutex::new(None);
    /// A sign-in in flight: one at a time.
    static RUNNING: AtomicBool = AtomicBool::new(false);

    fn path() -> std::path::PathBuf {
        crate::store::dir().join("quick.json")
    }

    pub(super) fn save() {
        let s = STORE.lock().unwrap();
        let _ = crate::store::write_private_json(&path(), &*s);
    }

    /// The stored login, for the UI.
    pub fn stored() -> Stored {
        STORE.lock().unwrap().clone()
    }

    pub fn state() -> Option<State> {
        STATE.lock().unwrap().clone()
    }

    fn set_state(s: State) {
        *STATE.lock().unwrap() = Some(s);
    }

    /// Loads quick.json at start.
    pub fn load() -> bool {
        let s = crate::store::read_json::<Stored>(&path()).unwrap_or_default();
        let on = !s.refresh.is_empty();
        *STORE.lock().unwrap() = s;
        on
    }

    /// A login is on this machine (an expired access token still counts: the
    /// refresh token is what Oak keeps).
    pub fn on() -> bool {
        !STORE.lock().unwrap().refresh.is_empty()
    }

    /// Forgets the login, here and in the browser's session.
    pub fn sign_out() {
        {
            let mut s = STORE.lock().unwrap();
            *s = Stored::default();
        }
        let _ = std::fs::remove_file(path());
        set_state(State::Idle);
        crate::rotation::forget_account(crate::api::Provider::Quick, "");
    }

    /// The access token, bought with the refresh token (`expires_at` kept).
    /// Err says what the server said, and marks the login failed.
    pub fn access_token() -> Result<String, String> {
        let (id, secret, refresh, mut access, expires) = {
            let s = STORE.lock().unwrap();
            (s.client_id.clone(), s.client_secret.clone(), s.refresh.clone(), s.access.clone(), s.expires_at)
        };
        if refresh.is_empty() {
            return Err("No Quick account: sign in on Home.".into());
        }
        if !access.is_empty() && expires > crate::api::now_ms() + 60_000 {
            return Ok(access);
        }
        let body = json!({
            "clientId": id,
            "clientSecret": secret,
            "grantType": "refresh_token",
            "refreshToken": refresh,
        });
        let resp = api::http()
            .post(OIDC_TOKEN)
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(api::network_error)?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            let why = serde_json::from_str::<Value>(&text).map(|v| provider::error_msg(&v, &text, None)).unwrap_or_else(|_| text.chars().take(300).collect());
            let mut s = STORE.lock().unwrap();
            s.error = format!("Quick refused to renew the sign-in ({status}): {why}");
            drop(s);
            save();
            return Err(format!("Quick: {why}. Sign in again on Home."));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("Quick sent an unreadable answer: {e}"))?;
        access = v["accessToken"].as_str().unwrap_or("").to_string();
        let next = v["refreshToken"].as_str().unwrap_or("").to_string();
        let ttl = v["expiresIn"].as_u64().unwrap_or(3600) as i64;
        if access.is_empty() {
            return Err("Quick's sign-in answered without an access token.".into());
        }
        {
            let mut s = STORE.lock().unwrap();
            s.access = access.clone();
            s.expires_at = crate::api::now_ms() + ttl * 1000;
            if !next.is_empty() {
                s.refresh = next;
            }
            s.error.clear();
        }
        save();
        Ok(access)
    }

    /// The socket's host for the signed-in account: the `tenantUrl` Quick's
    /// sign-in reports (its cell's own address), the cell Oak was configured
    /// with, or the one Quick's desktop app uses for this identity store.
    pub fn host() -> Result<String, String> {
        let stored = STORE.lock().unwrap().host.clone();
        if !stored.is_empty() {
            return Ok(stored);
        }
        match known_cell() {
            Some(h) => {
                STORE.lock().unwrap().host = h.clone();
                save();
                Ok(h)
            }
            None => Err("Quick's workspace address wasn't reported at sign-in: sign in again on Home.".into()),
        }
    }

    /// The host Oak saved at sign-in, for the UI.
    pub fn saved_host() -> String {
        STORE.lock().unwrap().host.clone()
    }

    /// The cell Oak's Quick login runs on. Quick's sign-in reports it
    /// (`tenantUrl`); when the callback doesn't, this is the cell Quick's
    /// desktop app signs in to for this identity store, and `OAK_QUICK_HOST`
    /// names another (a stand-in or a proxy, as `OAK_API_BASE` does for
    /// Claude).
    fn known_cell() -> Option<String> {
        if let Some(h) = std::env::var("OAK_QUICK_HOST").ok().map(|h| h.trim().trim_start_matches("https://").trim_end_matches('/').to_string()).filter(|h| !h.is_empty()) {
            return Some(h);
        }
        (!DEFAULT_CELL.is_empty()).then(|| DEFAULT_CELL.to_string())
    }

    /// Starts a fresh sign-in: registers Oak's client with the identity
    /// store, opens the browser at the approval page, and waits (in the
    /// background) for the code on the loopback port.
    pub fn start() -> Result<String, String> {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err("Quick's sign-in is already waiting for the browser.".into());
        }
        let (verifier, challenge) = pkce();
        let state = hex(rand::random::<[u8; 16]>());
        // Quick's desktop sign-in carries a `nonce`, and the workspace it
        // signed in to comes back with the code (it names the cell Oak's
        // socket goes to).
        let nonce = hex(rand::random::<[u8; 16]>());
        // The client, then the port it comes back to: the registration has
        // to name the redirect URI, so the port is taken first.
        let listener = match listen() {
            Ok(l) => l,
            Err(e) => {
                RUNNING.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        let port = listener.0.local_addr().map(|a| a.port()).unwrap_or(CALLBACK_PORT);
        let redirect = format!("http://127.0.0.1:{port}");
        let client = match register(&redirect) {
            Ok(c) => c,
            Err(e) => {
                RUNNING.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        let url = format!(
            "{OIDC_AUTHORIZE}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}&code_challenge={challenge}&code_challenge_method=S256&prompt=login&nonce={}",
            urlencode(&client.0),
            urlencode(&redirect),
            urlencode(SCOPE),
            urlencode(&nonce),
        );
        set_state(State::Waiting { url: url.clone(), port });
        let client2 = client.clone();
        let verifier2 = verifier.clone();
        let state2 = state.clone();
        std::thread::Builder::new()
            .name("quick-login".into())
            .spawn(move || {
                let out = wait_for_code(listener, &state2);
                match out {
                    Ok((code, host)) => match exchange(&client2, &code, &redirect, &verifier2) {
                        Ok(()) => {
                            let mut s = STORE.lock().unwrap();
                            if !host.is_empty() {
                                s.host = host;
                            }
                            s.client_id = client2.0.clone();
                            s.client_secret = client2.1.clone();
                            s.error.clear();
                            drop(s);
                            save();
                            set_state(State::Done);
                            crate::api::note_signed_in();
                        }
                        Err(e) => {
                            STORE.lock().unwrap().error = e.clone();
                            save();
                            set_state(State::Failed(e));
                        }
                    },
                    Err(e) => set_state(State::Failed(e)),
                }
                RUNNING.store(false, Ordering::SeqCst);
            })
            .map_err(|e| format!("Couldn't start the sign-in: {e}"))?;
        Ok(url)
    }

    fn hex(bytes: [u8; 16]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A PKCE pair: the verifier, and its S256 challenge.
    fn pkce() -> (String, String) {
        use sha2::{Digest, Sha256};
        let raw: [u8; 32] = rand::random();
        let verifier = b64url(&raw);
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        (verifier, challenge)
    }

    fn b64url(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn urlencode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// A loopback listener for the code, on the first free port from
    /// `CALLBACK_PORT`.
    fn listen() -> Result<(std::net::TcpListener, u16), String> {
        let mut last = String::new();
        for port in CALLBACK_PORT..CALLBACK_PORT + 40 {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => return Ok((l, port)),
                Err(e) => last = e.to_string(),
            }
        }
        Err(format!("Couldn't find a free port for Quick's sign-in: {last}"))
    }

    /// Registers Oak's public client with the identity store.
    fn register(redirect: &str) -> Result<(String, String), String> {
        let body = json!({
            "clientName": "Oak-Desktop",
            "clientType": "public",
            "scopes": [SCOPE],
            "grantTypes": ["authorization_code", "refresh_token"],
            "redirectUris": [redirect],
            "issuerUrl": format!("https://identitycenter.amazonaws.com/{IDENTITY_STORE}"),
        });
        let resp = api::http()
            .post(OIDC_REGISTER)
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(api::network_error)?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("Quick's identity store refused Oak's client ({status}): {}", text.chars().take(300).collect::<String>()));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("The registration was unreadable: {e}"))?;
        let id = v["clientId"].as_str().unwrap_or("").to_string();
        let secret = v["clientSecret"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            return Err("The registration came back without a client id.".into());
        }
        Ok((id, secret))
    }

    /// Waits for the browser's redirect with the code (and the workspace's
    /// host), answering the page.
    fn wait_for_code((listener, _port): (std::net::TcpListener, u16), expect: &str) -> Result<(String, String), String> {
        use std::io::{BufRead, BufReader, Write};
        listener.set_nonblocking(false).ok();
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        for stream in listener.incoming() {
            if std::time::Instant::now() > deadline {
                return Err("The sign-in timed out: try again.".into());
            }
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let Some(target) = line.split_whitespace().nth(1) else { continue };
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            if !path.starts_with("/callback") && path != "/" {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                continue;
            }
            let q = crate::quick::query(query);
            if let Some(err) = q.get("error") {
                let why = q.get("error_description").cloned().unwrap_or_else(|| err.clone());
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
                return Err(format!("Quick's sign-in was refused: {why}"));
            }
            let Some(state) = q.get("state") else { continue };
            if state != expect {
                continue;
            }
            let Some(code) = q.get("code").cloned() else { continue };
            let host = q.get("tenantUrl").map(|u| u.trim_start_matches("https://").trim_end_matches('/').to_string()).unwrap_or_default();
            let page = "<!doctype html><meta charset=utf-8><title>Oak</title><body style=\"font:16px system-ui;padding:40px\">Signed in to AWS Quick. You can close this tab.</body>";
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
            return Ok((code, host));
        }
        Err("The sign-in listener closed before the browser came back.".into())
    }

    /// Exchanges the code for the tokens, and asks Quick who signed in.
    fn exchange(client: &(String, String), code: &str, redirect: &str, verifier: &str) -> Result<(), String> {
        let body = json!({
            "clientId": client.0,
            "clientSecret": client.1,
            "grantType": "authorization_code",
            "code": code,
            "redirectUri": redirect,
            "codeVerifier": verifier,
        });
        let resp = api::http()
            .post(OIDC_TOKEN)
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(api::network_error)?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("Quick refused the sign-in ({status}): {}", text.chars().take(300).collect::<String>()));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("The sign-in's answer was unreadable: {e}"))?;
        let access = v["accessToken"].as_str().unwrap_or("").to_string();
        let refresh = v["refreshToken"].as_str().unwrap_or("").to_string();
        let ttl = v["expiresIn"].as_u64().unwrap_or(3600) as i64;
        if refresh.is_empty() {
            return Err("Quick's sign-in gave no refresh token, so it couldn't be kept.".into());
        }
        let who = whoami(&access).unwrap_or_default();
        let mut s = STORE.lock().unwrap();
        s.access = access;
        s.refresh = refresh;
        s.expires_at = crate::api::now_ms() + ttl * 1000;
        if !who.is_empty() {
            s.email = who;
        }
        Ok(())
    }

    /// Who the access token belongs to (best effort, for Home).
    fn whoami(access: &str) -> Option<String> {
        let resp = api::http()
            .get(format!("https://{}/integration/quick-work/user-info", host_or_default()?))
            .bearer_auth(access)
            .timeout(Duration::from_secs(20))
            .send()
            .ok()?;
        let v: Value = serde_json::from_str(&resp.text().ok()?).ok()?;
        v["email"].as_str().or(v["user"]["email"].as_str()).map(String::from)
    }

    fn host_or_default() -> Option<String> {
        let host = STORE.lock().unwrap().host.clone();
        (!host.is_empty()).then_some(host)
    }
}

/// `key=value` pairs of a URL's query, percent-decoded.
pub fn query(q: &str) -> std::collections::HashMap<String, String> {
    q.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((urldecode(k), urldecode(v)))
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------- the account

/// A Quick login as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub email: String,
    /// The refresh token, shortened.
    pub label: String,
    pub error: Option<String>,
}

pub fn account() -> Option<Account> {
    let s = login::stored();
    if s.refresh.is_empty() {
        return None;
    }
    Some(Account {
        email: s.email.clone(),
        label: provider::short_key(&s.refresh),
        error: (!s.error.is_empty()).then(|| s.error.clone()),
    })
}

/// A Quick login is set.
pub fn on() -> bool {
    login::on()
}

// The login is Oak's own OIDC client (`login`), so nothing here goes on a
// vault: there is no `sync`.

// ---------------------------------------------------------------- the frames

/// One frame, in Quick's envelope: the body is JSON, base64'd inside a
/// `rawInputEvent`.
fn envelope(v: &Value) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(v.to_string());
    json!({"eventType": "rawInputEvent", "payload": {"payload": body}}).to_string()
}

/// The bodies of the AWS event-stream messages in `buf`: each message is a
/// length-prefixed record whose body is the JSON Quick sent (its own event
/// wrapper, the same shape the app puts on the wire). A binary WebSocket
/// frame holds one or more of them.
pub fn event_bodies(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    let mut i = 0usize;
    while i + 12 <= buf.len() {
        let total = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let hlen = u32::from_be_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]) as usize;
        if total < 16 || hlen > total || i + total > buf.len() {
            break;
        }
        let body = &buf[i + 12 + hlen..i + total - 4];
        if !body.is_empty() {
            out.push(body.to_vec());
        }
        i += total;
    }
    out
}


/// A server frame's JSON: Quick's own event wrapper, whose `payload` is the
/// base64 body ([`crate::quick::event_bodies`]).
pub fn unwrap_frame(payload: &[u8]) -> Option<Value> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    let inner = v.get("payload")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(inner).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------- the request

/// The `user_message` frame for Oak request `r` on thread `thread`: the
/// newest user turn, since Quick keeps the thread's history.
fn user_message(r: &Request, thread: &str, m: &Model) -> Value {
    let text = newest_user_text(r);
    let trace = hex_uuid();
    json!({
        "type": "user_message",
        "payload": {
            "traceId": trace,
            "text": text,
            "send_origin": "composer",
            "had_physical_input": true,
            "surfaceType": SURFACE,
            "appVersion": APP_VERSION,
            "activityType": "chat",
            "surface": "desktop",
            "clientVersion": APP_VERSION,
            "cohort": "general_availability",
            "message_id": format!("msg-{}", &hex_uuid().replace('-', "")[..8]),
            "thread_id": thread,
            "agent_mode": m.id,
            "thinking_effort": effort(r, m),
            "user_timezone": "America/Phoenix",
            "user_locale": "en",
            "chat_agent_id": CHAT_AGENT,
            "auto_routing": false,
            "web_search_enabled": false,
            "direct_chat": false,
            "voice_mode": false,
        }
    })
}

/// The conversation's newest user text: what the user typed, past Oak's own
/// additions (`_oak_file` blocks, notes), or the last request's instruction
/// when there is none (a summary).
fn newest_user_text(r: &Request) -> String {
    for m in r.messages.iter().rev() {
        if m["role"] != "user" {
            continue;
        }
        let Some(blocks) = m["content"].as_array() else { continue };
        let mut out = String::new();
        for b in blocks {
            if b["type"] == "text" && b.get("_oak_file").is_none() {
                out.push_str(b["text"].as_str().unwrap_or(""));
                out.push('\n');
            }
        }
        let out = out.trim();
        if !out.is_empty() {
            return out.to_string();
        }
    }
    r.instruction.clone().unwrap_or_else(|| "Continue.".into())
}

fn hex_uuid() -> String {
    let b: [u8; 16] = rand::random();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6] & 0x0f, b[7], (b[8] & 0x3f) | 0x80, b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

// ---------------------------------------------------------------- the turn

/// What a turn has collected: the blocks by index, and the stop reason.
#[derive(Default)]
pub(crate) struct Turn {
    /// The assistant message's blocks, in order, with how the stream has
    /// filled each.
    blocks: Vec<Value>,
    /// Where the text block is (the last one opened).
    text_at: Option<usize>,
    thinking_at: Option<usize>,
    pub stop: String,
    pub usage: (u64, u64, u64),
    pub model: String,
    /// A tool Oak must run: (call id, name, input).
    pub calls: Vec<(String, String, Value)>,
    started: bool,
    /// The thread (`thread_id`) and assistant message (`message_id`) this
    /// turn is reading. Quick's socket carries other threads' frames too, and
    /// a thread keeps several messages, so frames of another thread (or of an
    /// older message) are not this turn's.
    thread: String,
    message: String,
}

impl Turn {
    /// A turn reading thread `thread`'s message `message` (Quick assigns the
    /// assistant's `message_id` when it starts the reply; until then the turn
    /// takes the first one the thread sends).
    pub(crate) fn for_message(thread: &str, message: &str) -> Turn {
        Turn { thread: thread.to_string(), message: message.to_string(), ..Default::default() }
    }

    /// Whether `v` is this turn's: its thread's (when it names one), and
    /// its message's (when it names one, and once the turn knows it). Other
    /// threads' frames and older messages' frames are not. The turn's driver
    /// asks before it reads a frame, so a tool call of another thread is
    /// never run.
    pub(crate) fn accepts(&mut self, v: &Value) -> bool {
        let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let thread = field("thread_id");
        if !thread.is_empty() && thread != self.thread {
            return false;
        }
        let message = field("message_id");
        if message.is_empty() {
            return true;
        }
        if self.message.is_empty() {
            self.message = message;
            return true;
        }
        message == self.message
    }

    fn text(&mut self) -> usize {
        // One text block for the whole message: Quick streams text around
        // (and between) its tool calls, and the transcript reads better as
        // one reply than as an interleaving of many.
        if let Some(i) = self.text_at {
            return i;
        }
        let i = self.blocks.len();
        self.blocks.push(json!({"type": "text", "text": ""}));
        self.text_at = Some(i);
        i
    }

    /// The stream's next frame, as Oak's events. `blocks` collects them, and
    /// `None` when a frame changes nothing Oak shows.
    pub(crate) fn feed(&mut self, v: &Value) -> Vec<Event> {
        if !self.accepts(v) {
            return vec![];
        }
        let kind = v["type"].as_str().unwrap_or("");
        let p = &v["payload"];
        let mut out = vec![];
        match kind {
            "message_start" => {
                self.started = true;
            }
            "thinking_start" => {
                let i = self.blocks.len();
                self.blocks.push(json!({"type": "reasoning", "text": "", "by": "quick"}));
                self.thinking_at = Some(i);
                out.push(Event::Block { index: i, block: self.blocks[i].clone() });
            }
            "thinking_delta" => {
                let text = p["delta"].as_str().unwrap_or("");
                if !text.is_empty()
                    && let Some(i) = self.thinking_at
                {
                    if let Some(t) = self.blocks[i]["text"].as_str().map(str::to_string) {
                        self.blocks[i]["text"] = json!(format!("{t}{text}"));
                    }
                    out.push(Event::Reasoning { index: i, text: text.to_string() });
                }
            }
            "thinking_complete" => {
                if let Some(i) = self.thinking_at.take() {
                    out.push(Event::BlockEnd { index: i });
                }
            }
            "text_delta" => {
                let text = p["delta"].as_str().unwrap_or("");
                if !text.is_empty() {
                    let i = self.text();
                    if let Some(t) = self.blocks[i]["text"].as_str().map(str::to_string) {
                        self.blocks[i]["text"] = json!(format!("{t}{text}"));
                    }
                    out.push(Event::Text { index: i, text: text.to_string() });
                }
            }
            "text_complete" | "message_complete" => {
                let text = p["text"].as_str().unwrap_or("");
                if let Some(i) = self.text_at
                    && let Some(seen) = self.blocks[i]["text"].as_str().map(str::to_string)
                    && seen.is_empty()
                    && !text.is_empty()
                {
                    self.blocks[i]["text"] = json!(text);
                    out.push(Event::Text { index: i, text: text.to_string() });
                }
                if kind == "message_complete" {
                    // The turn is over: Quick ran its own loop and Oak ran
                    // its tools as it asked, so nothing is left to do.
                    self.stop = stop::COMPLETE.into();
                    out.push(Event::Stop { reason: self.stop.clone(), detail: None });
                }
            }
            "tool_call" => {
                let id = p["tool_call_id"].as_str().unwrap_or("").to_string();
                let name = p["tool_name"].as_str().unwrap_or("").to_string();
                // Quick's arguments nest the tool's own under `params.arguments`.
                let args = p["arguments"]["params"]["arguments"].clone();
                let input = if args.is_null() { p["arguments"].clone() } else { args };
                if !name.is_empty() {
                    let i = self.blocks.len();
                    self.blocks.push(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
                    out.push(Event::Block { index: i, block: self.blocks[i].clone() });
                    out.push(Event::BlockEnd { index: i });
                    self.calls.push((id, name, input));
                }
            }
            "turn_usage" => {
                let n = |k: &str| p[k].as_u64().unwrap_or(0);
                self.usage = (n("input_tokens") + n("cache_read_tokens") + n("cache_write_tokens"), n("cache_read_tokens"), n("output_tokens"));
                if let Some(m) = p["resolved_model_id"].as_str().filter(|m| !m.is_empty()) {
                    self.model = m.to_string();
                }
                out.push(Event::Usage(Usage {
                    input: n("input_tokens"),
                    cache_read: n("cache_read_tokens"),
                    write: n("cache_write_tokens"),
                    output: n("output_tokens"),
                    ..Default::default()
                }));
            }
            "title_generated" => {
                // Quick names the thread it just made; Oak's own title
                // follows from the conversation, so nothing is shown here.
            }
            _ => {}
        }
        out
    }

    /// The text block is what the user reads: a call's output goes into
    /// the transcript as a result, and a message from Quick ends the turn.
    pub(crate) fn blocks(&self) -> Vec<Value> {
        self.blocks.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_effort() {
        assert!(serves("smart") && serves("fast") && serves("balanced") && !serves("claude-opus-5-5"));
        assert_eq!(crate::providers::Kind::of("smart"), crate::providers::Kind::Quick);
        let m = model("smart").unwrap();
        let mut r = Request { provider: crate::providers::Kind::Quick, model: "smart".into(), ..Default::default() };
        r.effort = Some("high".into());
        assert_eq!(effort(&r, m), "high");
        r.effort = Some("ultra".into());
        assert_eq!(effort(&r, m), "max");
        r.purpose = oak::Purpose::Title;
        assert_eq!(effort(&r, m), "off");
    }

    #[test]
    fn the_envelope_round_trips() {
        let v = json!({"type": "session_connect", "payload": {"session_id": "x"}});
        let wire = envelope(&v);
        let outer: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(outer["eventType"], "rawInputEvent");
        let body = outer["payload"]["payload"].as_str().unwrap();
        let back: Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(body).unwrap()).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn the_newest_user_turn_is_what_goes() {
        let r = Request {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "old"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
                json!({"role": "user", "content": [{"type": "text", "text": "file.rs", "_oak_file": "file.rs"}, {"type": "text", "text": "new"}]}),
            ],
            ..Default::default()
        };
        assert_eq!(newest_user_text(&r), "new");
        let empty = Request { instruction: Some("Summarize.".into()), ..Default::default() };
        assert_eq!(newest_user_text(&empty), "Summarize.");
    }

    #[test]
    fn another_threads_frames_are_not_the_turns() {
        let mut t = Turn::for_message("thread-a", "");
        // Another thread's text is another conversation's reply.
        let other = json!({"type": "text_delta", "payload": {"delta": "not mine"},
            "thread_id": "thread-b", "message_id": "msg-b1"});
        assert!(t.feed(&other).is_empty());
        assert!(t.accepts(&other) == false);
        // This thread's first message claims the turn.
        let first = json!({"type": "text_delta", "payload": {"delta": "mine"},
            "thread_id": "thread-a", "message_id": "msg-a1"});
        assert!(t.accepts(&first));
        assert_eq!(t.feed(&first).len(), 1);
        // An older message of the same thread is not.
        let older = json!({"type": "text_delta", "payload": {"delta": "old"},
            "thread_id": "thread-a", "message_id": "msg-a0"});
        assert!(!t.accepts(&older));
        // Nor is another thread's tool call (Oak must not run it).
        let call = json!({"type": "tool_call", "payload": {"tool_name": "run_python", "tool_call_id": "c9"},
            "thread_id": "thread-b"});
        assert!(!t.accepts(&call));
        assert_eq!(t.blocks()[0]["text"], "mine");
    }

    #[test]
    fn frames_become_oaks_events() {
        let mut t = Turn::for_message("th", "");
        let mut ev = vec![];
        for f in [
            json!({"type": "message_start", "payload": {"role": "assistant"}}),
            json!({"type": "thinking_delta", "payload": {"delta": "hm"}}),
            json!({"type": "text_delta", "payload": {"delta": "Hi"}}),
            json!({"type": "tool_call", "payload": {"tool_name": "EXECUTE", "tool_call_id": "c1",
                "arguments": {"params": {"name": "EXECUTE", "arguments": {"command": "ls"}}}}}),
            json!({"type": "message_complete", "payload": {"text": "Hi"}}),
        ] {
            ev.extend(t.feed(&f));
        }
        assert!(t.started);
        assert_eq!(t.calls.len(), 1);
        assert_eq!(t.calls[0].1, "EXECUTE");
        assert_eq!(t.calls[0].2["command"], "ls");
        assert_eq!(t.stop, stop::COMPLETE);
        assert!(ev.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Hi")));
        let blocks = t.blocks();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks.iter().find(|b| b["type"] == "tool_call").unwrap()["input"]["command"], "ls");
    }
}
