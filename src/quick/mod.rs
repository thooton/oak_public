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
//! the real project rather than a copy of it. Its device Python tool runs
//! Oak's own tools: its code is Oak calls written as Python calls
//! (`calls`), which the conversation's agent runs as it runs any model's,
//! and a thread's first message tells Quick so, with Oak's instructions and
//! tool definitions (`oak_preface`); later ones are the user's alone.
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

pub(crate) mod calls;
pub(crate) mod keeper;
pub(crate) mod recovery;
#[cfg(test)]
mod live_test;
#[cfg(test)]
mod switch_test;
pub(crate) mod markup;
#[cfg(test)]
mod models_test;
pub mod outpost;
#[cfg(test)]
mod outpost_test;
pub mod socket;
pub mod turn;
pub mod usage;

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

/// The discovery page that begins Quick's desktop sign-in: it sets the
/// `qwSurface` cookie, signs the user in through AWS's discovery portal, and
/// hands their workspace (`issuerUrl`, `tenantUrl`, `idcRegion`) back to
/// Oak's local callback port before Oak redirects to OIDC `/authorize`.
const DISCOVERY_URL: &str = "https://quick.aws.com/sn";

/// The OIDC endpoints, scoped to the workspace's IAM Identity Center region
/// (as `oidc_url` composes them).
fn oidc_url(region: &str, path: &str) -> String {
    let r = if region.trim().is_empty() {
        "us-east-1"
    } else {
        region.trim()
    };
    format!("https://oidc.{r}.amazonaws.com/{path}")
}

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
    Model {
        id: "smart",
        name: "Quick Smart",
        context: 1_000_000,
        max_output: 64_000,
        efforts: &["low", "medium", "high", "xhigh", "max"],
    },
    Model {
        id: "fast",
        name: "Quick Fast",
        context: 1_000_000,
        max_output: 64_000,
        efforts: &["low", "medium", "high"],
    },
    Model {
        id: "balanced",
        name: "Quick Balanced",
        context: 1_000_000,
        max_output: 64_000,
        efforts: &["low", "medium", "high"],
    },
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
    // The levels in order, so an effort the mode lacks goes to its nearest
    // below (a conversation keeps its effort across a switch: Claude's
    // `xhigh`, or `off`, or Smart's `max` taken to Fast).
    const RANK: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];
    let asked = r.effort.as_deref().unwrap_or("medium");
    let pick = match asked {
        "off" | "none" => "off",
        "low" | "min" | "minimal" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" | "ultra" => "max",
        _ => "medium",
    };
    let rank = |e: &str| RANK.iter().position(|x| *x == e);
    let want = rank(pick).unwrap_or(2);
    // Its highest that isn't above what was asked, else its lowest.
    m.efforts
        .iter()
        .copied()
        .filter(|e| rank(e).is_some_and(|k| k <= want))
        .max_by_key(|e| rank(e))
        .or_else(|| m.efforts.first().copied())
        .unwrap_or("low")
}

// ---------------------------------------------------------------- the login

pub mod login {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// One Quick login in quick.json: the registered client, the refresh
    /// token, who signed in, and what Quick's absence costs (the last error).
    /// A login kept on the vault (`remote`) has no client or refresh token
    /// here: the vault renews it and hands out access tokens.
    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct Stored {
        /// Which login this is: the email, lowercased, else one made at
        /// sign-in (`fill_id`). Rotation's seats and the vault name it.
        #[serde(default)]
        pub id: String,
        /// The login is on the vault: tokens come from there.
        #[serde(default)]
        pub remote: bool,
        /// The OIDC client Oak registered for itself (`client_id`).
        #[serde(default)]
        pub client_id: String,
        /// Its secret, as the registration gave it.
        #[serde(default)]
        pub client_secret: String,
        /// The token that lasts (`aor…`); each request buys an access token
        /// with it. Empty for a login on the vault.
        #[serde(default)]
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
        /// The IAM Identity Center region reported at discovery (`us-east-1`
        /// when absent).
        #[serde(default)]
        pub oidc_region: String,
        /// For Gateway OAuth accounts: the token endpoint and resource URI.
        #[serde(default)]
        pub token_endpoint: String,
        #[serde(default)]
        pub resource: String,
        /// The last sign-in or request error.
        #[serde(default)]
        pub error: String,
    }

    /// Where the sign-in is: idle, waiting for the browser, or done.
    #[derive(Clone, Debug, PartialEq)]
    pub enum State {
        Idle,
        /// The address to open, and the port Oak listens on for the code.
        Waiting {
            url: String,
            port: u16,
        },
        Done,
        Failed(String),
    }

    /// quick.json: the logins (one, from before there could be more).
    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum File {
        Logins { logins: Vec<Stored> },
        One(Box<Stored>),
    }

    /// The logins, and why one is on this machine although a vault is
    /// connected, when that was not asked for.
    pub(super) struct Logins {
        pub(super) list: Vec<Stored>,
        note: Option<String>,
    }

    pub(super) static STORE: LazyLock<Mutex<Logins>> = LazyLock::new(|| {
        Mutex::new(Logins {
            list: read_file(),
            note: None,
        })
    });
    pub(super) static STATE: Mutex<Option<State>> = Mutex::new(None);
    /// Held while an access token is renewed, so two requests don't spend
    /// one refresh token at once.
    static RENEWING: Mutex<()> = Mutex::new(());

    std::thread_local! {
        /// The login this thread's requests go through (`using`): a turn's,
        /// as rotation picked it for its conversation. Unset, the first.
        static USING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }
    /// A sign-in in flight: incremented on each `start()` so a new attempt
    /// cancels any older listener still waiting for the browser.
    static RUNNING: AtomicBool = AtomicBool::new(false);
    static LOGIN_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn path() -> std::path::PathBuf {
        crate::store::dir().join("quick.json")
    }

    /// The logins in quick.json, each with its id (one from before ids
    /// gets its own).
    fn read_file() -> Vec<Stored> {
        logins_of(crate::store::read_json(&path()))
    }

    /// The logins a quick.json holds (`v`), each with its id.
    pub(crate) fn logins_of(v: Option<Value>) -> Vec<Stored> {
        let mut list = match v.and_then(|v| serde_json::from_value(v).ok()) {
            Some(File::Logins { logins }) => logins,
            Some(File::One(s)) => vec![*s],
            None => vec![],
        };
        list.retain(|s| s.remote || !s.refresh.is_empty());
        for s in &mut list {
            fill_id(s);
        }
        list
    }

    fn write(l: &Logins) {
        if l.list.is_empty() {
            let _ = std::fs::remove_file(path());
            return;
        }
        let _ = crate::store::write_private_json(&path(), &File::Logins { logins: l.list.clone() });
    }

    /// Gives login `s` its id when it has none: its email, else one made
    /// from its client (which a sign-in registers afresh).
    pub(crate) fn fill_id(s: &mut Stored) {
        if !s.id.is_empty() {
            return;
        }
        s.id = if !s.email.trim().is_empty() {
            s.email.trim().to_lowercase()
        } else {
            use sha2::{Digest, Sha256};
            let seed = if s.client_id.is_empty() { &s.refresh } else { &s.client_id };
            let d = Sha256::digest(seed.as_bytes());
            format!("acct-{}", d.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>())
        };
    }

    /// The login this thread's requests go through: the one `using` set,
    /// else the first.
    pub fn current() -> Option<String> {
        let set = USING.with(|u| u.borrow().clone());
        let l = STORE.lock().unwrap();
        set.filter(|id| l.list.iter().any(|s| &s.id == id))
            .or_else(|| l.list.first().map(|s| s.id.clone()))
    }

    /// The login this thread's requests go through from now (`None`: the
    /// first). Threads a turn starts take it with them (`turn::run_in`).
    pub fn set_using(id: Option<String>) {
        USING.with(|u| *u.borrow_mut() = id);
    }

    /// The login `set_using` set on this thread.
    pub fn using() -> Option<String> {
        USING.with(|u| u.borrow().clone())
    }

    /// Login `id`, as stored.
    pub fn stored_of(id: &str) -> Option<Stored> {
        STORE.lock().unwrap().list.iter().find(|s| s.id == id).cloned()
    }

    /// The login this thread's requests go through, for the UI.
    pub fn stored() -> Stored {
        current().and_then(|id| stored_of(&id)).unwrap_or_default()
    }

    /// Every login, in order.
    pub fn all() -> Vec<Stored> {
        STORE.lock().unwrap().list.clone()
    }

    /// Changes login `id` in place (and saves); false when it is gone.
    fn update(id: &str, f: impl FnOnce(&mut Stored)) -> bool {
        let mut l = STORE.lock().unwrap();
        let Some(s) = l.list.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        f(s);
        write(&l);
        true
    }

    /// Who login `id` is, as `user-info` says, when it didn't say at
    /// sign-in (so signing it in again finds it).
    pub(crate) fn note_email(id: &str, email: &str) {
        let email = email.trim();
        if !email.is_empty() && stored_of(id).is_some_and(|s| s.email.is_empty()) {
            update(id, |s| s.email = email.to_string());
        }
    }

    /// Keeps login `s` in place of the one with its id (or its email: the
    /// same account signed in again keeps its id), else beside the others.
    /// One kept here keeps the outpost the old one had.
    pub(crate) fn keep(mut s: Stored) {
        fill_id(&mut s);
        let mut l = STORE.lock().unwrap();
        // (The vault's own logins are told apart by their ids alone.)
        let same = |o: &Stored| o.id == s.id || (!s.email.is_empty() && !o.remote && o.email.eq_ignore_ascii_case(&s.email));
        match l.list.iter_mut().find(|o| same(o)) {
            Some(o) => {
                // One on this machine keeps the id its seats name; the
                // vault's goes by the vault's.
                if !s.remote {
                    s.id = o.id.clone();
                }
                if s.outpost.is_empty() {
                    s.outpost = std::mem::take(&mut o.outpost);
                }
                if s.host.is_empty() {
                    s.host = std::mem::take(&mut o.host);
                }
                *o = s;
            }
            None => l.list.push(s),
        }
        write(&l);
    }

    pub fn state() -> Option<State> {
        STATE.lock().unwrap().clone()
    }

    fn set_state(s: State) {
        *STATE.lock().unwrap() = Some(s);
    }

    /// Loads quick.json at start (a login from before ids is saved with
    /// its own).
    pub fn load() -> bool {
        let list = read_file();
        let on = !list.is_empty();
        let mut l = STORE.lock().unwrap();
        l.list = list;
        write(&l);
        on
    }

    /// A login is kept (an expired access token still counts: the refresh
    /// token, or the vault, is what lasts).
    pub fn on() -> bool {
        !STORE.lock().unwrap().list.is_empty()
    }

    /// Signs login `id` out: one on the vault is removed there too, for
    /// every machine (when it can't be reached, it stays, and Err says why).
    pub fn sign_out(id: &str) -> Result<(), String> {
        if stored_of(id).is_some_and(|s| s.remote) {
            api::vault_remove("quick", id)?;
        }
        forget(id);
        set_state(State::Idle);
        Ok(())
    }

    /// Drops login `id` on this machine.
    fn forget(id: &str) {
        let mut l = STORE.lock().unwrap();
        l.list.retain(|s| s.id != id);
        l.note = None;
        write(&l);
        drop(l);
        crate::rotation::forget_account(crate::api::Provider::Quick, id);
        super::usage::forget(id);
    }

    /// Disconnected from the vault: its logins go from this machine.
    pub fn forget_remote() {
        let mut l = STORE.lock().unwrap();
        let n = l.list.len();
        l.list.retain(|s| !s.remote);
        if l.list.len() != n {
            write(&l);
        }
    }

    /// A login on the vault as this machine keeps it: who it is and where
    /// its workspace is, and the access token it handed out, if any.
    fn remote_login(info: &Value, access: Option<(String, i64)>) -> Stored {
        let field = |k: &str| info[k].as_str().unwrap_or("").to_string();
        let (access, expires_at) = access.unwrap_or_default();
        Stored {
            id: field("account_id"),
            remote: true,
            access,
            expires_at,
            email: field("email"),
            tenant: field("tenant"),
            host: field("host"),
            ..Stored::default()
        }
    }

    /// Brings the logins in step with the vault's: its logins are used (one
    /// on this machine too becomes its), and ones removed there go. This
    /// machine's outpost for each stays.
    pub fn sync() {
        let Ok(logins) = api::vault_list("quick") else { return };
        let ids: Vec<&str> = logins
            .iter()
            .filter_map(|i| i["account_id"].as_str())
            .filter(|a| !a.is_empty())
            .collect();
        let mut l = STORE.lock().unwrap();
        let before: Vec<(String, bool)> = l.list.iter().map(|s| (s.id.clone(), s.remote)).collect();
        l.list.retain(|s| !s.remote || ids.contains(&s.id.as_str()));
        for info in &logins {
            let Some(id) = info["account_id"].as_str().filter(|a| !a.is_empty()) else {
                continue;
            };
            let fresh = remote_login(info, None);
            match l.list.iter_mut().find(|s| s.id == id) {
                Some(s) if s.remote => {
                    s.email = fresh.email;
                    s.tenant = fresh.tenant;
                    if !fresh.host.is_empty() {
                        s.host = fresh.host;
                    }
                }
                Some(s) => {
                    let (outpost, host) = (std::mem::take(&mut s.outpost), std::mem::take(&mut s.host));
                    *s = fresh;
                    s.outpost = outpost;
                    if s.host.is_empty() {
                        s.host = host;
                    }
                }
                None => l.list.push(fresh),
            }
        }
        let after: Vec<(String, bool)> = l.list.iter().map(|s| (s.id.clone(), s.remote)).collect();
        let added = after.iter().any(|a| !before.contains(a));
        if after != before {
            write(&l);
        }
        drop(l);
        if added {
            crate::api::note_signed_in();
        }
    }

    /// Login `id` is on this machine while a vault is connected: it can
    /// move there.
    pub fn movable(id: &str) -> bool {
        api::vault_on() && stored_of(id).is_some_and(|s| !s.remote)
    }

    /// Why a login stayed on this machine, when it was meant for the vault.
    pub fn note() -> Option<String> {
        STORE.lock().unwrap().note.clone()
    }

