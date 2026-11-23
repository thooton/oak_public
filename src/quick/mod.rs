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

pub mod outpost;
pub mod socket;
pub mod turn;
pub mod usage;
#[cfg(test)]
mod live_test;
#[cfg(test)]
mod models_test;
#[cfg(test)]
mod outpost_test;

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

/// Quick's desktop app for this workspace, whose outpost id an older Oak
/// saved before it registered its own (`outpost::id` replaces it).
pub const DEFAULT_OUTPOST_ID: &str = "8cd71ba4-b3e6-4dba-869e-5f364e1ccf71";

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
        /// This machine's Quick outpost: tools Quick asks for are routed to
        /// the machine that owns it, and Oak runs them.
        #[serde(default)]
        pub outpost: String,
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

    /// Saves this machine's own outpost id (`outpost::id`).
    pub fn set_outpost(id: &str) {
        let mut s = STORE.lock().unwrap();
        s.outpost = id.to_string();
        drop(s);
        save();
    }

    /// The outpost Quick routes this machine's tools to, saved from the
    /// sign-in's callback (`outpostId`) or `OAK_QUICK_OUTPOST`.
    pub fn saved_outpost() -> String {
        std::env::var("OAK_QUICK_OUTPOST").ok().map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).unwrap_or_else(|| STORE.lock().unwrap().outpost.clone())
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
                    Ok((code, host, outpost)) => match exchange(&client2, &code, &redirect, &verifier2) {
                        Ok(()) => {
                            let mut s = STORE.lock().unwrap();
                            if !host.is_empty() {
                                s.host = host;
                            }
                            if !outpost.is_empty() {
                                s.outpost = outpost;
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
    fn wait_for_code((listener, _port): (std::net::TcpListener, u16), expect: &str) -> Result<(String, String, String), String> {
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
            let outpost = q.get("outpostId").cloned().unwrap_or_default();
            let page = "<!doctype html><meta charset=utf-8><title>Oak</title><body style=\"font:16px system-ui;padding:40px\">Signed in to AWS Quick. You can close this tab.</body>";
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
            return Ok((code, host, outpost));
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
    /// Its limits as `user-info` last read them (`usage`): the month's
    /// units and the session window.
    pub usage: Option<usage::Usage>,
    /// Why the last read of them failed, and when the reading was taken (ms).
    pub usage_error: Option<String>,
    pub usage_at: i64,
}

pub fn account() -> Option<Account> {
    let s = login::stored();
    if s.refresh.is_empty() {
        return None;
    }
    let (usage, usage_error, usage_at) = usage::last();
    Some(Account {
        email: s.email.clone(),
        label: provider::short_key(&s.refresh),
        error: (!s.error.is_empty()).then(|| s.error.clone()),
        usage,
        usage_error,
        usage_at,
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
fn user_message(r: &Request, thread: &str, m: &Model, outpost: Option<&str>) -> Value {
    let text = newest_user_text(r);
    let trace = hex_uuid();
    let mut payload = json!({
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
    });
    // The outpost names this machine: with it Quick routes the tools it
    // needs to run here, and Oak runs them; without it Quick answers from
    // its own empty sandbox.
    if let Some(outpost) = outpost {
        payload["outpostId"] = json!(outpost);
    }
    json!({"type": "user_message", "payload": payload})
}

/// What the user said since Quick last replied: the text of the user turn
/// (or turns) after the newest assistant message, past Oak's own additions
/// (`_oak_file` blocks). Quick keeps the thread's history, so this is all it
/// is sent. Only that part of the transcript is looked at: when Oak asks
/// again in the middle of a turn (the stream dropped, a pause or a usage limit
/// cut it, a retry), the newest user turn holds only the tool results, and
/// reading on back to an older user turn would send the user's earlier
/// message again, which Quick's agent then takes for the same message sent
/// twice. With nothing new it is told to carry on instead. A request with an
/// instruction of its own (a summary) sends that.
fn newest_user_text(r: &Request) -> String {
    if let Some(i) = r.instruction.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
        return i.to_string();
    }
    match new_user_text(r) {
        Some(t) => t,
        None if r.messages.iter().any(|m| m["role"] == "assistant") => CARRY_ON.into(),
        None => "Continue.".into(),
    }
}

/// The text of the user turns after the newest assistant message, past
/// Oak's own additions (`_oak_file` blocks) and the messages Quick already
/// took in mid-turn (`STEERED_KEY`): None when there is none.
pub(crate) fn new_user_text(r: &Request) -> Option<String> {
    let mut parts = vec![];
    for m in r.messages.iter().rev() {
        if m["role"] == "assistant" {
            break;
        }
        if m["role"] != "user" {
            continue;
        }
        let Some(blocks) = m["content"].as_array() else { continue };
        let said = |b: &&Value| b["type"] == "text" && b.get("_oak_file").is_none() && b.get(STEERED_KEY).is_none();
        let text: Vec<&str> = blocks.iter().filter(said).filter_map(|b| b["text"].as_str()).collect();
        parts.push(text.join("\n"));
    }
    parts.reverse();
    let out = parts.join("\n");
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// Marks a user message Quick took in while its turn ran (`steering_ingested`):
/// it is in Quick's history already, so no request sends it again.
pub(crate) const STEERED_KEY: &str = "_oak_steered";

/// The `user_message` frame for `text`, a message the user typed while the
/// turn on `thread` runs, sent as Quick's app sends one: `mid_turn`, with a
/// `message_id` of its own (`id`), which Quick names when its agent takes the
/// message in at its next step (`steering_ingested`).
pub(crate) fn steer_message(r: &Request, text: &str, id: &str, thread: &str, m: &Model, outpost: Option<&str>) -> Value {
    let mut said = r.clone();
    said.messages = vec![json!({"role": "user", "content": [{"type": "text", "text": text}]})];
    said.instruction = None;
    let mut v = user_message(&said, thread, m, outpost);
    v["payload"]["mid_turn"] = json!(true);
    v["payload"]["message_id"] = json!(id);
    v
}

/// What Quick is sent when Oak asks again in the middle of a turn with
/// nothing new from the user (only the results of the calls Oak ran).
pub(crate) const CARRY_ON: &str = "(From Oak, the device that runs your tools.) The connection to you was cut in the middle of your turn. There is no new message from me: carry on with what I asked from where you stopped, without starting over or repeating tool calls that already ran.";

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
    /// The text block was announced to Oak (`Event::Block`), so its deltas
    /// have a block to land in.
    text_open: bool,
    /// Oak has been given tool results and is waiting for Quick to carry the
    /// turn on: the next `message_start` of this thread is still this turn.
    awaiting_continuation: bool,
    /// The thread (`thread_id`) and assistant message (`message_id`) this
    /// turn is reading. Quick's socket carries other threads' frames too, and
    /// a thread keeps several messages, so frames of another thread (or of an
    /// older message) are not this turn's.
    thread: String,
    message: String,
    /// Where the text block's current section began (its bytes before it are
    /// sections Quick has already given whole), and whether Quick has given
    /// one whole in this message (`text_complete`).
    section_at: usize,
    section_given: bool,
    /// The message's thinking Quick has given whole so far: each
    /// `thinking_complete` carries all of it, so its newest thought is what
    /// follows this.
    thinking_given: String,
    /// The text blocks this message has opened: Quick's whole text for the
    /// message stands for its deltas only when there was one.
    message_texts: usize,
    /// Where this message's blocks begin (`blocks` before it are an earlier
    /// message's of the turn): Quick's whole text can run the message's
    /// earlier text blocks in at its start.
    message_from: usize,
    /// The calls of the turn by Quick's report of them (`tool_start`) and
    /// by the calls Oak runs (`tool_call`): one block for each, whoever runs
    /// it.
    quick_calls: Vec<QuickCall>,
    /// The text this message wrote in the replies Oak read before this one
    /// (Quick took in a message the user typed and its message went on in a
    /// new reply, `resume`): Quick's whole text can run it in too.
    earlier: Vec<String>,
}

/// A call in the turn: one Quick's agent made (`tool_start`), one Oak runs
/// (`tool_call`), or both, since Quick reports the calls it hands Oak too.
#[derive(Default, Clone)]
struct QuickCall {
    /// Quick's id for the call (`tool_id`), once Quick has reported it.
    tool_id: String,
    /// The call's block.
    block: usize,
    /// Oak runs it (a `tool_call` came for it): its result is Oak's, not
    /// Quick's record of it.
    device: bool,
    /// Its block has ended.
    done: bool,
}

/// A call's input as Quick gives it: the tool's own arguments, which Quick
/// nests under `params.arguments` when it hands a call to Oak.
fn quick_input(p: &Value) -> Value {
    let nested = &p["arguments"]["params"]["arguments"];
    let input = if nested.is_null() { &p["arguments"] } else { nested };
    if input.is_null() { json!({}) } else { input.clone() }
}

/// Whether `quick` (a tool as Quick's agent called it) and `device` (the
/// name its call to Oak came with) are one tool: the device's Python is
/// `run_python_on_device` to the agent and `run_python_outpost_local` to
/// Oak, and a device's own tools carry its name before theirs.
fn same_tool(quick: &str, device: &str) -> bool {
    let plain = |n: &str| n.replace("run_python_on_device", "run_python_outpost_local");
    let (q, d) = (plain(quick), plain(device));
    q == d || q.ends_with(&format!("_{d}"))
}

/// What Quick's record of a call (a `tool_result`) says it returned, or
/// why it failed.
fn quick_result(p: &Value) -> Result<String, String> {
    let said = match &p["result"] {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let error = match &p["error"] {
        Value::Null | Value::Bool(false) => None,
        Value::String(e) if e.trim().is_empty() => None,
        Value::String(e) => Some(e.clone()),
        e => Some(e.to_string()),
    };
    match error {
        Some(e) => Err(e),
        None if p["status"].as_str() == Some("error") || p["is_error"].as_bool() == Some(true) => Err(said),
        None => Ok(said),
    }
}

impl Turn {
    /// A turn reading thread `thread`'s message `message` (Quick assigns the
    /// assistant's `message_id` when it starts the reply; until then the turn
    /// takes the first one the thread sends).
    pub(crate) fn for_message(thread: &str, message: &str) -> Turn {
        Turn { thread: thread.to_string(), message: message.to_string(), ..Default::default() }
    }

    /// Whether `v` is this turn's. Other threads' frames never are. Joining
    /// a thread makes Quick replay its older messages, so a turn begins only
    /// at a `message_start` — the assistant message Quick opens for it — and
    /// nothing of another message is taken, thinking included.
    pub(crate) fn accepts(&mut self, v: &Value) -> bool {
        let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let thread = field("thread_id");
        if !thread.is_empty() && thread != self.thread {
            return false;
        }
        let message = field("message_id");
        if message.is_empty() {
            // What names no message (keepalives, the thread's title, status).
            return true;
        }
        match self.message.as_str() {
            // Not begun: only a start opens this turn.
            "" => {
                let k = v["type"].as_str().unwrap_or("");
                let opens = matches!(k, "message_start");
                if opens {
                    self.message = message;
                }
                opens
            }
            mine if message == mine => true,
            // After its tool results Quick carries the turn on in a fresh
            // assistant message: that is this turn's too, and its text is its
            // own block.
            _ => {
                let k = v["type"].as_str().unwrap_or("");
                if k == "message_start" && self.awaiting_continuation {
                    self.message = message;
                    self.awaiting_continuation = false;
                    self.text_at = None;
                    self.text_open = false;
                    self.section_at = 0;
                    self.section_given = false;
                    self.thinking_at = None;
                    self.thinking_given.clear();
                    self.message_texts = 0;
                    self.message_from = self.blocks.len();
                    self.earlier.clear();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// The block new text goes in: the text block while nothing has come
    /// after it, else a new one at the end. Quick sends a whole agent loop
    /// (thoughts, text, calls, more thoughts, the answer) as one message,
    /// and the reply grows at its end, as every other provider's does: text
    /// after a thought or a call is a block below it, never more of the text
    /// above.
    fn text(&mut self) -> usize {
        if let Some(i) = self.text_at
            && i + 1 == self.blocks.len()
        {
            return i;
        }
        let i = self.blocks.len();
        self.blocks.push(json!({"type": "text", "text": ""}));
        self.text_at = Some(i);
        self.text_open = false;
        self.section_at = 0;
        self.message_texts += 1;
        i
    }

    /// The text block new text goes in, opened in Oak, so its `Text` events
    /// have somewhere to land. Oak only fills blocks it was told about
    /// (`reply` drops text for an index it never saw), so a block's `Block`
    /// comes with its first text, and once. A text block something came
    /// after is done: it ends as the next one opens.
    fn open_text(&mut self, out: &mut Vec<Event>) -> usize {
        if let Some(i) = self.text_at.filter(|&i| self.text_open && i + 1 == self.blocks.len()) {
            return i;
        }
        if let Some(i) = self.text_at.filter(|_| self.text_open) {
            out.push(Event::BlockEnd { index: i });
        }
        let i = self.text();
        self.text_open = true;
        out.push(Event::Block { index: i, block: self.blocks[i].clone() });
        i
    }

    /// A new thought at the end of the reply (the one before, if open, ends).
    fn open_thinking(&mut self, out: &mut Vec<Event>) -> usize {
        if let Some(i) = self.thinking_at.take() {
            out.push(Event::BlockEnd { index: i });
        }
        let i = self.blocks.len();
        self.blocks.push(json!({"type": "reasoning", "text": "", "by": "quick"}));
        self.thinking_at = Some(i);
        out.push(Event::Block { index: i, block: self.blocks[i].clone() });
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
                self.open_thinking(&mut out);
            }
            "thinking_delta" => {
                let text = p["delta"].as_str().unwrap_or("");
                if !text.is_empty() {
                    // Quick opens only a message's first thought; each later
                    // one comes as deltas alone, and is a block of its own.
                    let i = match self.thinking_at {
                        Some(i) => i,
                        None => self.open_thinking(&mut out),
                    };
                    if let Some(t) = self.blocks[i]["text"].as_str().map(str::to_string) {
                        self.blocks[i]["text"] = json!(format!("{t}{text}"));
                    }
                    out.push(Event::Reasoning { index: i, text: text.to_string() });
                }
            }
            "thinking_complete" => {
                // Quick gives all of the message's thinking so far: what
                // follows the last of it is this thought whole, which stands
                // for its deltas (some may never have come, or none did).
                let total = p["thinking"].as_str().unwrap_or("").to_string();
                let newest = total.strip_prefix(self.thinking_given.as_str()).unwrap_or(&total).trim().to_string();
                if !total.is_empty() {
                    self.thinking_given = total.clone();
                }
                if self.thinking_at.is_none() && !newest.is_empty() {
                    self.open_thinking(&mut out);
                }
                if let Some(i) = self.thinking_at.take() {
                    let had = self.blocks[i]["text"].as_str().unwrap_or("").trim().to_string();
                    if !newest.is_empty() && had != newest {
                        self.blocks[i]["text"] = json!(newest);
                        out.push(Event::Block { index: i, block: self.blocks[i].clone() });
                    }
                    out.push(Event::BlockEnd { index: i });
                }
            }
            "text_delta" => {
                let text = p["delta"].as_str().unwrap_or("");
                if !text.is_empty() {
                    let i = self.open_text(&mut out);
                    if let Some(t) = self.blocks[i]["text"].as_str().map(str::to_string) {
                        self.blocks[i]["text"] = json!(format!("{t}{text}"));
                    }
                    out.push(Event::Text { index: i, text: text.to_string() });
                }
            }
            "text_complete" | "message_complete" => {
                // Quick gives the text whole when it ends (a section, then
                // the message): that is what it wrote, whatever the deltas
                // made of it, so it replaces them.
                let text = p["text"].as_str().unwrap_or("");
                let section = kind == "text_complete";
                if !text.is_empty() && (section || (!self.section_given && self.message_texts <= 1)) {
                    self.settle_text(text, section, &mut out);
                }
                if kind == "message_complete" {
                    // The turn is over: Quick ran its own loop and Oak ran
                    // its tools as it asked, so nothing is left to do.
                    out.extend(self.close(stop::COMPLETE));
                }
            }
            // Quick's report of a call its agent made: shown as it is made,
            // whether Quick runs it on its server or hands it to Oak (then
            // the call Oak runs is this block, not one more).
            "tool_start" => {
                let id = p["tool_id"].as_str().or(p["tool_call_id"].as_str()).unwrap_or("").to_string();
                let name = p["tool_name"].as_str().unwrap_or("").to_string();
                let input = quick_input(p);
                if !id.is_empty() && !name.is_empty() && !self.quick_calls.iter().any(|c| c.tool_id == id) {
                    match self.unreported_device_call(&name, &input) {
                        Some(k) => self.quick_calls[k].tool_id = id,
                        None => {
                            if let Some(t) = self.thinking_at.take() {
                                out.push(Event::BlockEnd { index: t });
                            }
                            let i = self.blocks.len();
                            self.blocks.push(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
                            out.push(Event::Block { index: i, block: self.blocks[i].clone() });
                            self.quick_calls.push(QuickCall { tool_id: id, block: i, ..Default::default() });
                        }
                    }
                }
            }
            "tool_arguments" => {
                let id = p["tool_id"].as_str().unwrap_or("");
                let input = quick_input(p);
                if let Some(c) = self.quick_calls.iter().find(|c| !id.is_empty() && c.tool_id == id && !c.device && !c.done)
                    && input.as_object().is_some_and(|o| !o.is_empty())
                    && self.blocks[c.block]["input"] != input
                {
                    let i = c.block;
                    self.blocks[i]["input"] = input;
                    out.push(Event::Block { index: i, block: self.blocks[i].clone() });
                }
            }
            // What a call Quick ran returned: its block ends with it. Quick
            // records the calls Oak ran too; those end with Oak's result.
            "tool_result" => {
                let id = ["tool_id", "tool_call_id", "tool_use_id", "id"].iter().find_map(|k| p[*k].as_str()).unwrap_or("");
                if let Some(k) = self.quick_calls.iter().position(|c| !id.is_empty() && c.tool_id == id && !c.device && !c.done) {
                    let input = quick_input(p);
                    let i = self.quick_calls[k].block;
                    if input.as_object().is_some_and(|o| !o.is_empty()) {
                        self.blocks[i]["input"] = input;
                    }
                    self.quick_calls[k].done = true;
                    let call = self.blocks[i]["id"].as_str().unwrap_or("").to_string();
                    out.extend(self.end_call(i, &call, &quick_result(p)));
                }
            }
            "tool_call" => {
                let id = p["tool_call_id"].as_str().unwrap_or("").to_string();
                let name = p["tool_name"].as_str().unwrap_or("").to_string();
                let input = quick_input(p);
                if !name.is_empty() {
                    // The block ends once Oak has run the call
                    // (`finished_call`), with its result: a call that ended
                    // here would go to Oak's own tools, which don't know
                    // Quick's names.
                    // A thought still open is done: the call comes after it.
                    if let Some(t) = self.thinking_at.take() {
                        out.push(Event::BlockEnd { index: t });
                    }
                    let block = json!({"type": "tool_call", "id": id, "name": name, "input": input});
                    // The call Quick reported is this one: its block becomes
                    // the call Oak runs.
                    match self.reported_call(&name, &input) {
                        Some(k) => {
                            self.quick_calls[k].device = true;
                            let i = self.quick_calls[k].block;
                            self.blocks[i] = block;
                            out.push(Event::Block { index: i, block: self.blocks[i].clone() });
                        }
                        None => {
                            let i = self.blocks.len();
                            self.blocks.push(block);
                            out.push(Event::Block { index: i, block: self.blocks[i].clone() });
                            self.quick_calls.push(QuickCall { block: i, device: true, ..Default::default() });
                        }
                    }
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

    /// Quick's whole text for the current section (`section`) or message:
    /// the block's text from where the section began becomes it, and Oak is
    /// told (a `Block` with the text, which replaces the one it has).
    fn settle_text(&mut self, text: &str, section: bool, out: &mut Vec<Event>) {
        // Whole text that comes once a thought or a call has begun is still
        // the text block's above it while that block's section is open (its
        // deltas came, its whole text not yet); otherwise it is new text, at
        // the end.
        let late = self.text_at.filter(|&i| {
            self.text_open && i + 1 < self.blocks.len() && self.blocks[i]["text"].as_str().is_some_and(|t| t.len() > self.section_at)
        });
        let i = match late {
            Some(i) => i,
            None => self.open_text(out),
        };
        let text = self.own_text(text, i);
        if text.trim().is_empty() {
            return;
        }
        let had = self.blocks[i]["text"].as_str().unwrap_or("").to_string();
        let start = if section { self.section_at.min(had.len()) } else { 0 };
        let kept = had.get(..start).unwrap_or("");
        // A section's whole text that already holds the block's sections
        // before it is the block's whole text.
        let want = if !kept.is_empty() && text.starts_with(kept) { text.to_string() } else { format!("{kept}{text}") };
        if want != had {
            if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                eprintln!("TEXT the deltas made {} bytes of a section Quick wrote as {}; Quick's is kept", had.len().saturating_sub(start), text.len());
            }
            self.blocks[i]["text"] = json!(want);
            out.push(Event::Block { index: i, block: self.blocks[i].clone() });
        }
        if section {
            self.section_at = want.len();
            self.section_given = true;
        }
    }

    /// The call Oak runs Quick reported first (`tool_start`) with no call to
    /// Oak yet for it: the one of `name` with this input, else the first of
    /// `name`.
    fn reported_call(&self, name: &str, input: &Value) -> Option<usize> {
        let open = |c: &QuickCall| !c.device && !c.done && same_tool(self.blocks[c.block]["name"].as_str().unwrap_or(""), name);
        let same = self.quick_calls.iter().position(|c| open(c) && self.blocks[c.block]["input"] == *input);
        same.or_else(|| self.quick_calls.iter().position(open))
    }

    /// A call Oak runs that Quick hasn't reported yet, for Quick's report of
    /// a call of `name` (the one with this input, else the first).
    fn unreported_device_call(&self, name: &str, input: &Value) -> Option<usize> {
        let open = |c: &QuickCall| c.device && c.tool_id.is_empty() && same_tool(name, self.blocks[c.block]["name"].as_str().unwrap_or(""));
        let same = self.quick_calls.iter().position(|c| open(c) && self.blocks[c.block]["input"] == *input);
        same.or_else(|| self.quick_calls.iter().position(open))
    }

    /// What of Quick's whole text `text` is block `i`'s own. Quick's whole
    /// text can carry what the message wrote before it: the text of the
    /// message's last text blocks above `i` (all of them, or those since the
    /// section Quick last gave whole), run together, then this block's.
    /// Those blocks have it already, so it is left out, or the answer
    /// repeats what was said between its calls. The longest such run is
    /// taken; text that starts with none is taken as it is.
    fn own_text<'a>(&self, text: &'a str, i: usize) -> &'a str {
        let from = self.message_from.min(i);
        let here = self.blocks[from..i].iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str());
        let above: Vec<&str> = self.earlier.iter().map(String::as_str).chain(here).map(str::trim).filter(|t| !t.is_empty()).collect();
        for k in 0..above.len() {
            let mut rest = text;
            let all = above[k..].iter().all(|t| match rest.trim_start().strip_prefix(t) {
                Some(r) => {
                    rest = r;
                    true
                }
                None => false,
            });
            if all {
                return rest.trim_start();
            }
        }
        text
    }

    /// Oak ran call `id` (`result`: what it returned, or why it failed): its
    /// block ends, with the result, which goes in the transcript as the
    /// call's (the agent doesn't run it again).
    pub(crate) fn finished_call(&mut self, id: &str, result: &Result<String, String>) -> Vec<Event> {
        let Some(i) = self.blocks.iter().rposition(|b| b["type"] == "tool_call" && b["id"] == id) else { return vec![] };
        self.end_call(i, id, result)
    }

    /// Call block `i` (`id`) ends with `result`, which goes in the
    /// transcript as the call's.
    fn end_call(&mut self, i: usize, id: &str, result: &Result<String, String>) -> Vec<Event> {
        let (content, is_error) = match result {
            Ok(s) => (s.clone(), false),
            Err(e) => (e.clone(), true),
        };
        self.blocks[i]["_oak_result"] = oak::tool_result(&json!(id), json!(content), is_error);
        let out = vec![Event::Block { index: i, block: self.blocks[i].clone() }, Event::BlockEnd { index: i }];
        // The transcript's copy of the call has no result in it.
        if let Some(o) = self.blocks[i].as_object_mut() {
            o.remove("_oak_result");
        }
        out
    }

    /// The reply Oak is reading ends here (`stop`): the open thought and
    /// text end, and a call Quick ran and never said the result of ends
    /// saying so (left open, Oak's agent would run it).
    pub(crate) fn close(&mut self, stop: &str) -> Vec<Event> {
        let mut out = vec![];
        self.stop = stop.into();
        if let Some(i) = self.thinking_at.take() {
            out.push(Event::BlockEnd { index: i });
        }
        for k in 0..self.quick_calls.len() {
            let c = self.quick_calls[k].clone();
            if c.device || c.done {
                continue;
            }
            self.quick_calls[k].done = true;
            let call = self.blocks[c.block]["id"].as_str().unwrap_or("").to_string();
            out.extend(self.end_call(c.block, &call, &Ok("Quick didn't say what this call returned.".into())));
        }
        if let Some(i) = self.text_at.filter(|_| self.text_open) {
            out.push(Event::BlockEnd { index: i });
        }
        out.push(Event::Stop { reason: self.stop.clone(), detail: None });
        out
    }

    /// Quick took in a message the user typed meanwhile: the reply Oak read
    /// ended there (`close`), and Quick's message goes on in Oak's next one,
    /// which starts empty. The text the message wrote so far is kept
    /// (`earlier`), so its whole text, which can run that in, repeats none of it.
    pub(crate) fn resume(&mut self) {
        let from = self.message_from.min(self.blocks.len());
        let texts: Vec<String> = self.blocks[from..].iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).map(String::from).collect();
        self.earlier.extend(texts);
        self.blocks.clear();
        self.text_at = None;
        self.thinking_at = None;
        self.text_open = false;
        self.section_at = 0;
        self.message_from = 0;
        self.quick_calls.clear();
        self.calls.clear();
        self.stop.clear();
    }

    /// Oak is giving Quick, in a message of its own, results its relay lost:
    /// Quick answers in a fresh assistant message, and that is this turn's
    /// too. The text read so far is closed, so the answer is a block of its
    /// own.
    pub(crate) fn await_resend(&mut self) -> Vec<Event> {
        let mut out = vec![];
        if let Some(i) = self.text_at.filter(|_| self.text_open) {
            out.push(Event::BlockEnd { index: i });
        }
        self.text_at = None;
        self.text_open = false;
        self.section_at = 0;
        self.section_given = false;
        self.awaiting_continuation = true;
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

    /// Asked again mid-turn (a dropped stream, a pause, a retry), the newest
    /// user turn holds only tool results: the user's earlier message is not
    /// sent again (Quick's agent read it as the same message sent twice).
    #[test]
    fn a_request_mid_turn_does_not_send_the_first_message_again() {
        let r = Request {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "fix the bug"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "ripgrep", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "found it"}]}),
            ],
            ..Default::default()
        };
        assert_eq!(newest_user_text(&r), CARRY_ON);
    }

    /// A message typed while Quick worked joins the user turn after the
    /// tool results: it is what goes, not the message that started the turn.
    #[test]
    fn a_message_sent_mid_turn_is_the_one_that_goes() {
        let r = Request {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "fix the bug"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "ripgrep", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "found it"}, {"type": "text", "text": "also show the server's calls"}]}),
            ],
            ..Default::default()
        };
        assert_eq!(newest_user_text(&r), "also show the server's calls");
    }

    /// A message Quick took in mid-turn is in its history: the request that
    /// reads on after it sends nothing new.
    #[test]
    fn a_message_quick_took_in_mid_turn_is_not_sent_again() {
        let r = Request {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "fix the bug"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "ripgrep", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "found it"}, {"type": "text", "text": "NO TESTING", STEERED_KEY: true}]}),
            ],
            ..Default::default()
        };
        assert_eq!(new_user_text(&r), None);
        assert_eq!(newest_user_text(&r), CARRY_ON);
    }

    /// A message typed mid-turn goes as Quick's app sends one (the capture's
    /// "NO TESTING"): `mid_turn`, with its own message id.
    #[test]
    fn a_mid_turn_message_is_sent_as_quicks_app_sends_it() {
        let m = model("smart").unwrap();
        let v = steer_message(&Request::default(), "NO TESTING", "msg-87fa24c3", "th", m, Some("dev"));
        assert_eq!(v["type"], "user_message");
        assert_eq!(v["payload"]["text"], "NO TESTING");
        assert_eq!(v["payload"]["mid_turn"], true);
        assert_eq!(v["payload"]["message_id"], "msg-87fa24c3");
        assert_eq!(v["payload"]["thread_id"], "th");
        assert_eq!(v["payload"]["outpostId"], "dev");
    }

    /// Quick took in a message: the reply Oak read ends (`STEERED`), the
    /// next starts empty in the same Quick message, and Quick's whole text,
    /// which runs the earlier part in, repeats none of it.
    #[test]
    fn a_message_taken_in_mid_turn_splits_the_reply_and_nothing_repeats() {
        let f = |kind: &str, p: Value| json!({"type": kind, "payload": p, "thread_id": "th", "message_id": "m1"});
        let mut t = Turn::for_message("th", "");
        t.feed(&f("message_start", json!({"role": "assistant"})));
        t.feed(&f("text_delta", json!({"delta": "Looking now."})));
        let ev = t.close(stop::STEERED);
        assert!(ev.contains(&Event::Stop { reason: stop::STEERED.into(), detail: None }));
        assert_eq!(t.blocks()[0]["text"], "Looking now.");
        t.resume();
        assert!(t.blocks().is_empty());
        t.feed(&f("text_delta", json!({"delta": "No tests, then."})));
        t.feed(&f("text_complete", json!({"text": "Looking now.No tests, then."})));
        t.feed(&f("message_complete", json!({"text": "Looking now.No tests, then."})));
        assert_eq!(t.stop, stop::COMPLETE);
        let b = t.blocks();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0]["text"], "No tests, then.");
    }

    /// A summary sends its instruction, not the user's last message again.
    #[test]
    fn a_summary_sends_its_instruction() {
        let r = Request {
            messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "fix the bug"}]})],
            instruction: Some("Summarize the conversation.".into()),
            ..Default::default()
        };
        assert_eq!(newest_user_text(&r), "Summarize the conversation.");
    }

    #[test]
    fn another_threads_frames_are_not_the_turns() {
        let mut t = Turn::for_message("thread-a", "");
        // Another thread's text is another conversation's reply.
        let other = json!({"type": "text_delta", "payload": {"delta": "not mine"},
            "thread_id": "thread-b", "message_id": "msg-b1"});
        assert!(t.feed(&other).is_empty());
        assert!(t.accepts(&other) == false);
        // An older message of this thread, replayed as the thread is joined,
        // is not (its text, thinking or calls).
        let replayed = json!({"type": "thinking_delta", "payload": {"delta": "old thoughts"},
            "thread_id": "thread-a", "message_id": "msg-a0"});
        assert!(!t.accepts(&replayed));
        assert!(t.feed(&replayed).is_empty());
        // A start opens this turn's message.
        let start = json!({"type": "message_start", "payload": {"role": "assistant"},
            "thread_id": "thread-a", "message_id": "msg-a1"});
        assert!(t.accepts(&start));
        let first = json!({"type": "text_delta", "payload": {"delta": "mine"},
            "thread_id": "thread-a", "message_id": "msg-a1"});
        assert!(t.accepts(&first));
        // A block opens, then the text fills it.
        assert_eq!(t.feed(&first).len(), 2);
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

    /// The reply's text must be announced as a block before its deltas: Oak's
    /// reply drops text for a block it never saw opened, which is exactly how
    /// a reply showed its thinking and then nothing.
    #[test]
    fn the_reply_text_is_announced_before_its_deltas() {
        let mut t = Turn::for_message("th", "");
        let mut ev = vec![];
        for f in [
            json!({"type": "message_start", "payload": {"role": "assistant"}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "thinking_start", "payload": {}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "thinking_delta", "payload": {"delta": "hm"}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "thinking_complete", "payload": {}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "text_delta", "payload": {"delta": "Hi"}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "text_delta", "payload": {"delta": " there"}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "message_complete", "payload": {"text": "Hi there"}, "thread_id": "th", "message_id": "m1"}),
        ] {
            ev.extend(t.feed(&f));
        }
        // The text block opens once, before its first delta.
        let opened: Vec<usize> = ev
            .iter()
            .filter_map(|e| match e {
                Event::Block { index, block } if block["type"] == "text" => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1, "the text block is announced exactly once");
        let first_text = ev.iter().position(|e| matches!(e, Event::Text { .. })).unwrap();
        let block_at = ev.iter().position(|e| matches!(e, Event::Block { block, .. } if block["type"] == "text")).unwrap();
        assert!(block_at < first_text, "the block comes before the text that fills it");
        // The block it opened is the one its deltas name.
        let text_at = ev.iter().find_map(|e| match e {
            Event::Text { index, .. } => Some(*index),
            _ => None,
        }).unwrap();
        assert_eq!(opened[0], text_at, "the text lands in the block that was opened");
        assert_eq!(t.text_at, Some(text_at));
    }

    /// After a tool result Quick carries the turn on in a fresh assistant
    /// message; its text is the reply's, and must be drawn.
    #[test]
    fn the_turn_carries_on_in_the_next_message_after_a_tool() {
        let mut t = Turn::for_message("th", "");
        let mut ev = vec![];
        for f in [
            json!({"type": "message_start", "payload": {}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "text_delta", "payload": {"delta": "let me look"}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "message_complete", "payload": {}, "thread_id": "th", "message_id": "m1"}),
        ] {
            ev.extend(t.feed(&f));
        }
        // Oak answered the tool: the turn waits for the continuation.
        t.awaiting_continuation = true;
        let mut second = vec![];
        for f in [
            json!({"type": "message_start", "payload": {}, "thread_id": "th", "message_id": "m2"}),
            json!({"type": "text_delta", "payload": {"delta": "it is there"}, "thread_id": "th", "message_id": "m2"}),
            json!({"type": "message_complete", "payload": {}, "thread_id": "th", "message_id": "m2"}),
        ] {
            second.extend(t.feed(&f));
        }
        assert!(!second.is_empty(), "the continuation is part of the turn");
        let texts: Vec<&str> = second.iter().filter_map(|e| match e {
            Event::Text { text, .. } => Some(text.as_str()),
            _ => None,
        }).collect();
        assert_eq!(texts, ["it is there"]);
        // Both messages' text is in the reply.
        let all: String = t.blocks().iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("|");
        assert!(all.contains("let me look") && all.contains("it is there"), "{all}");
    }

    /// After Oak gives back results the relay lost, Quick's answer comes in
    /// a new assistant message: that is the turn's, in a block of its own.
    #[test]
    fn the_answer_to_a_resend_is_the_turns() {
        let mut t = Turn::for_message("th", "");
        for f in [
            json!({"type": "message_start", "payload": {}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "text_delta", "payload": {"delta": "I couldn't get the byte count."}, "thread_id": "th", "message_id": "m1"}),
        ] {
            t.feed(&f);
        }
        let closed = t.await_resend();
        assert!(matches!(closed.as_slice(), [Event::BlockEnd { .. }]));
        let mut ev = vec![];
        for f in [
            json!({"type": "message_start", "payload": {}, "thread_id": "th", "message_id": "m3"}),
            json!({"type": "text_delta", "payload": {"delta": "34711"}, "thread_id": "th", "message_id": "m3"}),
        ] {
            ev.extend(t.feed(&f));
        }
        assert!(ev.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "34711")));
        let texts: Vec<String> = t.blocks().iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(texts.len(), 2, "{texts:?}");
    }

    /// Quick's whole text replaces what its deltas made, a section at a
    /// time: deltas that came late, twice or not at all leave no mark.
    #[test]
    fn quicks_whole_text_replaces_the_deltas() {
        let mut t = Turn::for_message("th", "");
        let f = |k: &str, p: Value| json!({"type": k, "payload": p, "thread_id": "th", "message_id": "m1"});
        let mut ev = vec![];
        for frame in [
            f("message_start", json!({})),
            f("text_delta", json!({"delta": "hidden "})),
            f("text_delta", json!({"delta": "-a"})),
            f("text_delta", json!({"delta": "than plain"})),
            f("text_complete", json!({"text": "hidden dotfiles, so it's closer to `ls -a` than plain `ls`:"})),
            f("text_delta", json!({"delta": " after"})),
            f("text_complete", json!({"text": " after the tool."})),
            f("message_complete", json!({"text": " after the tool."})),
        ] {
            ev.extend(t.feed(&frame));
        }
        let text = t.blocks().iter().find(|b| b["type"] == "text").unwrap()["text"].as_str().unwrap().to_string();
        assert_eq!(text, "hidden dotfiles, so it's closer to `ls -a` than plain `ls`: after the tool.");
        // Oak was told: the last block it was given for the text is whole.
        let last = ev.iter().rev().find_map(|e| match e {
            Event::Block { block, .. } if block["type"] == "text" => block["text"].as_str().map(String::from),
            _ => None,
        });
        assert_eq!(last.as_deref(), Some(text.as_str()));
        // A message whose text came only whole is taken whole.
        let mut u = Turn::for_message("th", "");
        u.feed(&f("message_start", json!({})));
        u.feed(&f("text_delta", json!({"delta": "garb"})));
        u.feed(&f("message_complete", json!({"text": "Whole."})));
        assert_eq!(u.blocks()[0]["text"], "Whole.");
    }

    /// A call Quick asked for ends once Oak has run it, carrying its result,
    /// so Oak's own agent doesn't run it (it knows none of Quick's names).
    #[test]
    fn a_call_ends_with_the_result_oak_got() {
        let mut t = Turn::for_message("th", "");
        let mut ev = vec![];
        for frame in [
            json!({"type": "message_start", "payload": {}, "thread_id": "th", "message_id": "m1"}),
            json!({"type": "tool_call", "payload": {"tool_name": "folder_list", "tool_call_id": "c1",
                "arguments": {"params": {"name": "folder_list", "arguments": {"path": "/C:/x"}}}}, "thread_id": "th", "message_id": "m1"}),
        ] {
            ev.extend(t.feed(&frame));
        }
        assert!(!ev.iter().any(|e| matches!(e, Event::BlockEnd { .. })), "the call doesn't end before it has run");
        let done = t.finished_call("c1", &Ok("{\"entries\": []}".into()));
        let (Event::Block { block, .. }, Event::BlockEnd { .. }) = (&done[0], &done[1]) else { panic!("{done:?}") };
        assert_eq!(block["_oak_result"]["type"], "tool_result");
        assert_eq!(block["_oak_result"]["call_id"], "c1");
        assert_eq!(block["input"]["path"], "/C:/x", "the input is Quick's");
        assert!(t.blocks()[0].get("_oak_result").is_none());
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
        // A thought Quick sends without its start is shown too, where it came.
        let kinds: Vec<&str> = blocks.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(kinds, ["reasoning", "text", "tool_call"]);
        assert_eq!(blocks[0]["text"], "hm");
        assert_eq!(blocks.iter().find(|b| b["type"] == "tool_call").unwrap()["input"]["command"], "ls");
    }

    /// Quick sends a whole agent loop as one message (as its app's captures
    /// show): a thought opened with `thinking_start`, text, a call, later
    /// thoughts as deltas alone, more calls, the answer. Each lands in a
    /// block of its own at the end, in the order it came, so the reply only
    /// grows at its end: no text goes back into a block above a call, and no
    /// thought is lost.
    #[test]
    fn the_reply_grows_at_the_end() {
        let f = |k: &str, p: Value| json!({"type": k, "payload": p, "thread_id": "th", "message_id": "m1"});
        let call = |id: &str| {
            json!({"type": "tool_call", "payload": {"tool_name": "fdfind", "tool_call_id": id,
                "arguments": {"params": {"name": "fdfind", "arguments": {"args": "pong"}}}}})
        };
        let mut t = Turn::for_message("th", "");
        let mut ev = vec![];
        for frame in [
            f("message_start", json!({"role": "assistant"})),
            f("thinking_start", json!({})),
            f("thinking_delta", json!({"delta": "Look first."})),
            f("thinking_complete", json!({"thinking": "Look first.\n\n"})),
            f("text_delta", json!({"delta": "I'll look."})),
            call("c1"),
            // The next thought: deltas alone, some of them lost.
            f("thinking_delta", json!({"delta": "Nothing there"})),
            f("thinking_complete", json!({"thinking": "Look first.\n\nNothing there, so search.\n\n"})),
            call("c2"),
            // A thought whose deltas never came at all.
            f("thinking_complete", json!({"thinking": "Look first.\n\nNothing there, so search.\n\nFound it.\n\n"})),
            f("text_delta", json!({"delta": "It is "})),
            f("text_delta", json!({"delta": "there."})),
            f("text_complete", json!({"text": "It is there."})),
            f("message_complete", json!({"text": "It is there."})),
        ] {
            ev.extend(t.feed(&frame));
        }
        let blocks = t.blocks();
        let kinds: Vec<&str> = blocks.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(kinds, ["reasoning", "text", "tool_call", "reasoning", "tool_call", "reasoning", "text"]);
        let said: Vec<&str> = blocks.iter().filter(|b| b["type"] != "tool_call").filter_map(|b| b["text"].as_str()).collect();
        assert_eq!(said, ["Look first.", "I'll look.", "Nothing there, so search.", "Found it.", "It is there."]);
        // Every block is announced once, in order, and what streams in goes
        // to the newest block: nothing is added above what came after it.
        let mut newest: Option<usize> = None;
        for e in &ev {
            match e {
                Event::Block { index, .. } if newest.is_none_or(|n| *index > n) => newest = Some(*index),
                Event::Text { index, .. } | Event::Reasoning { index, .. } => assert_eq!(Some(*index), newest, "{e:?} went above the reply's end"),
                _ => {}
            }
        }
        assert_eq!(newest, Some(blocks.len() - 1));
        // Whole text that comes just after a call began is still the text
        // its deltas went to, not new text below the call.
        let mut u = Turn::for_message("th", "");
        for frame in [f("message_start", json!({})), f("text_delta", json!({"delta": "Let me"})), call("c3"), f("text_complete", json!({"text": "Let me look."}))] {
            u.feed(&frame);
        }
        let shown: Vec<String> = u.blocks().iter().map(|b| format!("{}:{}", b["type"].as_str().unwrap_or(""), b["text"].as_str().unwrap_or(""))).collect();
        assert_eq!(shown, ["text:Let me look.", "tool_call:"]);
    }

    /// A call Quick runs on its server is shown as one Oak ran: its block
    /// opens as Quick starts it and ends with what Quick says it returned,
    /// so Oak's agent doesn't run it. A call Quick reports and then hands
    /// Oak is one block, whichever comes first.
    #[test]
    fn quicks_own_calls_are_shown() {
        let f = |k: &str, p: Value| json!({"type": k, "payload": p, "thread_id": "th", "message_id": "m1"});
        let mut t = Turn::for_message("th", "");
        let mut ev = vec![];
        for frame in [
            f("message_start", json!({})),
            f("tool_start", json!({"tool_name": "web_search", "tool_id": "s1", "arguments": {}})),
            f("tool_arguments", json!({"tool_id": "s1", "arguments": {"query": "rust egui"}})),
            f("tool_result", json!({"tool_id": "s1", "result": "3 results", "error": null})),
            // Reported, then handed to Oak.
            f("tool_start", json!({"tool_name": "run_python_on_device", "tool_id": "s2", "arguments": {"code": "print(1)"}})),
            f("tool_call", json!({"tool_name": "run_python_outpost_local", "tool_call_id": "c2",
                "arguments": {"params": {"name": "run_python_outpost_local", "arguments": {"code": "print(1)"}}}})),
            // Handed to Oak, then reported.
            f("tool_call", json!({"tool_name": "folder_list", "tool_call_id": "c3",
                "arguments": {"params": {"name": "folder_list", "arguments": {"path": "/x"}}}})),
            f("tool_start", json!({"tool_name": "folder_list", "tool_id": "s3", "arguments": {"path": "/x"}})),
            // Quick's records of Oak's calls aren't the results shown.
            f("tool_result", json!({"tool_id": "s2", "result": "relay's", "error": null})),
            f("tool_result", json!({"tool_id": "s3", "result": "relay's", "error": null})),
            // One Quick never gives the result of.
            f("tool_start", json!({"tool_name": "kg_search", "tool_id": "s4", "arguments": {"query": "oak"}})),
            f("tool_start", json!({"tool_name": "url_fetch", "tool_id": "s5", "arguments": {"url": "https://x"}})),
            f("tool_result", json!({"tool_id": "s5", "result": "", "error": "403"})),
            f("message_complete", json!({})),
        ] {
            ev.extend(t.feed(&frame));
        }
        let blocks = t.blocks();
        let named: Vec<(&str, &str)> = blocks.iter().map(|b| (b["name"].as_str().unwrap_or(""), b["id"].as_str().unwrap_or(""))).collect();
        assert_eq!(named, [("web_search", "s1"), ("run_python_outpost_local", "c2"), ("folder_list", "c3"), ("kg_search", "s4"), ("url_fetch", "s5")]);
        assert_eq!(blocks[0]["input"]["query"], "rust egui");
        // Only Oak's own calls are Oak's to run.
        let ran: Vec<&str> = t.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(ran, ["c2", "c3"]);
        // Each of Quick's calls ends, with its result.
        let ended = |i: usize| ev.iter().any(|e| matches!(e, Event::BlockEnd { index } if *index == i));
        let result = |i: usize| {
            ev.iter().rev().find_map(|e| match e {
                Event::Block { index, block } if *index == i => block.get("_oak_result").cloned(),
                _ => None,
            })
        };
        for i in [0, 3, 4] {
            assert!(ended(i) && result(i).is_some(), "block {i} ends with a result");
        }
        assert!(!ended(1) && !ended(2), "Oak's calls end once Oak has run them");
        assert_eq!(result(0).unwrap()["content"], "3 results");
        assert_eq!(result(4).unwrap()["is_error"], true);
        assert!(blocks.iter().all(|b| b.get("_oak_result").is_none()));
        assert!(same_tool("DESKTOP-1_folder_list", "folder_list") && same_tool("run_python_on_device_with_write", "run_python_outpost_local_with_write"));
        assert!(!same_tool("run_python", "run_python_outpost_local"));
    }

    /// Quick's whole text for the answer can carry what the message said
    /// between its calls, run together: each block keeps only its own, so
    /// the answer doesn't repeat the steps above it.
    #[test]
    fn the_answer_does_not_repeat_the_steps_above_it() {
        let f = |k: &str, p: Value| json!({"type": k, "payload": p, "thread_id": "th", "message_id": "m1"});
        let call = |id: &str| {
            json!({"type": "tool_call", "payload": {"tool_name": "ripgrep", "tool_call_id": id,
                "arguments": {"params": {"name": "ripgrep", "arguments": {}}}}, "thread_id": "th", "message_id": "m1"})
        };
        let mut t = Turn::for_message("th", "");
        for frame in [
            f("message_start", json!({})),
            f("text_delta", json!({"delta": "I'll check the notes."})),
            f("text_complete", json!({"text": "I'll check the notes."})),
            call("c1"),
            f("text_delta", json!({"delta": "Cargo is installed."})),
            call("c2"),
            f("text_delta", json!({"delta": "It built."})),
            f("text_complete", json!({"text": "I'll check the notes.Cargo is installed.It built."})),
            f("message_complete", json!({"text": "I'll check the notes.Cargo is installed.It built."})),
        ] {
            t.feed(&frame);
        }
        let said: Vec<String> = t.blocks().iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(said, ["I'll check the notes.", "Cargo is installed.", "It built."]);
        // With whitespace between them, and whole text Quick gives for an
        // earlier section that holds only that section, the same.
        let mut u = Turn::for_message("th", "");
        for frame in [
            f("message_start", json!({})),
            f("text_delta", json!({"delta": "Looking."})),
            call("c3"),
            f("text_delta", json!({"delta": "Done"})),
            f("text_complete", json!({"text": "Looking.\n\nDone, it is there."})),
        ] {
            u.feed(&frame);
        }
        let said: Vec<String> = u.blocks().iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(said, ["Looking.", "Done, it is there."]);
        // Whole text that carries only the blocks since the section Quick
        // last gave whole, the same.
        let mut v = Turn::for_message("th", "");
        for frame in [
            f("message_start", json!({})),
            f("text_delta", json!({"delta": "One."})),
            f("text_complete", json!({"text": "One."})),
            call("c4"),
            f("text_delta", json!({"delta": "Two."})),
            call("c5"),
            f("text_delta", json!({"delta": "Three."})),
            call("c6"),
            f("text_delta", json!({"delta": "Four."})),
            f("text_complete", json!({"text": "Two.Three.Four."})),
            f("message_complete", json!({"text": "Two.Three.Four."})),
        ] {
            v.feed(&frame);
        }
        let said: Vec<String> = v.blocks().iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(said, ["One.", "Two.", "Three.", "Four."]);
    }
}