    /// What the vault is given of login `s`: everything it needs to renew
    /// it (the client Oak registered, its region or Gateway endpoint) and
    /// to say where its workspace is.
    pub(crate) fn upload_body(s: &Stored) -> Value {
        json!({
            "account_id": s.id,
            "email": s.email,
            "tenant": s.tenant,
            "host": s.host,
            "client_id": s.client_id,
            "client_secret": s.client_secret,
            "oidc_region": s.oidc_region,
            "token_endpoint": s.token_endpoint,
            "resource": s.resource,
            "refresh_token": s.refresh,
            "access_token": s.access,
            "expires_at": s.expires_at,
        })
    }

    /// Puts login `s` on the vault; what this machine keeps of it.
    fn upload(s: &Stored) -> Result<Stored, String> {
        let info = api::vault_add("quick", &upload_body(s))?;
        if info["account_id"].as_str().is_none_or(str::is_empty) {
            return Err(format!(
                "{} didn't say which Quick account it took.",
                api::capital(&api::vault_name())
            ));
        }
        let mut kept = remote_login(&info, Some((s.access.clone(), s.expires_at)));
        if kept.host.is_empty() {
            kept.host = s.host.clone();
        }
        if kept.email.is_empty() {
            kept.email = s.email.clone();
        }
        kept.outpost = s.outpost.clone();
        Ok(kept)
    }

    /// Moves login `id` on this machine to the vault, which from then on is
    /// the only one to renew it.
    pub fn move_to_vault(id: &str) -> Result<(), String> {
        // No renewal may start while it moves.
        let _renewing = RENEWING.lock().unwrap();
        let s = stored_of(id)
            .filter(|s| !s.remote)
            .ok_or("There is no such Quick login on this machine.")?;
        let moved = upload(&s)?;
        if moved.id != id {
            forget(id);
        }
        keep(moved);
        STORE.lock().unwrap().note = None;
        Ok(())
    }

    /// A fresh sign-in: connected to a vault it goes there (one from before
    /// it kept Quick logins leaves it here, saying why).
    fn keep_signed_in(s: Stored) -> Result<(), String> {
        let mut note = None;
        let s = if api::signing_in_remotely() {
            match upload(&s) {
                Ok(r) => r,
                Err(e) if api::vault_cannot_keep(&e) => {
                    note = Some(format!("Kept on this machine: {e}"));
                    s
                }
                Err(e) => return Err(e),
            }
        } else {
            s
        };
        keep(s);
        STORE.lock().unwrap().note = note;
        Ok(())
    }

    /// The access token of the login this thread's requests go through
    /// (`current`).
    pub fn access_token() -> Result<String, String> {
        let id = current().ok_or("No Quick account: sign in on Home.")?;
        access_token_of(&id, None)
    }

    /// Login `id`'s access token: the one it has while it lasts, else one
    /// bought with its refresh token (or, on the vault, asked of it). With
    /// `refused` (Quick turned that token down) a new one, whatever its
    /// time. Err says what the server said, and marks the login failed.
    pub fn access_token_of(id: &str, refused: Option<&str>) -> Result<String, String> {
        let fresh = |s: &Stored| {
            !s.access.is_empty()
                && refused != Some(s.access.as_str())
                && s.expires_at > crate::api::now_ms() + 60_000
        };
        let s = stored_of(id).ok_or("That Quick account is no longer signed in: sign in on Home.")?;
        if fresh(&s) {
            return Ok(s.access);
        }
        // One renewal at a time: another request may have just renewed it.
        let _renewing = RENEWING.lock().unwrap();
        let s = stored_of(id).ok_or("That Quick account is no longer signed in: sign in on Home.")?;
        if fresh(&s) {
            return Ok(s.access);
        }
        if s.remote {
            return remote_access(&s, refused);
        }
        if s.refresh.is_empty() {
            return Err("No Quick account: sign in on Home.".into());
        }
        let resp = if !s.token_endpoint.is_empty() {
            let form = format!(
                "grant_type=refresh_token&client_id={}&refresh_token={}&resource={}",
                urlencode(&s.client_id),
                urlencode(&s.refresh),
                urlencode(&s.resource),
            );
            api::http()
                .post(&s.token_endpoint)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(form)
                .timeout(Duration::from_secs(30))
                .send()
                .map_err(api::network_error)?
        } else {
            let body = json!({
                "clientId": s.client_id,
                "clientSecret": s.client_secret,
                "grantType": "refresh_token",
                "refreshToken": s.refresh,
            });
            api::http()
                .post(oidc_url(&s.oidc_region, "token"))
                .header("Content-Type", "application/json")
                .json(&body)
                .timeout(Duration::from_secs(30))
                .send()
                .map_err(api::network_error)?
        };
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            let why = serde_json::from_str::<Value>(&text)
                .map(|v| provider::error_msg(&v, &text, None))
                .unwrap_or_else(|_| text.chars().take(300).collect());
            update(id, |s| s.error = format!("Quick refused to renew the sign-in ({status}): {why}"));
            return Err(format!("Quick: {why}. Sign in again on Home."));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("Quick sent an unreadable answer: {e}"))?;
        let access = v["accessToken"]
            .as_str()
            .or(v["access_token"].as_str())
            .unwrap_or("")
            .to_string();
        let next = v["refreshToken"]
            .as_str()
            .or(v["refresh_token"].as_str())
            .unwrap_or("")
            .to_string();
        let ttl = v["expiresIn"]
            .as_u64()
            .or(v["expires_in"].as_u64())
            .unwrap_or(3600) as i64;
        if access.is_empty() {
            return Err("Quick's sign-in answered without an access token.".into());
        }
        update(id, |s| {
            s.access = access.clone();
            s.expires_at = crate::api::now_ms() + ttl * 1000;
            if !next.is_empty() {
                s.refresh = next;
            }
            s.error.clear();
        });
        Ok(access)
    }

    /// An access token for login `s` from the vault: the one it has, or a
    /// new one when `refused` is still its current one.
    fn remote_access(s: &Stored, refused: Option<&str>) -> Result<String, String> {
        let Some(v) = api::vault_token("quick", &s.id, refused)? else {
            forget(&s.id);
            return Err(format!(
                "The Quick login was removed from {}: sign in again on Home.",
                api::vault_name()
            ));
        };
        let access = v["access_token"]
            .as_str()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| format!("{} sent no Quick token.", api::capital(&api::vault_name())))?
            .to_string();
        let expires = v["expires_at"].as_i64().unwrap_or(0);
        let host = v["host"].as_str().unwrap_or("").to_string();
        update(&s.id, |s| {
            s.access = access.clone();
            s.expires_at = expires;
            if !host.is_empty() {
                s.host = host;
            }
            s.error.clear();
        });
        Ok(access)
    }

    /// The socket's host for the login this thread's requests go through:
    /// the `tenantUrl` Quick's sign-in reports (its cell's own address), the
    /// cell Oak was configured with, or the one Quick's desktop app uses for
    /// this identity store.
    pub fn host() -> Result<String, String> {
        let id = current().ok_or("No Quick account: sign in on Home.")?;
        host_of(&id)
    }

    /// Login `id`'s socket host (as `host`).
    pub fn host_of(id: &str) -> Result<String, String> {
        let stored = stored_of(id).map(|s| s.host).unwrap_or_default();
        if !stored.is_empty() {
            return Ok(stored);
        }
        match known_cell() {
            Some(h) => {
                update(id, |s| s.host = h.clone());
                Ok(h)
            }
            None => Err(
                "Quick's workspace address wasn't reported at sign-in: sign in again on Home."
                    .into(),
            ),
        }
    }

    /// The host Oak saved at sign-in for the current login.
    #[cfg(test)]
    pub fn saved_host() -> String {
        stored().host
    }

    /// Saves this machine's own outpost id for the current login
    /// (`outpost::id`): each account registers this machine as its own.
    pub fn set_outpost(outpost: &str) {
        if let Some(id) = current() {
            update(&id, |s| s.outpost = outpost.to_string());
        }
    }

    /// The outpost Quick routes this machine's tools to for the current
    /// login, saved from the sign-in's callback (`outpostId`) or
    /// `OAK_QUICK_OUTPOST`.
    pub fn saved_outpost() -> String {
        std::env::var("OAK_QUICK_OUTPOST")
            .ok()
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .unwrap_or_else(|| stored().outpost)
    }

    /// The cell Oak's Quick login runs on. Quick's sign-in reports it
    /// (`tenantUrl`); when the callback doesn't, this is the cell Quick's
    /// desktop app signs in to for this identity store, and `OAK_QUICK_HOST`
    /// names another (a stand-in or a proxy, as `OAK_API_BASE` does for
    /// Claude).
    fn known_cell() -> Option<String> {
        if let Some(h) = std::env::var("OAK_QUICK_HOST")
            .ok()
            .map(|h| {
                h.trim()
                    .trim_start_matches("https://")
                    .trim_end_matches('/')
                    .to_string()
            })
            .filter(|h| !h.is_empty())
        {
            return Some(h);
        }
        (!DEFAULT_CELL.is_empty()).then(|| DEFAULT_CELL.to_string())
    }

    /// What Phase 1 discovery (`/callback`) resolved for Phase 2 (`/?code=`).
    #[derive(Clone, Debug)]
    pub(crate) enum Discovered {
        Idc {
            client: (String, String),
            redirect: String,
            region: String,
        },
        Gateway {
            client_id: String,
            redirect: String,
            token_endpoint: String,
            resource: String,
        },
    }

    /// Starts a fresh sign-in: opens the local loopback callback server and
    /// returns Quick's two-phase discovery URL (`https://quick.aws.com/sn?qwSurface=...`).
    /// When the user signs in there, Quick redirects their browser to
    /// `http://127.0.0.1:{port}/callback` with their workspace (`issuerUrl`,
    /// `tenantUrl`, `idcRegion`), Oak registers its OIDC client and 302-redirects
    /// the browser (with its active Identity Center session) to `/authorize`,
    /// and then exchanges the returned PKCE authorization code for tokens.
    pub fn start() -> Result<String, String> {
        let epoch = LOGIN_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        if RUNNING.load(Ordering::SeqCst) {
            let until = std::time::Instant::now() + Duration::from_millis(300);
            while RUNNING.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        RUNNING.store(true, Ordering::SeqCst);
        let (verifier, challenge) = pkce();
        let state = hex(rand::random::<[u8; 16]>());
        let nonce = hex(rand::random::<[u8; 16]>());
        let (listeners, port) = match listen() {
            Ok(l) => l,
            Err(e) => {
                RUNNING.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        let surface = format!("desktop:{nonce}:{port}:{APP_VERSION}");
        let url = format!("{DISCOVERY_URL}?qwSurface={}", urlencode(&surface));
        set_state(State::Waiting {
            url: url.clone(),
            port,
        });
        std::thread::Builder::new()
            .name("quick-login".into())
            .spawn(move || {
                let out = wait_for_login(listeners, port, &nonce, &state, &challenge, epoch);
                if LOGIN_GEN.load(Ordering::SeqCst) != epoch {
                    return;
                }
                match out {
                    Ok(Some((code, host, outpost, discovered))) => {
                        let disc = match discovered {
                            Some(d) => Ok(d),
                            None => {
                                let redirect = format!("http://127.0.0.1:{port}");
                                let issuer = format!(
                                    "https://identitycenter.amazonaws.com/{IDENTITY_STORE}"
                                );
                                register(&redirect, &issuer, "us-east-1").map(|client| {
                                    Discovered::Idc {
                                        client,
                                        redirect,
                                        region: "us-east-1".into(),
                                    }
                                })
                            }
                        };
                        match disc.and_then(|d| exchange(&d, &code, &verifier, &host, &outpost)) {
                            Ok(()) => {
                                set_state(State::Done);
                                crate::api::note_signed_in();
                            }
                            Err(e) => set_state(State::Failed(e)),
                        }
                    }
                    Ok(None) => return,
                    Err(e) => set_state(State::Failed(e)),
                }
                if LOGIN_GEN.load(Ordering::SeqCst) == epoch {
                    RUNNING.store(false, Ordering::SeqCst);
                }
            })
            .map_err(|e| {
                RUNNING.store(false, Ordering::SeqCst);
                format!("Couldn't start the sign-in: {e}")
            })?;
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
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// The epoch of the sign-in in flight (a `start()` bumps it, cancelling
    /// any older listener).
    #[cfg(test)]
    pub(crate) fn login_epoch() -> u64 {
        LOGIN_GEN.load(Ordering::SeqCst)
    }

    /// Loopback listeners (IPv4 `127.0.0.1` and, when available, IPv6 `::1` so
    /// `http://localhost:{port}` works on both) on the first free port from
    /// `CALLBACK_PORT`.
    pub(crate) fn listen() -> Result<(Vec<std::net::TcpListener>, u16), String> {
        let mut last = String::new();
        for port in CALLBACK_PORT..CALLBACK_PORT + 40 {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(v4) => {
                    let _ = v4.set_nonblocking(true);
                    let mut list = vec![v4];
                    if let Ok(v6) = std::net::TcpListener::bind(("::1", port)) {
                        let _ = v6.set_nonblocking(true);
                        list.push(v6);
                    }
                    return Ok((list, port));
                }
                Err(e) => last = e.to_string(),
            }
        }
        Err(format!(
            "Couldn't find a free port for Quick's sign-in: {last}"
        ))
    }

    /// Registers a public OIDC client with the discovered IAM Identity Center
    /// instance and region.
    fn register(
        redirect: &str,
        issuer_url: &str,
        region: &str,
    ) -> Result<(String, String), String> {
        let body = json!({
            "clientName": "AmazonQuick-Desktop",
            "clientType": "public",
            "scopes": [SCOPE],
            "grantTypes": ["authorization_code", "refresh_token"],
            "redirectUris": [redirect],
            "issuerUrl": issuer_url,
        });
        let resp = api::http()
            .post(oidc_url(region, "client/register"))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(api::network_error)?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!(
                "Quick's identity store refused Oak's client ({status}): {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("The registration was unreadable: {e}"))?;
        let id = v["clientId"].as_str().unwrap_or("").to_string();
        let secret = v["clientSecret"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            return Err("The registration came back without a client id.".into());
        }
        Ok((id, secret))
    }

    /// Builds the Phase 1 discovery 302-redirect URL and state from the query
    /// parameters sent to `/callback`.
    pub(crate) fn resolve_discovery(
        q: &std::collections::HashMap<String, String>,
        port: u16,
        expected_nonce: &str,
        state: &str,
        challenge: &str,
        reg: impl FnOnce(&str, &str, &str) -> Result<(String, String), String>,
    ) -> Result<(String, Discovered), String> {
        if let Some(n) = q.get("nonce")
            && n != expected_nonce
        {
            return Err("Quick's sign-in failed: the discovery nonce didn't match.".into());
        }
        if q.get("namespace").is_some_and(|s| !s.trim().is_empty()) {
            let client_id = q.get("clientId").cloned().unwrap_or_default();
            let tenant_url = q.get("tenantUrl").cloned().unwrap_or_default();
            let auth_endpoint = q.get("authEndpoint").cloned().unwrap_or_default();
            let token_endpoint = q.get("tokenEndpoint").cloned().unwrap_or_default();
            if client_id.is_empty()
                || tenant_url.is_empty()
                || auth_endpoint.is_empty()
                || token_endpoint.is_empty()
            {
                return Err(
                    "Quick's sign-in discovery returned an incomplete Gateway manifest.".into(),
                );
            }
            let redirect = format!("http://localhost:{port}");
            let auth_url = format!(
                "{auth_endpoint}?response_type=code&client_id={}&redirect_uri={}&state={state}&code_challenge={challenge}&code_challenge_method=S256&resource={}",
                urlencode(&client_id),
                urlencode(&redirect),
                urlencode(&tenant_url),
            );
            return Ok((
                auth_url,
                Discovered::Gateway {
                    client_id,
                    redirect,
                    token_endpoint,
                    resource: tenant_url,
                },
            ));
        }
        let issuer = q
            .get("issuerUrl")
            .cloned()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("https://identitycenter.amazonaws.com/{IDENTITY_STORE}"));
        let region = q
            .get("idcRegion")
            .cloned()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "us-east-1".into());
        let redirect = format!("http://127.0.0.1:{port}");
        let client = reg(&redirect, &issuer, &region)?;
        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}&code_challenge={challenge}&code_challenge_method=S256&prompt=login",
            oidc_url(&region, "authorize"),
            urlencode(&client.0),
            urlencode(&redirect),
            urlencode(SCOPE),
        );
        Ok((
            auth_url,
            Discovered::Idc {
                client,
                redirect,
                region,
            },
        ))
    }

    /// Runs the two-phase loopback callback server:
    /// Phase 1 (`GET /callback?nonce=...&issuerUrl=...&tenantUrl=...`):
    /// registers the OIDC client and 302-redirects the browser to `/authorize`.
    /// Phase 2 (`GET /?code=...&state=...`): returns the code and workspace host.
    #[allow(clippy::type_complexity)]
    pub(crate) fn wait_for_login(
        listeners: Vec<std::net::TcpListener>,
        port: u16,
        expected_nonce: &str,
        expect_state: &str,
        challenge: &str,
        epoch: u64,
    ) -> Result<Option<(String, String, String, Option<Discovered>)>, String> {
        use std::io::{BufRead, BufReader, Write};
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        let mut host = String::new();
        let mut outpost = String::new();
        let mut discovered: Option<Discovered> = None;
        loop {
            if LOGIN_GEN.load(Ordering::SeqCst) != epoch {
                return Ok(None);
            }
            if std::time::Instant::now() > deadline {
                return Err("The sign-in timed out: try again.".into());
            }
            let Some(mut stream) = listeners
                .iter()
                .find_map(|l| l.accept().ok().map(|(s, _)| s))
            else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let Some(target) = line.split_whitespace().nth(1) else {
                continue;
            };
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            if !path.starts_with("/callback") && path != "/" {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            let q = crate::quick::query(query);
            if let Some(err) = q.get("error") {
                let why = q
                    .get("error_description")
                    .cloned()
                    .unwrap_or_else(|| err.clone());
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return Err(format!("Quick's sign-in was refused: {why}"));
            }
            if let Some(h) = q
                .get("tenantUrl")
                .map(|u| {
                    u.trim_start_matches("https://")
                        .trim_end_matches('/')
                        .to_string()
                })
                .filter(|h| !h.is_empty())
            {
                host = h;
            }
            if let Some(o) = q.get("outpostId").filter(|o| !o.is_empty()) {
                outpost = o.clone();
            }
            if let Some(code) = q.get("code").cloned() {
                let Some(state) = q.get("state") else {
                    continue;
                };
                if state != expect_state {
                    let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let page = "<!doctype html><meta charset=utf-8><title>Oak</title><body style=\"font:16px system-ui;padding:40px\">Signed in to AWS Quick. You can close this tab.</body>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                return Ok(Some((code, host, outpost, discovered)));
            }
            if path.starts_with("/callback") {
                match resolve_discovery(&q, port, expected_nonce, expect_state, challenge, register)
                {
                    Ok((auth_url, disc)) => {
                        discovered = Some(disc);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 302 Found\r\nLocation: {auth_url}\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                    Err(e) => {
                        let page = format!(
                            "<!doctype html><meta charset=utf-8><title>Oak</title><body style=\"font:16px system-ui;padding:40px\">{e}</body>"
                        );
                        let _ = write!(
                            stream,
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                            page.len()
                        );
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Exchanges the code for the tokens, saves the login, and asks Quick who
    /// signed in.
    fn exchange(
        discovered: &Discovered,
        code: &str,
        verifier: &str,
        host: &str,
        outpost: &str,
    ) -> Result<(), String> {
        let (resp, client_id, client_secret, region, token_endpoint, resource) = match discovered {
            Discovered::Idc {
                client,
                redirect,
                region,
            } => {
                let body = json!({
                    "clientId": client.0,
                    "clientSecret": client.1,
                    "grantType": "authorization_code",
                    "code": code,
                    "redirectUri": redirect,
                    "codeVerifier": verifier,
                });
                let r = api::http()
                    .post(oidc_url(region, "token"))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .timeout(Duration::from_secs(30))
                    .send()
                    .map_err(api::network_error)?;
                (
                    r,
                    client.0.clone(),
                    client.1.clone(),
                    region.clone(),
                    String::new(),
                    String::new(),
                )
            }
            Discovered::Gateway {
                client_id,
                redirect,
                token_endpoint,
                resource,
            } => {
                let form = format!(
                    "grant_type=authorization_code&client_id={}&code={}&redirect_uri={}&code_verifier={}&resource={}",
                    urlencode(client_id),
                    urlencode(code),
                    urlencode(redirect),
                    urlencode(verifier),
                    urlencode(resource),
                );
                let r = api::http()
                    .post(token_endpoint)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .header("Accept", "application/json")
                    .body(form)
                    .timeout(Duration::from_secs(30))
                    .send()
                    .map_err(api::network_error)?;
                (
                    r,
                    client_id.clone(),
                    String::new(),
                    String::new(),
                    token_endpoint.clone(),
                    resource.clone(),
                )
            }
        };
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!(
                "Quick refused the sign-in ({status}): {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("The sign-in's answer was unreadable: {e}"))?;
        let access = v["accessToken"]
            .as_str()
            .or(v["access_token"].as_str())
            .unwrap_or("")
            .to_string();
        let refresh = v["refreshToken"]
            .as_str()
            .or(v["refresh_token"].as_str())
            .unwrap_or("")
            .to_string();
        let ttl = v["expiresIn"]
            .as_u64()
            .or(v["expires_in"].as_u64())
            .unwrap_or(3600) as i64;
        if refresh.is_empty() {
            return Err("Quick's sign-in gave no refresh token, so it couldn't be kept.".into());
        }
        let host = if host.is_empty() { known_cell().unwrap_or_default() } else { host.to_string() };
        let mut s = Stored {
            host,
            outpost: outpost.to_string(),
            client_id,
            client_secret,
            oidc_region: region,
            token_endpoint,
            resource,
            access: access.clone(),
            refresh,
            expires_at: crate::api::now_ms() + ttl * 1000,
            ..Stored::default()
        };
        s.email = whoami(&access, &s.host).unwrap_or_default();
        fill_id(&mut s);
        // The same account signed in again: its login is renewed in place
        // (on the vault when it is there).
        keep_signed_in(s)
    }

    /// Who the access token belongs to (best effort, for Home).
    fn whoami(access: &str, host: &str) -> Option<String> {
        if host.is_empty() {
            return None;
        }
        // (A POST, as `usage` reads it: a GET is not found.)
        let resp = api::http()
            .post(format!("https://{host}/integration/quick-work/user-info"))
            .bearer_auth(access)
            .header("Content-Type", "application/json")
            .body("{}")
            .timeout(Duration::from_secs(20))
            .send()
            .ok()?;
        let v: Value = serde_json::from_str(&resp.text().ok()?).ok()?;
        v["email"]
            .as_str()
            .or(v["user"]["email"].as_str())
            .map(String::from)
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
    /// The login's id (`login::Stored::id`).
    pub id: String,
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

fn account_of(s: &login::Stored) -> Account {
    let (usage, usage_error, usage_at) = usage::last_of(&s.id);
    Account {
        id: s.id.clone(),
        email: s.email.clone(),
        label: if s.remote { s.id.clone() } else { provider::short_key(&s.refresh) },
        error: (!s.error.is_empty()).then(|| s.error.clone()),
        usage,
        usage_error,
        usage_at,
    }
}

/// The login this thread's requests go through (the first, off a turn).
#[cfg(test)]
pub fn account() -> Option<Account> {
    let id = login::current()?;
    login::stored_of(&id).map(|s| account_of(&s))
}

/// Every Quick login, as the UI shows them.
pub fn accounts() -> Vec<Account> {
    login::all().iter().map(account_of).collect()
}

/// The login conversation `conv` is on (its seat), else the first.
pub fn account_of_conv(conv: &str) -> Option<Account> {
    let all = accounts();
    let seat = crate::rotation::seat(api::Provider::Quick, conv);
    seat.and_then(|id| all.iter().find(|a| a.id == id).cloned())
        .or_else(|| all.into_iter().next())
}

/// A Quick login is set.
pub fn on() -> bool {
    login::on()
}

/// What of a conversation a login that never had its Quick thread lacks
/// (it moved there from another login): all of request `messages` but the
/// newest user turns' text and pictures, which the request sends, as
/// `agent::unseen_by` gives it when Quick wrote none of it.
pub(crate) fn all_unseen(messages: &[Value]) -> Vec<Value> {
    let mut out = messages.to_vec();
    let last = out.iter().rposition(|m| m["role"] == "assistant").map_or(0, |i| i + 1);
    for m in &mut out[last..] {
        if let Some(c) = m["content"].as_array_mut() {
            c.retain(|b| !oak::is(b, "text") && !oak::is(b, "image"));
        }
    }
    let calls: std::collections::HashSet<String> = out
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| oak::is_call(b))
        .filter_map(|b| b["id"].as_str().map(String::from))
        .collect();
    for m in &mut out {
        if let Some(c) = m["content"].as_array_mut() {
            c.retain(|b| !oak::is_result(b) || b["call_id"].as_str().is_some_and(|id| calls.contains(id)));
        }
    }
    out.retain(|m| m["content"].as_array().is_none_or(|c| !c.is_empty()));
    out
}

/// The logins as rotation weighs them: by what is left of their month and
/// session window (a used-up one, or one Quick says can't run, waits);
/// ones on this machine are a fallback once the vault has logins.
fn candidates(now: i64) -> Vec<crate::rotation::Candidate> {
    let all = login::all();
    let any_remote = all.iter().any(|s| s.remote);
    all.iter()
        .map(|s| {
            let (u, _, _) = usage::last_of(&s.id);
            let (left, usable, resets_at) = match &u {
                Some(u) => {
                    let month_live = u.month_resets_at.is_none_or(|r| r > now);
                    let month = if month_live { u.month_percent } else { 0.0 };
                    let session_out = u.session_resumes_at.is_some_and(|r| r > now);
                    let used = month.max(if session_out { 100.0 } else { u.session_percent });
                    (
                        (100.0 - used.min(100.0)) as f64,
                        u.allowed() && !session_out && month < 100.0,
                        u.month_resets_at.filter(|_| month_live).unwrap_or(i64::MAX),
                    )
                }
                None => (100.0, true, i64::MAX),
            };
            crate::rotation::Candidate {
                id: s.id.clone(),
                left,
                usable: usable && s.error.is_empty() && !usage::observed_limit(&s.id, now),
                resets_at,
                preferred: s.remote || !any_remote,
            }
        })
        .collect()
}

/// The login conversation `conv`'s request goes through, and whether the
/// conversation was on another before (whose Quick thread this one lacks).
/// It keeps its login while that one can run; with none that can, it
/// stays where it is and Quick says why.
pub(crate) fn login_for(conv: &str) -> Option<(String, bool)> {
    let now = api::now_ms();
    let cands = candidates(now);
    if cands.is_empty() {
        return None;
    }
    let before = crate::rotation::seat(api::Provider::Quick, conv);
    let held = before.as_ref().and_then(|b| cands.iter().find(|c| &c.id == b));
    let picked = match held {
        // Its thread lives on its login: it moves only when that one can't run.
        Some(c) if c.usable || !cands.iter().any(|c| c.usable) => c.id.clone(),
        _ => {
            let usable: Vec<_> = cands.iter().filter(|c| c.usable).cloned().collect();
            let pool = if usable.is_empty() { &cands } else { &usable };
            if conv.is_empty() {
                pool[0].id.clone()
            } else {
                crate::rotation::pick(api::Provider::Quick, conv, pool, now, false)
                    .unwrap_or_else(|| pool[0].id.clone())
            }
        }
    };
    if !conv.is_empty() {
        crate::rotation::used(api::Provider::Quick, conv, &picked, 0, now);
    }
    let moved = before.is_some_and(|b| b != picked);
    Some((picked, moved))
}

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
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inner)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------- the request

/// The names Quick calls this machine's Python by (a device's own tools may
/// carry its name before them), which run Oak's tools (`calls`).
pub(crate) fn is_snippet(name: &str) -> bool {
    [
        "run_python_outpost_local",
        "run_python_outpost_local_with_write",
        "run_python_on_device",
        "run_python_on_device_with_write",
    ]
    .iter()
    .any(|n| name == *n || name.ends_with(&format!("_{n}")))
}

/// Marks a snippet's own call block (`is_snippet`), which the transcript
/// doesn't show: the Oak calls it ran are blocks of their own, and that is
/// all the user sees of it. One that failed is shown, with why.
pub(crate) const QUIET_KEY: &str = "_oak_quiet";

/// A call block as Oak keeps it: quiet when it is a snippet's (`QUIET_KEY`).
fn call_block(id: &str, name: &str, input: Value) -> Value {
    let mut block = json!({"type": "tool_call", "id": id, "name": name, "input": input});
    if is_snippet(name) {
        block[QUIET_KEY] = json!(true);
    }
    block
}

/// The threads Oak has briefed since it started.
static BRIEFED: LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(Default::default);

/// The tools Oak offers Quick: the request's (the conversation's, with the
/// kits it loaded), or a fresh session's when it carries none; not those a
/// snippet can't call (`calls::offered`).
pub(crate) fn oak_tools(r: &Request) -> Vec<Value> {
    let tools = if r.tools.is_empty() {
        crate::tools::definitions_with_kits(false, false, false, &[])
    } else {
        r.tools.clone()
    };
    tools
        .into_iter()
        .filter(|t| t["name"].as_str().is_some_and(calls::offered))
        .collect()
}

/// What Quick is told about this device: its Python tool runs Oak's tool
/// calls (`calls`), and Oak's own system prompt (the request's, as every
/// other model gets it) and tool definitions.
fn briefing(r: &Request) -> String {
    let mut tools = String::new();
    for t in oak_tools(r) {
        tools.push_str(&format!(
            "\n### {}\n{}\nInput: {}\n",
            t["name"].as_str().unwrap_or(""),
            t["description"].as_str().unwrap_or("").trim(),
            t["input_schema"]
        ));
    }
    // The instructions and tools come first, each in a section of its own,
    // so Quick's agent takes them as what it was loaded with, not as part
    // of what the user wrote; how to call them follows.
    let system = r.system.trim();
    let (instructions, which) = if system.is_empty() {
        (String::new(), "Above are the tools")
    } else {
        (
            format!("<instructions_loaded>\n{system}\n</instructions_loaded>\n\n"),
            "Above are the instructions and tools",
        )
    };
    format!(
        "{instructions}<tools_loaded>{tools}</tools_loaded>\n\n[Oak, on this device]\n{which} Oak has loaded for you here: its own system prompt, as it gives it to every model it runs, and its tool definitions. Follow them as your instructions for this work. This device runs Oak, a coding agent app, and Oak answers your device tools here, in the conversation's working folder on this machine. Your device Python tool (run_python_on_device, with or without write) doesn't run Python here: it runs Oak's own tools, the ones in <tools_loaded>, which do more than the Python sandbox and answer faster. Its code is one or more Oak tool calls written as Python calls, one per line, with literal arguments only, for example:\n\nREAD(path=\"src/main.rs\")\nEXECUTE(command=\"cargo test 2>&1 | tail -30\", summary=\"Run the tests\", timeout=300)\n\nAll the calls of one snippet run at the same time (calls on a file one of them writes keep their order), so put independent calls together in one snippet; its result gives each call's result, in order. Where the loaded instructions say tool calls in one reply run at the same time, for you that is the calls of one snippet. Arguments go by name (a tool's required ones may also come first without, in order), as Python literals: strings (triple-quoted and raw ones too), numbers, True/False/None, lists and dicts. Nothing else of Python runs: no variables, imports, expressions or loops. A tool with actions takes its action as an argument, as in TASK(action=\"output\", id=1). ASK isn't available this way: ask the user in your reply, with a decision card (Oak asks its questions with ASK, and the answers are your next message). While a goal is active, a card doesn't wait for the answers: Oak asks it as soon as you have written it, so keep working, in the same turn, on what doesn't depend on them; the answers come to you later in a message. Your other device tools (folder_list, ripgrep, fdfind, file_*) still work, but prefer Oak's. If this device drops off (it shows as OFFLINE, or a call to it fails to connect), Oak is most likely restarting or the network blipped: keep checking for it for at least 5 to 10 minutes before you end your turn, and once it is back, carry on where you left off. Don't save any new memories: don't call save_to_memory or add or edit memories, even when asked to; deleting them when the user asks is fine.\n[End of Oak's notes. The user's message follows.]"
    )
}

/// What a message on `thread` opens with: Oak's briefing, once per thread,
/// else nothing. Quick keeps the thread's history, briefing included, so it
/// isn't sent again when the prompt or tools change (notes, kits, the folder)
/// or Oak restarts: a thread Quick has already run snippets in has had it.
fn oak_preface(r: &Request, thread: &str) -> Option<String> {
    let mut sent = BRIEFED.lock().unwrap_or_else(|e| e.into_inner());
    let first = sent.insert(thread.to_string());
    // A login the conversation moved to has none of its thread: not the
    // briefing either, whatever the conversation's own Quick work says.
    if r.quick_session.is_none() && moving(&r.seat_conv()) {
        return Some(briefing(r));
    }
    if !first || (r.quick_session.is_none() && quick_worked_here(r)) {
        return None;
    }
    Some(briefing(r))
}

/// Conversations whose Quick thread moved to another login (`login_for`)
/// and no message has reached there yet. That login's thread has none of
/// the conversation, so each request sends it all (`all_unseen`) with Oak's
/// briefing until one gets there (`moved_in`): the move is seen only once,
/// and a first request that failed to send would otherwise leave the next
/// one sending its newest turn alone to a thread that has nothing.
static MOVED: LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(Default::default);

/// Conversation `conv`'s thread moved to another login just now.
pub(crate) fn moved_out(conv: &str) {
    if !conv.is_empty() {
        MOVED.lock().unwrap_or_else(|e| e.into_inner()).insert(conv.to_string());
    }
}

/// A message with the whole of `conv` reached the login it moved to.
pub(crate) fn moved_in(conv: &str) {
    MOVED.lock().unwrap_or_else(|e| e.into_inner()).remove(conv);
}

/// Whether `conv` moved to a login no message has reached yet.
pub(crate) fn moving(conv: &str) -> bool {
    !conv.is_empty() && MOVED.lock().unwrap_or_else(|e| e.into_inner()).contains(conv)
}

/// Whether Quick has run snippets in this conversation's thread already (so
/// it holds the briefing they were called by). Not those in what the thread
/// lacks (`unseen`): a fork's, or a moved login's, are another thread's,
/// which Oak can't otherwise tell after a restart (`MOVED` is in memory).
fn quick_worked_here(r: &Request) -> bool {
    let calls = |ms: &[Value]| -> Vec<String> {
        ms.iter()
            .filter(|m| m["role"] == "assistant")
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "tool_call" && b["name"].as_str().is_some_and(is_snippet))
            .map(|b| b["id"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let elsewhere: std::collections::HashSet<String> = calls(&r.unseen).into_iter().collect();
    calls(&r.messages).iter().any(|id| !elsewhere.contains(id))
}

/// The `user_message` frame for Oak request `r` on thread `thread`: the
/// newest user turn, since Quick keeps the thread's history.
fn user_message(r: &Request, thread: &str, m: &Model, outpost: Option<&str>) -> Value {
    user_message_as(r, thread, m, outpost, true)
}

/// `user_message`, opened with Oak's briefing where the thread needs it
/// (`brief`), else the user's text as it is.
fn user_message_as(r: &Request, thread: &str, m: &Model, outpost: Option<&str>, brief: bool) -> Value {
    let newest = match new_user_text(r) {
        // The conversation went on without this thread: it carries on from
        // there, not from a cut connection.
        None if r.instruction.is_none() && !r.unseen.is_empty() => CARRY_ON_SO_FAR.to_string(),
        _ => newest_user_text(r),
    };
    // What the thread hasn't seen of the conversation goes first (a
    // summary's own request carries its instruction alone).
    let newest = match r.instruction.is_none().then(|| so_far(r, thread)).flatten() {
        Some(s) => format!("{s}\n\n{newest}"),
        None => newest,
    };
    let text = if r
        .instruction
        .as_deref()
        .is_some_and(|i| !i.trim().is_empty())
    {
        newest
    } else if let Some(preface) = brief.then(|| oak_preface(r, thread)).flatten() {
        format!("{preface}\n\n{newest}")
    } else {
        newest
    };
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
    if let Some(i) = r
        .instruction
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
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
        let Some(blocks) = m["content"].as_array() else {
            continue;
        };
        // (Not the reminders a Generic-mode request adds of what changed,
        // notes and tools: Quick was briefed once and is told nothing more.)
        let said = |b: &Value| {
            b["type"] == "text"
                && b.get("_oak_file").is_none()
                && b.get(STEERED_KEY).is_none()
                && !b["text"].as_str().is_some_and(|t| t.starts_with("<system-reminder>"))
        };
        // The answers to the ASK Quick's decision cards became are what the
        // user said to them, as Quick's app sends a card's.
        let answered = |b: &Value| {
            b["type"] == "tool_result"
                && b["call_id"]
                    .as_str()
                    .or(b["tool_use_id"].as_str())
                    .is_some_and(|c| c.starts_with(ASK_CALL))
        };
        let text: Vec<String> = blocks
            .iter()
            .filter_map(|b| {
                if answered(b) {
                    calls::result_text(b).ok().map(|a| markup::decision_response(&a))
                } else if said(b) {
                    b["text"].as_str().map(String::from)
                } else {
                    None
                }
            })
            .filter(|t| !t.trim().is_empty())
            .collect();
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
pub(crate) fn steer_message(
    r: &Request,
    text: &str,
    id: &str,
    thread: &str,
    m: &Model,
    outpost: Option<&str>,
) -> Value {
    let mut said = r.clone();
    said.messages = vec![json!({"role": "user", "content": [{"type": "text", "text": text}]})];
    said.instruction = None;
    said.unseen.clear();
    // (Mid-turn: the turn's own message had the briefing, and what the
    // thread hadn't seen.)
    let mut v = user_message_as(&said, thread, m, outpost, false);
    v["payload"]["mid_turn"] = json!(true);
    v["payload"]["message_id"] = json!(id);
    v
}

/// What Quick is sent after the conversation so far (`so_far`) when the
/// user has said nothing since (a turn under way moved to Quick).
pub(crate) const CARRY_ON_SO_FAR: &str = "Carry on with the work from where the conversation above leaves off.";

/// The most of the conversation so far a message carries (characters): the
/// newest turns, and a line saying earlier ones were left out.
const SO_FAR_CHARS: usize = 400_000;

/// The most of one call's input or result it carries (characters).
const SO_FAR_PART: usize = 6_000;

/// `s`, clipped to `max` characters, saying so.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}… [{} more characters]", &s[..i], s[i..].chars().count()),
        None => s.to_string(),
    }
}

/// The conversation so far that Quick's thread doesn't have (a request's
/// `unseen`: other models wrote it, or the thread is a fresh one), as text
/// for the message: its turns in order, "Assistant" for the model's (Quick's
/// own, as Quick reads them). The model isn't told of any switch: this is the
/// conversation, which its thread lacks. Each call's input and result is cut
/// to `SO_FAR_PART`, and a conversation past `SO_FAR_CHARS` to its start and
/// newest turns; when anything is cut, the whole of it, uncut, is written to
/// the session's scratchpad (`so_far_file`) and the message names the file,
/// so nothing is lost. None when there is nothing.
fn so_far(r: &Request, thread: &str) -> Option<String> {
    let (body, cut) = so_far_body(&r.unseen, SO_FAR_PART, SO_FAR_CHARS)?;
    let whole = match cut.then(|| so_far_file(r, thread)).flatten() {
        Some(path) => format!(
            " Long tool inputs and results are cut short here, and so is the middle of a long conversation. The whole of it, uncut, is in {}: don't read it to catch up (what is here is enough for that); READ only the part you need, with offset and limit, if the work needs something that was cut.",
            path.display()
        ),
        None => String::new(),
    };
    Some(format!(
        "<conversation_so_far>\nThe conversation you are in, so far, oldest first (\"Assistant\" is you, the same assistant): it began before this thread, so this thread's history doesn't hold it. It is not a new task or a fresh start: carry on from where it leaves off, without starting over, introducing yourself, or looking again at what it already shows.{whole}\n\n{body}\n</conversation_so_far>"
    ))
}

/// What the file with the conversation so far (`so_far_file`) is named
/// with, before the thread's id and `.md`.
const SO_FAR_FILE: &str = "quick-conversation-";

/// Whether `b` is Quick catching up on a fresh thread: a call its agent made
/// (`agent::BY_KEY`) on the file with the conversation so far (`so_far_file`),
/// reading back what the conversation already holds. Every request leaves it
/// and its result out (`agent::for_provider`): another provider's model has
/// the conversation itself, and a later fresh Quick thread is sent it anew. The
/// transcript shows it as one closed row (`a_catch_up_read_is_left_out`).
pub(crate) fn is_boot(b: &Value) -> bool {
    if b["type"] != "tool_call" || !b[crate::agent::BY_KEY].as_str().is_some_and(|by| by.starts_with("quick")) {
        return false;
    }
    let input = b["input"].to_string();
    input.match_indices(SO_FAR_FILE).any(|(i, _)| {
        let rest = &input[i + SO_FAR_FILE.len()..];
        let id = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-').len();
        id > 0 && rest[id..].starts_with(".md")
    })
}

/// The whole of `r.unseen`, uncut, in a file in the session's scratchpad
/// named for the thread: its path, or None with no scratchpad or when it
/// can't be written.
fn so_far_file(r: &Request, thread: &str) -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(r.scratch.as_deref().filter(|s| !s.is_empty())?);
    let (body, _) = so_far_body(&r.unseen, usize::MAX, usize::MAX)?;
    let name: String = thread
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let path = dir.join(format!("{SO_FAR_FILE}{name}.md"));
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(
        &path,
        format!("# The conversation so far, oldest first (\"Assistant\" is you)\n\n{body}\n"),
    )
    .ok()?;
    Some(path)
}

/// The turns of `unseen` as text, each call's input and result cut to
/// `part` characters and the whole to about `max` bytes (its start and its
/// newest turns), and whether anything was cut. None when there is nothing.
fn so_far_body(unseen: &[Value], part: usize, max: usize) -> Option<(String, bool)> {
    let mut cut = false;
    let mut cut_to = |s: &str| {
        let c = clip(s, part);
        cut |= c.len() != s.len();
        c
    };
    let mut names = std::collections::HashMap::new();
    let mut turns: Vec<String> = vec![];
    // The newest user turns' files go with the message itself
    // (`turn::attached_images`), as their text and pictures do.
    let newest = unseen.iter().rposition(|m| m["role"] == "assistant").map_or(0, |i| i + 1);
    for (k, m) in unseen.iter().enumerate() {
        let assistant = m["role"] == "assistant";
        let mut lines = vec![];
        for b in m["content"].as_array().into_iter().flatten() {
            match b["type"].as_str().unwrap_or("") {
                "text" => {
                    let t = b["text"].as_str().unwrap_or("").trim();
                    if !t.is_empty() {
                        lines.push(format!("{}: {t}", if assistant { "Assistant" } else { "User" }));
                    }
                }
                "tool_call" => {
                    let name = b["name"].as_str().unwrap_or("tool");
                    names.insert(b["id"].as_str().unwrap_or("").to_string(), name.to_string());
                    lines.push(format!("Assistant called {name}: {}", cut_to(&b["input"].to_string())));
                }
                // The user's answers to Quick's own decision card (its ASK),
                // when another model went on after them: what the user said.
                // (The newest turns' answers go as the message, `new_user_text`.)
                "tool_result" if b["call_id"].as_str().is_some_and(|c| c.starts_with(ASK_CALL)) => {
                    if k >= newest {
                        continue;
                    }
                    if let Ok(said) = calls::result_text(b) {
                        lines.push(format!("User: {}", markup::decision_response(&said).trim()));
                    }
                }
                "tool_result" => {
                    let name = names.get(b["call_id"].as_str().unwrap_or("")).map_or("the call", String::as_str);
                    let content = match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(parts) => parts
                            .iter()
                            .map(|p| match p["type"].as_str() {
                                Some("text") => p["text"].as_str().unwrap_or("").to_string(),
                                Some(other) => format!("[{other}]"),
                                None => String::new(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    let error = if b["is_error"] == true { " (an error)" } else { "" };
                    lines.push(format!("Result of {name}{error}: {}", cut_to(content.trim())));
                }
                "image" => lines.push("[a picture]".into()),
                // A text file's words are the conversation's too; a PDF
                // can't go as text.
                "document" if k >= newest => {}
                "document" => {
                    let title = b["title"].as_str().unwrap_or("untitled");
                    lines.push(match document_text(b) {
                        Some(t) => format!("[a document: {title}]\n{}", cut_to(t.trim())),
                        None => format!("[a document: {title}]"),
                    });
                }
                _ => {}
            }
        }
        if !lines.is_empty() {
            turns.push(lines.join("\n"));
        }
    }
    if turns.is_empty() {
        return None;
    }
    // The newest turns, as many as fit.
    let boundary = |text: &str, mut index: usize| {
        while !text.is_char_boundary(index) { index -= 1; }
        index
    };
    let mut body = turns.join("\n\n");
    if body.len() > max {
        let head = boundary(&body, max / 4);
        let tail = boundary(&body, body.len() - (max - head));
        body = format!("{}\n\n[Earlier context clipped here.]\n\n{}", &body[..head], &body[tail..]);
        cut = true;
    }
    Some((body, cut))
}

/// A text document's words (an attached text file, `transcript::file_block`):
/// its `data` as it is, or its blob's bytes. None for a PDF or anything else.
pub(crate) fn document_text(b: &Value) -> Option<String> {
    if b["type"] != "document" || !b["media_type"].as_str().is_some_and(|m| m.starts_with("text/")) {
        return None;
    }
    if let Some(t) = b["data"].as_str() {
        return Some(t.to_string());
    }
    let data = crate::blobs::data_of(b)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// What Quick is sent when Oak asks again in the middle of a turn with
/// nothing new from the user (only the results of the calls Oak ran).
pub(crate) const CARRY_ON: &str = "(From Oak, the device that runs your tools.) The connection to you was cut in the middle of your turn. There is no new message from me: carry on with what I asked from where you stopped, without starting over or repeating tool calls that already ran.";

fn hex_uuid() -> String {
    let b: [u8; 16] = rand::random();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6] & 0x0f,
        b[7],
        (b[8] & 0x3f) | 0x80,
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

// ---------------------------------------------------------------- the turn

/// What a turn has collected: the blocks by index, and the stop reason.
#[derive(Default)]
pub(crate) struct Turn {
    // Oak keeps using its original conversation id after a Quick account handoff.
    local: Option<String>,
    outpost_missing: bool,
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
    /// The tools Oak offers Quick in this turn (`oak_tools`), which a
    /// snippet's calls are matched to.
    pub(crate) oak_tools: Vec<Value>,
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
    /// Assistant `message_id`s from earlier turns on this thread, so a
    /// replayed or trailing `message_start` from an interrupted or paused
    /// turn is never mistaken for the new turn's reply.
    pub(crate) ignore_messages: std::collections::HashSet<String>,
    /// The text block Quick's text streams into (its index), as Quick wrote
    /// it, markup and all: Oak is shown what `markup::split` makes of it.
    raw: Option<(usize, String)>,
    /// The questions of the decision cards the reply wrote, which Oak asks
    /// with ASK once it ends (`close`), or at once during a goal (`ask_now`).
    questions: Vec<Value>,
    /// The questions asked mid-turn (`ask_now`), and how many ASKs that was.
    asked: Vec<String>,
    asked_now: usize,
    /// Whether ASK asks asynchronously, as a test sets it; else the
    /// conversation's goal says (`asks_async`).
    pub(crate) asks_async: Option<bool>,
}

/// The id of the ASK a reply's decision cards become (then the message's
/// id): its answers are what Quick is sent next (`new_user_text`).
pub(crate) const ASK_CALL: &str = "quick-ask-";
/// The id of an ASK a reply's cards become mid-turn, during a goal
/// (`Turn::ask_now`; then the message's id and a count): it returns at once,
/// so it runs alongside the turn's other calls rather than on its own.
pub(crate) const ASK_NOW_CALL: &str = "quick-ask-now-";

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
    let input = if nested.is_null() {
        &p["arguments"]
    } else {
        nested
    };
    if input.is_null() {
        json!({})
    } else {
        input.clone()
    }
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
        None if p["status"].as_str() == Some("error") || p["is_error"].as_bool() == Some(true) => {
            Err(said)
        }
        None => Ok(said),
    }
}

impl Turn {
    /// A turn reading thread `thread`'s message `message` (Quick assigns the
    /// assistant's `message_id` when it starts the reply; until then the turn
    /// takes the first one the thread sends).
    pub(crate) fn for_message(thread: &str, message: &str) -> Turn {
        Turn {
            thread: thread.to_string(),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn local(&self) -> &str {
        self.local.as_deref().unwrap_or(&self.thread)
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
                if self.ignore_messages.contains(&message) {
                    return false;
                }
                let k = v["type"].as_str().unwrap_or("");
                let opens = matches!(k, "message_start");
                if k == "error" {
                    return true;
                }
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
                if self.ignore_messages.contains(&message) {
                    return false;
                }
                let k = v["type"].as_str().unwrap_or("");
                if k == "message_start" && self.awaiting_continuation {
                    self.ignore_messages.insert(self.message.clone());
                    self.message = message;
                    self.awaiting_continuation = false;
                    self.text_at = None;
                    self.text_open = false;
                    self.raw = None;
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
        if let Some(i) = self
            .text_at
            .filter(|&i| self.text_open && i + 1 == self.blocks.len())
        {
            return i;
        }
        if let Some(i) = self.text_at.filter(|_| self.text_open) {
            self.flush_text(out);
            out.push(Event::BlockEnd { index: i });
        }
        let i = self.text();
        self.text_open = true;
        out.push(Event::Block {
            index: i,
            block: self.blocks[i].clone(),
        });
        i
    }

    /// A new thought at the end of the reply (the one before, if open, ends).
    fn open_thinking(&mut self, out: &mut Vec<Event>) -> usize {
        if let Some(i) = self.thinking_at.take() {
            out.push(Event::BlockEnd { index: i });
        }
        let i = self.blocks.len();
        self.blocks
            .push(json!({"type": "reasoning", "text": "", "by": "quick"}));
        self.thinking_at = Some(i);
        out.push(Event::Block {
            index: i,
            block: self.blocks[i].clone(),
        });
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
                    out.push(Event::Reasoning {
                        index: i,
                        text: text.to_string(),
                    });
                }
            }
            "thinking_complete" => {
                // Quick gives all of the message's thinking so far: what
                // follows the last of it is this thought whole, which stands
                // for its deltas (some may never have come, or none did).
                let total = p["thinking"].as_str().unwrap_or("").to_string();
                let newest = total
                    .strip_prefix(self.thinking_given.as_str())
                    .unwrap_or(&total)
                    .trim()
                    .to_string();
                if !total.is_empty() {
                    self.thinking_given = total.clone();
                }
                if self.thinking_at.is_none() && !newest.is_empty() {
                    self.open_thinking(&mut out);
                }
                if let Some(i) = self.thinking_at.take() {
                    let had = self.blocks[i]["text"]
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !newest.is_empty() && had != newest {
                        self.blocks[i]["text"] = json!(newest);
                        out.push(Event::Block {
                            index: i,
                            block: self.blocks[i].clone(),
                        });
                    }
                    out.push(Event::BlockEnd { index: i });
                }
            }
            "text_delta" => {
                let text = p["delta"].as_str().unwrap_or("");
                if !text.is_empty() {
                    let i = self.open_text(&mut out);
                    self.stream_text(i, text, &mut out);
                    self.ask_now(&mut out);
                }
            }
            "text_complete" | "message_complete" => {
                // Quick gives the text whole when it ends (a section, then
                // the message): that is what it wrote, whatever the deltas
                // made of it, so it replaces them.
                let text = p["text"].as_str().unwrap_or("");
                let section = kind == "text_complete";
                if recovery::limit_notice(text) || (!section && recovery::limit_error(p)) {
                    out.extend(self.show_limit_notice());
                } else if !text.is_empty() && (section || (!self.section_given && self.message_texts <= 1)) {
                    self.settle_text(text, section, &mut out);
                }
                if section {
                    self.ask_now(&mut out);
                } else {
                    // The turn is over: Quick ran its own loop and Oak ran
                    // its tools as it asked, so nothing is left to do.
                    out.extend(self.close(self.completion()));
                }
            }
            // Quick's report of a call its agent made: shown as it is made,
            // whether Quick runs it on its server or hands it to Oak (then
            // the call Oak runs is this block, not one more).
            "tool_start" => {
                let id = p["tool_id"]
                    .as_str()
                    .or(p["tool_call_id"].as_str())
                    .unwrap_or("")
                    .to_string();
                let name = p["tool_name"].as_str().unwrap_or("").to_string();
                let input = quick_input(p);
                if !id.is_empty()
                    && !name.is_empty()
                    && !self.quick_calls.iter().any(|c| c.tool_id == id)
                {
                    match self.unreported_device_call(&name, &input) {
                        Some(k) => self.quick_calls[k].tool_id = id,
                        None => {
                            if let Some(t) = self.thinking_at.take() {
                                out.push(Event::BlockEnd { index: t });
                            }
                            let i = self.blocks.len();
                            self.blocks.push(call_block(&id, &name, input));
                            out.push(Event::Block {
                                index: i,
                                block: self.blocks[i].clone(),
                            });
                            self.quick_calls.push(QuickCall {
                                tool_id: id,
                                block: i,
                                ..Default::default()
                            });
                        }
                    }
                }
            }
            "tool_arguments" => {
                let id = p["tool_id"].as_str().unwrap_or("");
                let input = quick_input(p);
                if let Some(c) = self
                    .quick_calls
                    .iter()
                    .find(|c| !id.is_empty() && c.tool_id == id && !c.device && !c.done)
                    && input.as_object().is_some_and(|o| !o.is_empty())
                    && self.blocks[c.block]["input"] != input
                {
                    let i = c.block;
                    self.blocks[i]["input"] = input;
                    out.push(Event::Block {
                        index: i,
                        block: self.blocks[i].clone(),
                    });
                }
            }
            // What a call Quick ran returned: its block ends with it. Quick
            // records the calls Oak ran too; those end with Oak's result.
            "tool_result" => {
                let id = ["tool_id", "tool_call_id", "tool_use_id", "id"]
                    .iter()
                    .find_map(|k| p[*k].as_str())
                    .unwrap_or("");
                if same_tool(p["tool_name"].as_str().unwrap_or(""), "list_outposts") {
                    self.record_outposts(&quick_result(p));
                }
                if let Some(k) = self
                    .quick_calls
                    .iter()
                    .position(|c| !id.is_empty() && c.tool_id == id && !c.device && !c.done)
                {
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
                    let block = call_block(&id, &name, input.clone());
                    // The call Quick reported is this one: its block becomes
                    // the call Oak runs.
                    match self.reported_call(&name, &input) {
                        Some(k) => {
                            self.quick_calls[k].device = true;
                            let i = self.quick_calls[k].block;
                            self.blocks[i] = block;
                            out.push(Event::Block {
                                index: i,
                                block: self.blocks[i].clone(),
                            });
                        }
                        None => {
                            let i = self.blocks.len();
                            self.blocks.push(block);
                            out.push(Event::Block {
                                index: i,
                                block: self.blocks[i].clone(),
                            });
                            self.quick_calls.push(QuickCall {
                                block: i,
                                device: true,
                                ..Default::default()
                            });
                        }
                    }
                    self.calls.push((id, name, input));
                }
            }
            "turn_usage" => {
                let n = |k: &str| p[k].as_u64().unwrap_or(0);
                self.usage = (
                    n("input_tokens") + n("cache_read_tokens") + n("cache_write_tokens"),
                    n("cache_read_tokens"),
                    n("output_tokens"),
                );
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
        // Quick's markup is Oak's to read, not to show (`markup`).
        let (clean, questions) = markup::split(text, false);
        self.add_questions(questions);
        let text = clean.as_str();
        // Whole text that comes once a thought or a call has begun is still
        // the text block's above it while that block's section is open (its
        // deltas came, its whole text not yet); otherwise it is new text, at
        // the end.
        let late = self.text_at.filter(|&i| {
            self.text_open
                && i + 1 < self.blocks.len()
                && self.blocks[i]["text"]
                    .as_str()
                    .is_some_and(|t| t.len() > self.section_at)
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
        let start = if section {
            self.section_at.min(had.len())
        } else {
            0
        };
        let kept = had.get(..start).unwrap_or("");
        // A section's whole text that already holds the block's sections
        // before it is the block's whole text.
        let want = if !kept.is_empty() && text.starts_with(kept) {
            text.to_string()
        } else {
            format!("{kept}{text}")
        };
        if want != had {
            if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                eprintln!(
                    "TEXT the deltas made {} bytes of a section Quick wrote as {}; Quick's is kept",
                    had.len().saturating_sub(start),
                    text.len()
                );
            }
            self.blocks[i]["text"] = json!(want);
            out.push(Event::Block {
                index: i,
                block: self.blocks[i].clone(),
            });
        }
        if section {
            self.section_at = want.len();
            self.section_given = true;
        }
        // Deltas after it add to what Quick has given whole.
        self.raw = Some((i, want));
    }

    /// More of Quick's text, `delta`, for text block `i`: Oak is shown what
    /// the block's text as Quick wrote it now makes (`markup::split`, which
    /// holds back what may begin a card).
    fn stream_text(&mut self, i: usize, delta: &str, out: &mut Vec<Event>) {
        let had = self.blocks[i]["text"].as_str().unwrap_or("").to_string();
        let mut raw = match self.raw.take() {
            Some((at, raw)) if at == i => raw,
            _ => had.clone(),
        };
        raw.push_str(delta);
        let (shown, questions) = markup::split(&raw, true);
        self.raw = Some((i, raw));
        self.add_questions(questions);
        self.show_text(i, &had, shown, out);
    }

    /// Text block `i`, which Oak shows as `had`, is to show `shown`: more
    /// text when it only grows, else the block again (which replaces it).
    fn show_text(&mut self, i: usize, had: &str, shown: String, out: &mut Vec<Event>) {
        if shown == had {
            return;
        }
        let more = shown.strip_prefix(had).map(str::to_string);
        self.blocks[i]["text"] = json!(shown);
        match more {
            Some(text) => out.push(Event::Text { index: i, text }),
            None => out.push(Event::Block {
                index: i,
                block: self.blocks[i].clone(),
            }),
        }
    }

    /// The open text block is over: what was held back of it, as maybe the
    /// start of a card, is shown as it is.
    fn flush_text(&mut self, out: &mut Vec<Event>) {
        let Some((i, raw)) = self.raw.take() else {
            return;
        };
        if self.text_at != Some(i) || !self.text_open || i >= self.blocks.len() {
            return;
        }
        let had = self.blocks[i]["text"].as_str().unwrap_or("").to_string();
        let (shown, questions) = markup::split(&raw, false);
        self.add_questions(questions);
        self.show_text(i, &had, shown, out);
    }

    /// Cards the reply wrote: a question asked again (Quick's whole text
    /// repeats what its deltas wrote) replaces the one before, and one
    /// already asked mid-turn (`ask_now`) isn't asked again.
    fn add_questions(&mut self, questions: Vec<Value>) {
        for q in questions {
            if self.asked.iter().any(|a| q["question"] == a.as_str()) {
                continue;
            }
            self.questions.retain(|h| h["question"] != q["question"]);
            self.questions.push(q);
        }
    }

    /// While a goal is active, ASK doesn't wait for the answers, so the
    /// cards the reply wrote are asked at once, mid-turn, as an ASK that
    /// runs alongside the turn's calls (`ASK_NOW_CALL`): the user can
    /// answer while Quick works on, and the answers come to it in a message
    /// (`agent::Worker::steer_quick`). Without a goal they wait for the
    /// turn's end (`close`), where Oak's loop waits for the answers.
    fn ask_now(&mut self, out: &mut Vec<Event>) {
        if self.questions.is_empty() || !self.asks_async() {
            return;
        }
        let questions = std::mem::take(&mut self.questions);
        self.asked.extend(
            questions
                .iter()
                .filter_map(|q| q["question"].as_str().map(String::from)),
        );
        self.asked_now += 1;
        let id = format!("{ASK_NOW_CALL}{}-{}", self.message, self.asked_now);
        out.extend(self.oak_call(&id, "ASK", &json!({"questions": questions})));
    }

    /// Whether ASK asks asynchronously for this turn's conversation now (its
    /// goal is active), or as a test sets it.
    fn asks_async(&self) -> bool {
        self.asks_async
            .unwrap_or_else(|| crate::agent::goal_active_for(self.local()))
    }

    /// The call Oak runs Quick reported first (`tool_start`) with no call to
    /// Oak yet for it: the one of `name` with this input, else the first of
    /// `name`.
    fn reported_call(&self, name: &str, input: &Value) -> Option<usize> {
        let open = |c: &QuickCall| {
            !c.device
                && !c.done
                && same_tool(self.blocks[c.block]["name"].as_str().unwrap_or(""), name)
        };
        let same = self
            .quick_calls
            .iter()
            .position(|c| open(c) && self.blocks[c.block]["input"] == *input);
        same.or_else(|| self.quick_calls.iter().position(open))
    }

    /// A call Oak runs that Quick hasn't reported yet, for Quick's report of
    /// a call of `name` (the one with this input, else the first).
    fn unreported_device_call(&self, name: &str, input: &Value) -> Option<usize> {
        let open = |c: &QuickCall| {
            c.device
                && c.tool_id.is_empty()
                && same_tool(name, self.blocks[c.block]["name"].as_str().unwrap_or(""))
        };
        let same = self
            .quick_calls
            .iter()
            .position(|c| open(c) && self.blocks[c.block]["input"] == *input);
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
        let here = self.blocks[from..i]
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str());
        let above: Vec<&str> = self
            .earlier
            .iter()
            .map(String::as_str)
            .chain(here)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        for k in 0..above.len() {
            let mut rest = text;
            let all = above[k..]
                .iter()
                .all(|t| match rest.trim_start().strip_prefix(t) {
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

    /// A call of Oak's own tools that Quick's agent made through its device
    /// Python (`calls`): a block of its own at the reply's end, whole, with
    /// no result, so Oak's agent runs it as it runs any model's call (and
    /// the turn waits for what it returns: `turn::ran`).
    pub(crate) fn oak_call(&mut self, id: &str, name: &str, input: &Value) -> Vec<Event> {
        let mut out = vec![];
        if let Some(t) = self.thinking_at.take() {
            out.push(Event::BlockEnd { index: t });
        }
        let i = self.blocks.len();
        self.blocks
            .push(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
        out.push(Event::Block {
            index: i,
            block: json!({"type": "tool_call", "id": id, "name": name, "input": {}}),
        });
        out.push(Event::Input {
            index: i,
            json: input.to_string(),
        });
        out.push(Event::BlockEnd { index: i });
        out
    }

    /// Oak ran call `id` (`result`: what it returned, or why it failed): its
    /// block ends, with the result, which goes in the transcript as the
    /// call's (the agent doesn't run it again).
    pub(crate) fn finished_call(
        &mut self,
        id: &str,
        result: &Result<String, String>,
    ) -> Vec<Event> {
        let Some(i) = self
            .blocks
            .iter()
            .rposition(|b| b["type"] == "tool_call" && b["id"] == id)
        else {
            return vec![];
        };
        for c in &mut self.quick_calls {
            if c.block == i {
                c.done = true;
            }
        }
        self.end_call(i, id, result)
    }

    /// Call block `i` (`id`) ends with `result`, which goes in the
    /// transcript as the call's.
    fn end_call(&mut self, i: usize, id: &str, result: &Result<String, String>) -> Vec<Event> {
        if same_tool(self.blocks[i]["name"].as_str().unwrap_or(""), "list_outposts") {
            self.record_outposts(result);
        } else if result.is_ok() && self.quick_calls.iter().any(|c| c.block == i && c.device) {
            self.outpost_missing = false;
        }
        let (content, is_error) = match result {
            Ok(s) => (s.clone(), false),
            Err(e) => (e.clone(), true),
        };
        // A snippet that failed is shown, with why: no Oak call of its own
        // says so.
        if is_error && let Some(o) = self.blocks[i].as_object_mut() {
            o.remove(QUIET_KEY);
        }
        self.blocks[i]["_oak_result"] = oak::tool_result(&json!(id), json!(content), is_error);
        let out = vec![
            Event::Block {
                index: i,
                block: self.blocks[i].clone(),
            },
            Event::BlockEnd { index: i },
        ];
        // The transcript's copy of the call has no result in it.
        if let Some(o) = self.blocks[i].as_object_mut() {
            o.remove("_oak_result");
        }
        out
    }

    fn record_outposts(&mut self, result: &Result<String, String>) {
        let text = match result { Ok(s) | Err(s) => s };
        if recovery::outpost_missing(text) {
            self.outpost_missing = true;
        } else if result.is_ok() && recovery::outpost_available(text) {
            self.outpost_missing = false;
        }
    }

    fn show_limit_notice(&mut self) -> Vec<Event> {
        let mut out = vec![];
        if self.completion() != recovery::LIMIT {
            if let Some(i) = self.text_at.filter(|_| self.text_open) {
                self.flush_text(&mut out);
                out.push(Event::BlockEnd { index: i });
            }
            self.text_at = None;
            self.text_open = false;
            let i = self.open_text(&mut out);
            self.show_text(i, "", recovery::LIMIT_NOTICE.into(), &mut out);
        }
        out
    }

    fn completion(&self) -> &'static str {
        let text = self.blocks.iter().rev().find(|b| oak::is(b, "text"));
        if text.and_then(|b| b["text"].as_str()).and_then(|t| t.lines().last()).is_some_and(recovery::limit_notice) {
            recovery::LIMIT
        } else if self.outpost_missing && self.questions.is_empty() {
            recovery::DEVICE
        } else {
            stop::COMPLETE
        }
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
            if c.done {
                continue;
            }
            if c.device && stop != crate::agent::CUT_SHORT && stop != stop::TOOL_CALLS {
                continue;
            }
            self.quick_calls[k].done = true;
            let call = self.blocks[c.block]["id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let result = if stop == crate::agent::CUT_SHORT {
                Err("Stopped: the user interrupted the turn.".into())
            } else if c.device {
                Err("Stopped: the session was paused.".into())
            } else {
                Ok("Quick didn't say what this call returned.".into())
            };
            out.extend(self.end_call(c.block, &call, &result));
        }
        if let Some(i) = self.text_at.filter(|_| self.text_open) {
            self.flush_text(&mut out);
            self.text_open = false;
            out.push(Event::BlockEnd { index: i });
        }
        // The decision cards Quick's reply offered are Oak's ASK: the user
        // answers them as any model's questions, Oak's loop goes on for it,
        // and the answers are Quick's next message (`new_user_text`).
        if self.stop == stop::COMPLETE && !self.questions.is_empty() {
            let questions = std::mem::take(&mut self.questions);
            let id = format!("{ASK_CALL}{}", self.message);
            out.extend(self.oak_call(&id, "ASK", &json!({"questions": questions})));
            self.stop = stop::TOOL_CALLS.into();
        }
        out.push(Event::Stop {
            reason: self.stop.clone(),
            detail: None,
        });
        out
    }

    /// Quick took in a message the user typed meanwhile: the reply Oak read
    /// ended there (`close`), and Quick's message goes on in Oak's next one,
    /// which starts empty. The text the message wrote so far is kept
    /// (`earlier`), so its whole text, which can run that in, repeats none of it.
    pub(crate) fn resume(&mut self) {
        let from = self.message_from.min(self.blocks.len());
        let texts: Vec<String> = self.blocks[from..]
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .map(String::from)
            .collect();
        self.earlier.extend(texts);
        self.blocks.clear();
        self.raw = None;
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
            self.flush_text(&mut out);
            out.push(Event::BlockEnd { index: i });
        }
        self.text_at = None;
        self.text_open = false;
        self.section_at = 0;
        self.section_given = false;
        // That message read errors for its results: what it asked isn't
        // what the answer to the real ones will.
        self.questions.clear();
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
    fn a_missing_outpost_retries_only_if_the_reply_ends_without_access() {
        let start = |id| json!({"type": "tool_start", "payload": {"tool_id": id, "tool_name": "list_outposts"}});
        let result = |id, devices| json!({"type": "tool_result", "payload": {"tool_id": id,
            "result": {"outposts": devices, "scope": "current_turn_access"}}});
        let complete = || json!({"type": "message_complete", "payload": {"text": "I checked for this device."}});
        for attempt in 0..6 {
            let mut turn = Turn::for_message("device-retry", "");
            assert!(!turn.feed(&start("one")).is_empty());
            let results = turn.feed(&result("one", json!([])));
            assert!(results.iter().any(|e| matches!(e, Event::Block { block, .. } if block["_oak_result"]["call_id"] == "one")));
            if attempt == 5 {
                turn.feed(&start("two"));
                turn.feed(&result("two", json!([{"id": "back-online"}])));
            }
            let expected = if attempt == 5 { stop::COMPLETE } else { recovery::DEVICE };
            assert!(matches!(turn.feed(&complete()).last(), Some(Event::Stop { reason, .. }) if reason == expected));
        }
        // The same error text from another tool must not start a retry.
        let mut turn = Turn::for_message("device-retry", "");
        turn.feed(&json!({"type": "tool_start", "payload": {"tool_id": "one", "tool_name": "file_read"}}));
        turn.feed(&result("one", json!([])));
        assert!(matches!(turn.feed(&complete()).last(), Some(Event::Stop { reason, .. }) if reason == stop::COMPLETE));
    }

    #[test]
    fn preserves_work_before_the_usage_limit_notice() {
        let mut turn = Turn::for_message("quota", "");
        turn.feed(&json!({"type": "text_complete", "payload": {"text": "First test passed."}}));
        let end = turn.feed(&json!({"type": "message_complete", "payload": {"text": recovery::LIMIT_NOTICE}}));
        assert!(matches!(end.last(), Some(Event::Stop { reason, .. }) if reason == recovery::LIMIT));
        assert_eq!(turn.blocks()[0]["text"], "First test passed.");
        assert_eq!(turn.blocks()[1]["text"], recovery::LIMIT_NOTICE);
    }

    #[test]
    fn models_and_effort() {
        assert!(
            serves("smart") && serves("fast") && serves("balanced") && !serves("claude-opus-5-5")
        );
        assert_eq!(
            crate::providers::Kind::of("smart"),
            crate::providers::Kind::Quick
        );
        let m = model("smart").unwrap();
        let mut r = Request {
            provider: crate::providers::Kind::Quick,
            model: "smart".into(),
            ..Default::default()
        };
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
        let back: Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(body)
                .unwrap(),
        )
        .unwrap();
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
        let empty = Request {
            instruction: Some("Summarize.".into()),
            ..Default::default()
        };
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
    /// "NO TESTING"): `mid_turn`, with its own message id, and the text as
    /// typed.
    #[test]
    fn a_mid_turn_message_is_sent_as_quicks_app_sends_it() {
        let m = model("smart").unwrap();
        let v = steer_message(
            &Request::default(),
            "NO TESTING",
            "msg-87fa24c3",
            "th",
            m,
            Some("dev"),
        );
        assert_eq!(v["type"], "user_message");
        assert_eq!(
            v["payload"]["text"],
            "NO TESTING"
        );
        assert_eq!(v["payload"]["mid_turn"], true);
        assert_eq!(v["payload"]["message_id"], "msg-87fa24c3");
        assert_eq!(v["payload"]["thread_id"], "th");
        assert_eq!(v["payload"]["outpostId"], "dev");
    }

    /// Quick is told once how this device's Python tool runs Oak's tools,
    /// with Oak's prompt and tools, then sent the user's messages alone, even
    /// when the prompt or tools change.
    #[test]
    fn user_message_tells_quick_how_to_call_oaks_tools() {
        let m = model("smart").unwrap();
        let thread = "oak-test-briefing";
        let mut r = Request {
            system: "You are Oak.".into(),
            tools: vec![
                json!({"name": "READ", "description": "Read a file.", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
                json!({"name": "ASK", "description": "Ask the user.", "input_schema": {"type": "object", "properties": {}}}),
            ],
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "find all todo comments"}]}),
            ],
            ..Default::default()
        };
        let sent = |r: &Request| user_message(r, thread, m, Some("dev"))["payload"]["text"].as_str().unwrap().to_string();
        let first = sent(&r);
        // The instructions and tools come first, then how to call them.
        assert!(first.starts_with("<instructions_loaded>\nYou are Oak.\n</instructions_loaded>\n\n<tools_loaded>\n### READ\nRead a file.\nInput: {"), "{first}");
        assert!(first.contains("\n</tools_loaded>\n\n[Oak, on this device]\nAbove are the instructions and tools"), "{first}");
        assert!(!first.contains("### ASK"), "ASK can't be called through a snippet: {first}");
        assert!(first.ends_with("[End of Oak's notes. The user's message follows.]\n\nfind all todo comments"), "{first}");
        assert_eq!(sent(&r), "find all todo comments");
        r.system = "You are Oak, with a note.".into();
        assert_eq!(sent(&r), "find all todo comments", "a changed prompt is not sent again");
        // Nor what a Generic-mode request adds of what changed.
        r.messages[0]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "text", "text": "<system-reminder>\nNote 3 added\n</system-reminder>"}));
        assert_eq!(sent(&r), "find all todo comments");
        // A thread Quick has run snippets in (before Oak restarted) has had it.
        let worked = Request {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "Oak_run_python_on_device", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "ok"}, {"type": "text", "text": "next"}]}),
            ],
            ..r.clone()
        };
        assert_eq!(user_message(&worked, "oak-test-briefing-restarted", m, Some("dev"))["payload"]["text"], "next");
        // A summary sends its instruction alone.
        r.instruction = Some("Summarize.".into());
        assert_eq!(sent(&r), "Summarize.");
    }

    /// A switch to Quick carries the conversation over: what other models
    /// wrote since Quick's own last reply goes first, and the user's new
    /// message last; its thread's own work isn't repeated.
    #[test]
    fn a_thread_continues_a_conversation_another_model_worked_on() {
        let m = &MODELS[0];
        let (thread, outpost) = ("th-switched", Some("dev"));
        let reply = |by: &str, text: &str| json!({"role": "assistant", "content": [{"type": "text", "text": text, crate::agent::BY_KEY: by}]});
        let r = Request {
            provider: crate::providers::Kind::Quick,
            model: m.id.into(),
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "fix the build"}]}),
                reply("claude", "The test fails."),
                json!({"role": "user", "content": [
                    {"type": "tool_result", "call_id": "t1", "content": "error: no such file"},
                    {"type": "text", "text": "carry on ask Quick to fix it"}
                ]}),
            ],
            unseen: vec![
                json!({"role": "assistant", "content": [{"type": "text", "text": "The test fails."}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "error: no such file"}]}),
            ],
            ..Default::default()
        };
        let text = user_message(&r, thread, m, outpost)["payload"]["text"].as_str().unwrap().to_string();
        let so_far = text.find("<conversation_so_far>").expect("the conversation so far: {text}");
        let said = text.find("carry on ask Quick to fix it").expect("the new message");
        assert!(so_far < said, "what it missed comes first: {text}");
        assert!(text.contains("Assistant: The test fails.") && text.contains("Result of the call: error: no such file"), "{text}");
        assert!(!text.contains("fix the build"), "what the thread has isn't repeated: {text}");
        // Nothing new to say: it carries on from there.
        let mut quiet = r.clone();
        quiet.messages.pop();
        quiet.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "error"}]}));
        let text = user_message(&quiet, thread, m, outpost)["payload"]["text"].as_str().unwrap().to_string();
        assert!(text.contains(CARRY_ON_SO_FAR) && text.contains("<conversation_so_far>"), "{text}");
    }

    /// A conversation so far that the message cuts short is written whole
    /// to the session's scratchpad, and the message names the file.
    #[test]
    fn a_cut_conversation_so_far_is_written_whole_to_the_scratchpad() {
        let m = &MODELS[0];
        let dir = std::env::temp_dir().join(format!("oak-quick-so-far-{}", std::process::id()));
        let long = format!("{}TAIL-OF-THE-RESULT", "x".repeat(SO_FAR_PART + 100));
        let r = Request {
            provider: crate::providers::Kind::Quick,
            model: m.id.into(),
            messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "go on"}]})],
            unseen: vec![
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "t1", "name": "READ", "input": {"path": "a.rs"}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": long}]}),
            ],
            scratch: Some(dir.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let text = user_message(&r, "th-cut", m, Some("dev"))["payload"]["text"].as_str().unwrap().to_string();
        let file = dir.join("quick-conversation-th-cut.md");
        assert!(text.contains(&file.display().to_string()), "the file is named: {text}");
        assert!(!text.contains("TAIL-OF-THE-RESULT"), "the message has it cut: {text}");
        let whole = std::fs::read_to_string(&file).expect("the whole conversation");
        assert!(whole.contains(&long) && whole.contains("a.rs"), "{whole}");
        // Nothing cut: no file.
        let mut short = r.clone();
        short.unseen[1]["content"][0]["content"] = json!("ok");
        let text = user_message(&short, "th-short", m, Some("dev"))["payload"]["text"].as_str().unwrap().to_string();
        let so_far = &text[text.find("<conversation_so_far>").expect("the conversation so far")..];
        assert!(so_far.contains("Result of READ: ok") && !so_far.contains("quick-conversation-"), "{so_far}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Oak's calls through Quick's device Python are blocks of their own,
    /// whole, that Oak's agent runs.
    #[test]
    fn an_oak_call_is_a_whole_block_at_the_end() {
        let mut t = Turn::for_message("th-oak-call", "");
        let ev = t.oak_call("c1_oak0", "READ", &json!({"path": "a.rs"}));
        assert!(matches!(&ev[0], Event::Block { index: 0, block } if block["name"] == "READ" && block["input"] == json!({})));
        assert!(matches!(&ev[1], Event::Input { index: 0, json } if json == r#"{"path":"a.rs"}"#));
        assert!(matches!(ev[2], Event::BlockEnd { index: 0 }));
        assert_eq!(t.blocks()[0]["input"]["path"], "a.rs");
    }

    #[test]
    fn the_login_listener_waits_through_unrelated_requests() {
        use std::io::{Read, Write};
        let (listeners, port) = login::listen().expect("a loopback port");
        let epoch = login::login_epoch();
        let server = std::thread::spawn(move || {
            login::wait_for_login(listeners, port, "n1", "s1", "c1", epoch)
        });
        let ask = |line: &str| -> String {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
            s.write_all(line.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        };
        // Another program's request (or the browser's own) changes nothing.
        let ico = ask("GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        assert!(ico.starts_with("HTTP/1.1 404"), "{ico}");
        // The workspace's callback: the code comes back with its host.
        let done = ask(
            "GET /?code=abc&state=s1&tenantUrl=https%3A%2F%2Fcell.example.com&outpostId=dev HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        assert!(done.starts_with("HTTP/1.1 200"), "{done}");
        let got = server
            .join()
            .unwrap()
            .expect("the login listener")
            .expect("a code");
        assert_eq!(got.0, "abc");
        assert_eq!(got.1, "cell.example.com");
        assert_eq!(got.2, "dev");
        assert!(got.3.is_none(), "no discovery was needed for a code");
    }

    #[test]
    fn a_login_from_before_accounts_reads_as_the_first_with_an_id() {
        let one = json!({"client_id": "c1", "client_secret": "s1", "refresh": "aor1", "email": "Me@Example.com", "host": "h", "outpost": "o"});
        let list = login::logins_of(Some(one));
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].id.as_str(), list[0].outpost.as_str()), ("me@example.com", "o"));
        // Without an email, an id from its client, the same each time.
        let anon = json!({"client_id": "c2", "client_secret": "", "refresh": "aor2"});
        let a = login::logins_of(Some(anon.clone()));
        assert!(a[0].id.starts_with("acct-") && a[0].id == login::logins_of(Some(anon))[0].id);
        // Several, as Oak keeps them now; a vault's needs no refresh token,
        // and one with neither is dropped.
        let many = json!({"logins": [
            {"id": "a@x.com", "client_id": "c", "client_secret": "", "refresh": "aor"},
            {"id": "b@x.com", "remote": true, "host": "cell"},
            {"id": "c@x.com"}
        ]});
        let ids: Vec<String> = login::logins_of(Some(many)).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["a@x.com", "b@x.com"]);
        assert!(login::logins_of(None).is_empty());
    }

    #[test]
    fn the_vault_is_given_what_renews_a_login() {
        let s = login::Stored {
            id: "me@x.com".into(),
            client_id: "cid".into(),
            client_secret: "sec".into(),
            refresh: "aor".into(),
            oidc_region: "eu-west-1".into(),
            host: "cell".into(),
            ..Default::default()
        };
        let v = login::upload_body(&s);
        for (k, want) in [("account_id", "me@x.com"), ("client_id", "cid"), ("client_secret", "sec"), ("refresh_token", "aor"), ("oidc_region", "eu-west-1"), ("host", "cell")] {
            assert_eq!(v[k], want, "{k}");
        }
    }

    #[test]
    fn a_login_new_to_the_conversation_is_sent_all_of_it() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "first"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}, {"type": "tool_call", "id": "c1", "name": "READ", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "file"}, {"type": "text", "text": "newest"}]}),
        ];
        let out = all_unseen(&msgs);
        assert_eq!(out.len(), 3);
        // The newest turn's text goes as the message itself; its results stay.
        assert_eq!(out[2]["content"].as_array().unwrap().len(), 1);
        assert_eq!(out[2]["content"][0]["type"], "tool_result");
        // A conversation's first message has nothing before it.
        assert!(all_unseen(&msgs[..1]).is_empty());
    }

    #[test]
    fn login_discovery_builds_authorize_redirect_without_nonce() {
        let q = query(
            "nonce=n123&issuerUrl=https%3A%2F%2Fidentitycenter.amazonaws.com%2Fssoins-72233782ae72fb91&tenantUrl=https%3A%2F%2Fcell.example.com&idcRegion=us-east-1",
        );
        let (auth_url, disc) = login::resolve_discovery(
            &q,
            18080,
            "n123",
            "st456",
            "ch789",
            |redirect, issuer, region| {
                assert_eq!(redirect, "http://127.0.0.1:18080");
                assert_eq!(
                    issuer,
                    "https://identitycenter.amazonaws.com/ssoins-72233782ae72fb91"
                );
                assert_eq!(region, "us-east-1");
                Ok(("cid".into(), "csec".into()))
            },
        )
        .unwrap();
        assert!(
            auth_url.starts_with("https://oidc.us-east-1.amazonaws.com/authorize?"),
            "{auth_url}"
        );
        assert!(
            auth_url.contains("client_id=cid")
                && auth_url.contains("state=st456")
                && auth_url.contains("code_challenge=ch789"),
            "{auth_url}"
        );
        assert!(
            !auth_url.contains("nonce="),
            "authorize URL must not carry nonce: {auth_url}"
        );
        assert!(matches!(disc, login::Discovered::Idc { .. }));
        // Mismatched nonce is rejected.
        assert!(
            login::resolve_discovery(&q, 18080, "wrong", "st456", "ch789", |_, _, _| Ok((
                "c".into(),
                "s".into()
            )))
            .is_err()
        );
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
        assert!(ev.contains(&Event::Stop {
            reason: stop::STEERED.into(),
            detail: None
        }));
        assert_eq!(t.blocks()[0]["text"], "Looking now.");
        t.resume();
        assert!(t.blocks().is_empty());
        t.feed(&f("text_delta", json!({"delta": "No tests, then."})));
        t.feed(&f(
            "text_complete",
            json!({"text": "Looking now.No tests, then."}),
        ));
        t.feed(&f(
            "message_complete",
            json!({"text": "Looking now.No tests, then."}),
        ));
        assert_eq!(t.stop, stop::COMPLETE);
        let b = t.blocks();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0]["text"], "No tests, then.");
    }

    /// A summary sends its instruction, not the user's last message again.
    #[test]
    fn a_summary_sends_its_instruction() {
        let r = Request {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "fix the bug"}]}),
            ],
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
        assert!(!t.accepts(&other));
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
        let first_text = ev
            .iter()
            .position(|e| matches!(e, Event::Text { .. }))
            .unwrap();
        let block_at = ev
            .iter()
            .position(|e| matches!(e, Event::Block { block, .. } if block["type"] == "text"))
            .unwrap();
        assert!(
            block_at < first_text,
            "the block comes before the text that fills it"
        );
        // The block it opened is the one its deltas name.
        let text_at = ev
            .iter()
            .find_map(|e| match e {
                Event::Text { index, .. } => Some(*index),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            opened[0], text_at,
            "the text lands in the block that was opened"
        );
        assert_eq!(t.text_at, Some(text_at));
    }

    /// A reply's decision card is Oak's ASK, not text: neither the card nor
    /// the sources show, even while they stream, and the reply ends on the
    /// ASK for Oak's loop to run.
    #[test]
    fn a_decision_card_is_an_ask() {
        let mut t = Turn::for_message("th", "");
        let reply = "Pick one:\n<decision question=\"Which?\">\n<option description=\"the first\">A</option>\n<option>B</option>\n</decision>\n<sources>\n[x](mem://1)\n</sources>";
        let frame = |kind: &str, payload: Value| json!({"type": kind, "payload": payload, "thread_id": "th", "message_id": "m1"});
        let mut ev = t.feed(&frame("message_start", json!({})));
        for c in reply.as_bytes().chunks(7) {
            let delta = std::str::from_utf8(c).unwrap();
            ev.extend(t.feed(&frame("text_delta", json!({"delta": delta}))));
        }
        ev.extend(t.feed(&frame("message_complete", json!({"text": reply}))));
        // What Oak shows, delta by delta and block by block.
        let mut shown = String::new();
        for e in &ev {
            match e {
                Event::Text { text, .. } => shown.push_str(text),
                Event::Block { block, .. } if block["type"] == "text" => {
                    shown = block["text"].as_str().unwrap_or("").to_string()
                }
                _ => {}
            }
            assert!(!shown.contains('<'), "{shown:?}");
        }
        assert_eq!(shown, "Pick one:");
        let blocks = t.blocks();
        let ask = blocks.iter().find(|b| b["name"] == "ASK").expect("an ASK");
        assert_eq!(ask["id"], format!("{ASK_CALL}m1"));
        assert_eq!(
            ask["input"]["questions"][0]["options"],
            json!([{"label": "A", "description": "the first"}, "B"])
        );
        assert!(crate::agent::Ask::new(&ask["input"]).is_ok());
        assert!(matches!(ev.last(), Some(Event::Stop { reason, .. }) if reason == stop::TOOL_CALLS));
    }

    /// During a goal ASK doesn't wait, so a card is asked as soon as it is
    /// written, mid-turn (an ASK that runs alongside the turn's calls), the
    /// text after it is a block of its own, and the reply ends as any turn
    /// does, not on the ASK again.
    #[test]
    fn a_card_during_a_goal_is_asked_at_once() {
        let mut t = Turn::for_message("th", "");
        t.asks_async = Some(true);
        let card = "Pick one:\n<decision question=\"Which?\">\n<option>A</option>\n<option>B</option>\n</decision>\n";
        let after = "Meanwhile I'll keep going.";
        let frame = |kind: &str, payload: Value| json!({"type": kind, "payload": payload, "thread_id": "th", "message_id": "m1"});
        let mut ev = t.feed(&frame("message_start", json!({})));
        for c in card.as_bytes().chunks(5) {
            ev.extend(t.feed(&frame("text_delta", json!({"delta": std::str::from_utf8(c).unwrap()}))));
        }
        // Asked as soon as the card closes, before the reply goes on.
        let asked = ev
            .iter()
            .position(|e| matches!(e, Event::Block { block, .. } if block["name"] == "ASK"))
            .expect("the ASK, mid-turn");
        assert!(!ev[asked..].iter().any(|e| matches!(e, Event::Stop { .. })));
        ev.extend(t.feed(&frame("text_delta", json!({"delta": after}))));
        ev.extend(t.feed(&frame("message_complete", json!({"text": format!("{card}{after}")}))));
        let blocks = t.blocks();
        let asks: Vec<&Value> = blocks.iter().filter(|b| b["name"] == "ASK").collect();
        assert_eq!(asks.len(), 1, "{blocks:?}");
        assert_eq!(asks[0]["id"], format!("{ASK_NOW_CALL}m1-1"));
        assert_eq!(asks[0]["input"]["questions"][0]["options"], json!(["A", "B"]));
        let at = blocks.iter().position(|b| b["name"] == "ASK").unwrap();
        let texts: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        assert!(blocks[..at].iter().any(|b| b["text"] == "Pick one:"), "{blocks:?}");
        assert!(blocks[at..].iter().any(|b| b["text"].as_str().is_some_and(|s| s.contains(after))), "{texts:?}");
        assert!(texts.iter().all(|s| !s.contains('<')), "{texts:?}");
        assert!(matches!(ev.last(), Some(Event::Stop { reason, .. }) if reason == stop::COMPLETE));
    }

    /// The user's answers to that ASK are Quick's next message, as its app
    /// sends a card's.
    #[test]
    fn answers_to_a_card_are_quicks_next_message() {
        let r = Request {
            messages: vec![
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "quick-ask-m1", "name": "ASK", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "quick-ask-m1", "content": "1. Which?\n   Answer: A\n"}]}),
            ],
            ..Default::default()
        };
        assert_eq!(
            new_user_text(&r).as_deref(),
            Some("<decision_response>\nWhich?: A\n</decision_response>")
        );
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
        let texts: Vec<&str> = second
            .iter()
            .filter_map(|e| match e {
                Event::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["it is there"]);
        // Both messages' text is in the reply.
        let all: String = t
            .blocks()
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            all.contains("let me look") && all.contains("it is there"),
            "{all}"
        );
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
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::Text { text, .. } if text == "34711"))
        );
        let texts: Vec<String> = t
            .blocks()
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect();
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
            f(
                "text_complete",
                json!({"text": "hidden dotfiles, so it's closer to `ls -a` than plain `ls`:"}),
            ),
            f("text_delta", json!({"delta": " after"})),
            f("text_complete", json!({"text": " after the tool."})),
            f("message_complete", json!({"text": " after the tool."})),
        ] {
            ev.extend(t.feed(&frame));
        }
        let text = t.blocks().iter().find(|b| b["type"] == "text").unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            text,
            "hidden dotfiles, so it's closer to `ls -a` than plain `ls`: after the tool."
        );
        // Oak was told: the last block it was given for the text is whole.
        let last = ev.iter().rev().find_map(|e| match e {
            Event::Block { block, .. } if block["type"] == "text" => {
                block["text"].as_str().map(String::from)
            }
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
        assert!(
            !ev.iter().any(|e| matches!(e, Event::BlockEnd { .. })),
            "the call doesn't end before it has run"
        );
        let done = t.finished_call("c1", &Ok("{\"entries\": []}".into()));
        let (Event::Block { block, .. }, Event::BlockEnd { .. }) = (&done[0], &done[1]) else {
            panic!("{done:?}")
        };
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
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::Text { text, .. } if text == "Hi"))
        );
        let blocks = t.blocks();
        // A thought Quick sends without its start is shown too, where it came.
        let kinds: Vec<&str> = blocks.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(kinds, ["reasoning", "text", "tool_call"]);
        assert_eq!(blocks[0]["text"], "hm");
        assert_eq!(
            blocks.iter().find(|b| b["type"] == "tool_call").unwrap()["input"]["command"],
            "ls"
        );
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
            f(
                "thinking_complete",
                json!({"thinking": "Look first.\n\nNothing there, so search.\n\n"}),
            ),
            call("c2"),
            // A thought whose deltas never came at all.
            f(
                "thinking_complete",
                json!({"thinking": "Look first.\n\nNothing there, so search.\n\nFound it.\n\n"}),
            ),
            f("text_delta", json!({"delta": "It is "})),
            f("text_delta", json!({"delta": "there."})),
            f("text_complete", json!({"text": "It is there."})),
            f("message_complete", json!({"text": "It is there."})),
        ] {
            ev.extend(t.feed(&frame));
        }
        let blocks = t.blocks();
        let kinds: Vec<&str> = blocks.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(
            kinds,
            [
                "reasoning",
                "text",
                "tool_call",
                "reasoning",
                "tool_call",
                "reasoning",
                "text"
            ]
        );
        let said: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] != "tool_call")
            .filter_map(|b| b["text"].as_str())
            .collect();
        assert_eq!(
            said,
            [
                "Look first.",
                "I'll look.",
                "Nothing there, so search.",
                "Found it.",
                "It is there."
            ]
        );
        // Every block is announced once, in order, and what streams in goes
        // to the newest block: nothing is added above what came after it.
        let mut newest: Option<usize> = None;
        for e in &ev {
            match e {
                Event::Block { index, .. } if newest.is_none_or(|n| *index > n) => {
                    newest = Some(*index)
                }
                Event::Text { index, .. } | Event::Reasoning { index, .. } => {
                    assert_eq!(Some(*index), newest, "{e:?} went above the reply's end")
                }
                _ => {}
            }
        }
        assert_eq!(newest, Some(blocks.len() - 1));
        // Whole text that comes just after a call began is still the text
        // its deltas went to, not new text below the call.
        let mut u = Turn::for_message("th", "");
        for frame in [
            f("message_start", json!({})),
            f("text_delta", json!({"delta": "Let me"})),
            call("c3"),
            f("text_complete", json!({"text": "Let me look."})),
        ] {
            u.feed(&frame);
        }
        let shown: Vec<String> = u
            .blocks()
            .iter()
            .map(|b| {
                format!(
                    "{}:{}",
                    b["type"].as_str().unwrap_or(""),
                    b["text"].as_str().unwrap_or("")
                )
            })
            .collect();
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
            f(
                "tool_start",
                json!({"tool_name": "web_search", "tool_id": "s1", "arguments": {}}),
            ),
            f(
                "tool_arguments",
                json!({"tool_id": "s1", "arguments": {"query": "rust egui"}}),
            ),
            f(
                "tool_result",
                json!({"tool_id": "s1", "result": "3 results", "error": null}),
            ),
            // Reported, then handed to Oak.
            f(
                "tool_start",
                json!({"tool_name": "run_python_on_device", "tool_id": "s2", "arguments": {"code": "print(1)"}}),
            ),
            f(
                "tool_call",
                json!({"tool_name": "run_python_outpost_local", "tool_call_id": "c2",
                "arguments": {"params": {"name": "run_python_outpost_local", "arguments": {"code": "print(1)"}}}}),
            ),
            // Handed to Oak, then reported.
            f(
                "tool_call",
                json!({"tool_name": "folder_list", "tool_call_id": "c3",
                "arguments": {"params": {"name": "folder_list", "arguments": {"path": "/x"}}}}),
            ),
            f(
                "tool_start",
                json!({"tool_name": "folder_list", "tool_id": "s3", "arguments": {"path": "/x"}}),
            ),
            // Quick's records of Oak's calls aren't the results shown.
            f(
                "tool_result",
                json!({"tool_id": "s2", "result": "relay's", "error": null}),
            ),
            f(
                "tool_result",
                json!({"tool_id": "s3", "result": "relay's", "error": null}),
            ),
            // One Quick never gives the result of.
            f(
                "tool_start",
                json!({"tool_name": "kg_search", "tool_id": "s4", "arguments": {"query": "oak"}}),
            ),
            f(
                "tool_start",
                json!({"tool_name": "url_fetch", "tool_id": "s5", "arguments": {"url": "https://x"}}),
            ),
            f(
                "tool_result",
                json!({"tool_id": "s5", "result": "", "error": "403"}),
            ),
            f("message_complete", json!({})),
        ] {
            ev.extend(t.feed(&frame));
        }
        let blocks = t.blocks();
        let named: Vec<(&str, &str)> = blocks
            .iter()
            .map(|b| {
                (
                    b["name"].as_str().unwrap_or(""),
                    b["id"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("web_search", "s1"),
                ("run_python_outpost_local", "c2"),
                ("folder_list", "c3"),
                ("kg_search", "s4"),
                ("url_fetch", "s5")
            ]
        );
        assert_eq!(blocks[0]["input"]["query"], "rust egui");
        // The device Python's block is quiet (its Oak calls are shown
        // instead); the others are shown.
        let quiet: Vec<bool> = blocks.iter().map(|b| b.get(QUIET_KEY).is_some()).collect();
        assert_eq!(quiet, [false, true, false, false, false]);
        // Only Oak's own calls are Oak's to run.
        let ran: Vec<&str> = t.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(ran, ["c2", "c3"]);
        // Each of Quick's calls ends, with its result.
        let ended = |i: usize| {
            ev.iter()
                .any(|e| matches!(e, Event::BlockEnd { index } if *index == i))
        };
        let result = |i: usize| {
            ev.iter().rev().find_map(|e| match e {
                Event::Block { index, block } if *index == i => block.get("_oak_result").cloned(),
                _ => None,
            })
        };
        for i in [0, 3, 4] {
            assert!(
                ended(i) && result(i).is_some(),
                "block {i} ends with a result"
            );
        }
        assert!(
            !ended(1) && !ended(2),
            "Oak's calls end once Oak has run them"
        );
        assert_eq!(result(0).unwrap()["content"], "3 results");
        assert_eq!(result(4).unwrap()["is_error"], true);
        assert!(blocks.iter().all(|b| b.get("_oak_result").is_none()));
        assert!(
            same_tool("DESKTOP-1_folder_list", "folder_list")
                && same_tool(
                    "run_python_on_device_with_write",
                    "run_python_outpost_local_with_write"
                )
        );
        assert!(!same_tool("run_python", "run_python_outpost_local"));
        // A snippet that failed is shown, with why.
        let failed = t.finished_call("c2", &Err("Not an Oak call.".into()));
        assert!(failed.iter().any(|e| matches!(e, Event::Block { index: 1, block }
            if block.get(QUIET_KEY).is_none() && block["_oak_result"]["is_error"] == true)));
        assert!(is_snippet("Oak_run_python_on_device") && !is_snippet("run_python"));
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
            f(
                "text_complete",
                json!({"text": "I'll check the notes.Cargo is installed.It built."}),
            ),
            f(
                "message_complete",
                json!({"text": "I'll check the notes.Cargo is installed.It built."}),
            ),
        ] {
            t.feed(&frame);
        }
        let said: Vec<String> = t
            .blocks()
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            said,
            ["I'll check the notes.", "Cargo is installed.", "It built."]
        );
        // With whitespace between them, and whole text Quick gives for an
        // earlier section that holds only that section, the same.
        let mut u = Turn::for_message("th", "");
        for frame in [
            f("message_start", json!({})),
            f("text_delta", json!({"delta": "Looking."})),
            call("c3"),
            f("text_delta", json!({"delta": "Done"})),
            f(
                "text_complete",
                json!({"text": "Looking.\n\nDone, it is there."}),
            ),
        ] {
            u.feed(&frame);
        }
        let said: Vec<String> = u
            .blocks()
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect();
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
        let said: Vec<String> = v
            .blocks()
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(said, ["One.", "Two.", "Three.", "Four."]);
    }
}
