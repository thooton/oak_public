//! Freebuff: a separate agent service Oak talks to as a pipe, with its own
//! login, agent definitions and system prompt. This module copies the wire
//! protocol of the Freebuff CLI: the browser code login, the model catalog,
//! the hourly session (admission and release), Freebucks balances and the
//! chat-completions stream translated into the Anthropic events Oak's agent
//! loop understands.
//!
//! Every device identifier here is synthetic: the "enhanced" fingerprint and
//! the Ed25519 request-signing key are generated once per Oak folder from a
//! random seed, so nothing about the real machine is ever sent.
use crate::tools::Head;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub const API: &str = "https://www.codebuff.com";
pub const WEB: &str = "https://www.codebuff.com";

/// The client environment descriptor the CLI reports.
const CLIENT_ENV: &str = "v1;in=1;out=1;tp=none;term=1;ct=0;sz=120x40;ci=0;ssh=0;l=0;p=na;g=na;osc=na;tzo=0;px=none;tls=1;ca=0;cde=none";

/// The build this copy of the protocol reports itself as. The CLI sends its
/// own `CODEBUFF_CLI_VERSION` on every session call and completion
/// (`cliVersionHeaders`), as an observe-only marker.
const CLIENT_VERSION: &str = "0.2.35";
/// Bun's `fetch` user agent, which the CLI's requests carry wherever the
/// caller set none.
const BUN_UA: &str = "Bun/1.3.14";

/// The local IANA zone (the header is omitted for UTC, as the CLI does).
fn timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ")
        && !tz.trim().is_empty()
    {
        return Some(tz.trim().to_string());
    }
    let zone = iana_time_zone::get_timezone().ok()?;
    let utc = matches!(zone.as_str(), "UTC" | "Etc/UTC" | "Etc/GMT" | "GMT" | "Etc/UCT");
    (!utc).then_some(zone)
}

fn tz_header(req: Wire) -> Wire {
    match timezone() {
        Some(tz) => req.header("x-fb-timezone", tz),
        None => req,
    }
}

/// The client identity the service expects on the AI-SDK call. The runtime
/// token is the host runtime's UA, lowercase: a Bun build says
/// `runtime/bun/1.3.14`.
const AI_SDK_UA: &str = "ai-sdk/openai-compatible/0.0.0-test/codebuff ai-sdk/provider-utils/3.0.25 runtime/bun/1.3.14";

/// A request written the way Bun's `fetch` writes one: the caller's headers
/// sorted by byte order, then `Connection`, `User-Agent` (when the caller set
/// none), `Accept`, `Host`, `Accept-Encoding` and `Content-Length`.
struct Wire {
    method: reqwest::Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
}

impl Wire {
    fn new(method: reqwest::Method, url: impl Into<String>) -> Wire {
        Wire { method, url: url.into(), headers: Vec::new(), body: None, timeout: None }
    }
    fn get(url: impl Into<String>) -> Wire {
        Wire::new(reqwest::Method::GET, url)
    }
    fn post(url: impl Into<String>) -> Wire {
        Wire::new(reqwest::Method::POST, url)
    }
    fn header(mut self, k: &str, v: impl Into<String>) -> Wire {
        self.headers.push((k.to_string(), v.into()));
        self
    }
    fn bearer(self, token: &str) -> Wire {
        self.header("Authorization", format!("Bearer {token}"))
    }
    fn json(mut self, v: &Value) -> Wire {
        self.body = Some(serde_json::to_vec(v).unwrap_or_default());
        self
    }
    fn body(mut self, b: Vec<u8>) -> Wire {
        self.body = Some(b);
        self
    }
    fn timeout(mut self, d: Duration) -> Wire {
        self.timeout = Some(d);
        self
    }

    /// The request head exactly as Bun writes it.
    fn headers(&self) -> Vec<(String, String)> {
        let host = match reqwest::Url::parse(&self.url) {
            Ok(u) => match u.port() {
                Some(p) => format!("{}:{}", u.host_str().unwrap_or(""), p),
                None => u.host_str().unwrap_or("").to_string(),
            },
            Err(_) => String::new(),
        };
        let mut user = self.headers.clone();
        user.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let has_ua = user.iter().any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
        let mut out = user;
        out.push(("Connection".into(), "keep-alive".into()));
        if !has_ua {
            out.push(("User-Agent".into(), BUN_UA.into()));
        }
        out.push(("Accept".into(), "*/*".into()));
        out.push(("Host".into(), host));
        out.push(("Accept-Encoding".into(), "gzip, deflate, br, zstd".into()));
        let needs_len = self.body.is_some()
            || matches!(self.method, reqwest::Method::POST | reqwest::Method::DELETE | reqwest::Method::PUT | reqwest::Method::PATCH);
        if needs_len {
            out.push(("Content-Length".into(), self.body.as_ref().map(|b| b.len()).unwrap_or(0).to_string()));
        }
        out
    }

    fn send(self) -> reqwest::Result<reqwest::blocking::Response> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let url = reqwest::Url::parse(&self.url).expect("valid freebuff url");
        let mut map = HeaderMap::new();
        for (k, v) in self.headers() {
            if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
                map.append(n, v);
            }
        }
        let mut b = crate::api::http().request(self.method, url).headers(map);
        if let Some(body) = self.body {
            b = b.body(body);
        }
        if let Some(t) = self.timeout {
            b = b.timeout(t);
        }
        b.send()
    }
}

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;
/// A session hour, and the grace that follows it while the model still works.
const HOUR_MS: i64 = 3_600_000;
const GRACE_MS: i64 = 30 * 60_000;
const CATALOG_TTL: i64 = 30 * 60_000;
const SESSION_TTL: i64 = 8_000;

/// A Freebuff model Oak offers. `id` is the legacy model id the service still
/// accepts, `agent` the Freebuff agent definition whose system prompt the
/// service requires with it.
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
    pub agent: &'static str,
    pub context: u64,
    pub efforts: &'static [&'static str],
}

pub const MODELS: &[Model] = &[
    Model {
        id: "deepseek/deepseek-v4-flash",
        name: "DeepSeek V4.1 Flash 1017",
        agent: "base3-free-deepseek-flash",
        context: 1_048_576,
        efforts: &["low", "high", "max"],
    },
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn serves(id: &str) -> bool {
    model(id).is_some()
}

#[derive(Clone)]
struct Cred {
    token: String,
    id: String,
    email: String,
}

#[derive(Clone)]
struct Pending {
    fingerprint: String,
    hash: String,
    expires: String,
}

/// An account and everything cached for it: its catalog (and fetch id),
/// its session hour. Another account must not inherit any of it.
#[derive(Clone)]
struct Acct {
    cred: Cred,
    /// A copy of the vault's account.
    remote: bool,
    catalog: Option<Value>,
    /// When the held catalog should be refetched (`refreshAt`, else 30 min).
    catalog_until: i64,
    session: Option<Value>,
    session_at: i64,
    hour_started: Option<i64>,
    grace_until: Option<i64>,
    error: Option<String>,
}

impl Acct {
    fn new(cred: Cred, remote: bool) -> Acct {
        Acct { cred, remote, catalog: None, catalog_until: 0, session: None, session_at: 0, hour_started: None, grace_until: None, error: None }
    }
}

#[derive(Default)]
struct State {
    list: Vec<Acct>,
    pending: Option<Pending>,
    /// Why an account is on this machine although a vault is
    /// connected, when that was not asked for.
    note: Option<String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

thread_local! {
    /// The account this thread's requests go through (a request's thread
    /// picks it, `stream`); elsewhere the first.
    static USING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` on the account in use (see `USING`), if there is one.
fn with_acct<T>(f: impl FnOnce(&mut Acct) -> T) -> Option<T> {
    let using = USING.with(|u| u.borrow().clone());
    let mut s = STATE.lock().unwrap();
    let i = using.and_then(|id| s.list.iter().position(|a| a.cred.id == id)).unwrap_or(0);
    s.list.get_mut(i).map(f)
}

// ---------------------------------------------------------------------------
// Synthetic device identity
// ---------------------------------------------------------------------------

fn profile() -> Value {
    let path = crate::store::dir().join("freebuff-device.json");
    if let Some(v) = crate::store::read_json::<Value>(&path)
    {
        return v;
    }
    let mut seed = [0u8; 32];
    for b in seed.iter_mut() {
        *b = rand::random::<u8>();
    }
    let hex = |label: &str, n: usize, alphabet: &[u8]| -> String {
        let mut h = Sha256::new();
        h.update(seed);
        h.update(label.as_bytes());
        let d = h.finalize();
        (0..n).map(|i| alphabet[d[i % d.len()] as usize % alphabet.len()] as char).collect()
    };
    let up = b"0123456789ABCDEF";
    let alnum = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let uuid_raw = hex("uuid", 32, up);
    let p = json!({
        "system": {
            "manufacturer": "Dell Inc.",
            "model": "XPS 15 9530",
            "serial": hex("serial", 7, alnum),
            "uuid": format!("{}-{}-{}-{}-{}", &uuid_raw[0..8], &uuid_raw[8..12], &uuid_raw[12..16], &uuid_raw[16..20], &uuid_raw[20..32]),
        },
        "cpu": {"manufacturer": "GenuineIntel", "brand": "13th Gen Intel(R) Core(TM) i7-13700H", "cores": 16, "physicalCores": 12},
        "os": {"platform": "win32", "distro": "Microsoft Windows 11 Pro", "arch": "x64", "hostname": format!("DESKTOP-{}", hex("host", 7, alnum))},
        "runtime": {"nodeVersion": "v24.3.0", "platform": "win32", "arch": "x64", "shell": "bash", "cpuCount": 16},
        "network": {"macAddresses": [format!("{}-{}-{}-{}-{}-{}", hex("m1a",2,up), hex("m1b",2,up), hex("m1c",2,up), hex("m1d",2,up), hex("m1e",2,up), hex("m1f",2,up))], "interfaceCount": 3},
        "machineId": hex("machine", 32, up),
        "fingerprintVersion": "2.0",
    });
    let _ = crate::store::write_private_json(&path, &p);
    p
}

/// `enhanced-<base64url sha256 of the profile JSON>`.
pub fn fingerprint() -> String {
    let p = profile();
    let digest = Sha256::digest(p.to_string().as_bytes());
    format!("enhanced-{}", B64.encode(digest))
}

fn key_path() -> std::path::PathBuf {
    crate::store::dir().join("freebuff-device-key.json")
}

fn key_record(create: bool) -> Option<Value> {
    if let Some(v) = crate::store::read_json::<Value>(&key_path())
        && v["version"].as_u64() == Some(1)
    {
        return Some(v);
    }
    if !create {
        return None;
    }
    let mut rng = rand::rng();
    let signing = SigningKey::generate(&mut rng);
    let public = B64.encode(signing.verifying_key().as_bytes());
    // PKCS#8 v1 around the raw Ed25519 seed, the reference client's format.
    let mut der = vec![
        0x30u8, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
        0x04, 0x20,
    ];
    der.extend_from_slice(&signing.to_bytes());
    let record = json!({
        "version": 1,
        "publicKey": public,
        "privateKey": B64.encode(der),
        "registrations": {},
    });
    let _ = crate::store::write_private(&key_path(), serde_json::to_string(&record).unwrap_or_default());
    Some(record)
}

fn signing_key(record: &Value) -> Option<SigningKey> {
    let der = B64.decode(record["privateKey"].as_str()?).ok()?;
    if der.len() < 32 {
        return None;
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&der[der.len() - 32..]);
    Some(SigningKey::from_bytes(&seed))
}

/// The three device headers for a request (empty when there is no key).
/// The caller only asks for these while a catalog is held: the CLI signs
/// nothing before its first catalog fetch (and never registers a key then).
fn device_headers(
    cred: &Cred,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    fetch_id: Option<&str>,
    register: bool,
) -> Vec<(String, String)> {
    let Some(mut record) = key_record(register) else { return vec![] };
    let Some(key) = signing_key(&record) else { return vec![] };
    let scope = format!("{API} user:{}", cred.id);
    let key_id = match record["registrations"][&scope].as_str().map(String::from) {
        Some(id) => id,
        None if register => {
            let resp = Wire::post(format!("{API}/api/v1/freebuff/device-keys"))
                .bearer(&cred.token)
                .header("content-type", "application/json")
                .json(&json!({"publicKey": record["publicKey"], "client": "cli"}))
                .timeout(Duration::from_secs(15))
                .send();
            let Ok(resp) = resp else { return vec![] };
            if !resp.status().is_success() {
                return vec![];
            }
            let Ok(v) = resp.json::<Value>() else { return vec![] };
            let Some(id) = v["keyId"].as_str().map(String::from) else { return vec![] };
            record["registrations"][&scope] = json!(id);
            let _ = crate::store::write_private(&key_path(), serde_json::to_string(&record).unwrap_or_default());
            id
        }
        None => return vec![],
    };
    let ts = crate::api::now_ms();
    let body_sha = crate::api::hex(&Sha256::digest(body.unwrap_or(b"")));
    let path = reqwest::Url::parse(url).map(|u| u.path().to_string()).unwrap_or_default();
    let payload = format!(
        "freebuff-device-v1\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
        ts,
        body_sha,
        fetch_id.unwrap_or("")
    );
    let sig = key.sign(payload.as_bytes());
    vec![
        ("x-freebuff-device-key".into(), key_id),
        ("x-freebuff-device-ts".into(), ts.to_string()),
        ("x-freebuff-device-sig".into(), B64.encode(sig.to_bytes())),
    ]
}

/// The legacy-id → catalog-handle digest (`freebuffLegacyModelDigest`).
fn legacy_digest(id: &str) -> String {
    let input = format!("freebuff-legacy-model:{id}");
    let (mut h1, mut h2) = (0x811c_9dc5u32, 0x0100_0193u32 ^ 0x5bd1_e995u32);
    for unit in input.encode_utf16() {
        let c = unit as u32;
        h1 = (h1 ^ c).wrapping_mul(0x0100_0193);
        h2 = (h2 ^ c).wrapping_mul(0x5bd1_e995);
    }
    format!("{h1:08x}{h2:08x}")
}

// ---------------------------------------------------------------------------
// Login and stored credentials
// ---------------------------------------------------------------------------

fn cred_path() -> std::path::PathBuf {
    crate::store::dir().join("freebuff.json")
}

/// freebuff.json: the accounts (one, from before there could be more).
fn read_file() -> Vec<(Cred, bool)> {
    let Some(v) = crate::store::read_json::<Value>(&cred_path()) else { return vec![] };
    let one = |v: &Value| {
        let token = v["token"].as_str().filter(|t| !t.is_empty())?;
        let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Some((Cred { token: token.to_string(), id: field("id"), email: field("email") }, v["remote"] == true))
    };
    match v["accounts"].as_array() {
        Some(list) => list.iter().filter_map(one).collect(),
        None => one(&v).into_iter().collect(),
    }
}

fn save(s: &State) {
    if s.list.is_empty() {
        let _ = std::fs::remove_file(cred_path());
        return;
    }
    let list: Vec<Value> = s.list.iter().map(|a| json!({"token": a.cred.token, "id": a.cred.id, "email": a.cred.email, "remote": a.remote})).collect();
    let _ = crate::store::write_private_json(&cred_path(), &json!({"accounts": list}));
}

/// Reads freebuff.json (the tokens of the signed-in Freebuff accounts).
pub fn load() -> bool {
    let creds = read_file();
    let mut s = STATE.lock().unwrap();
    let old = std::mem::take(&mut s.list);
    s.list = creds
        .into_iter()
        .map(|(c, remote)| match old.iter().find(|a| a.cred.id == c.id && a.cred.token == c.token) {
            Some(a) => Acct { remote, ..a.clone() },
            None => Acct::new(c, remote),
        })
        .collect();
    !s.list.is_empty()
}

pub fn on() -> bool {
    !STATE.lock().unwrap().list.is_empty()
}

/// A Freebuff account as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    pub email: String,
    /// The vault's.
    pub remote: bool,
    pub error: Option<String>,
}

/// The accounts, in order.
pub fn accounts() -> Vec<Account> {
    STATE.lock().unwrap().list.iter().map(|a| Account { id: a.cred.id.clone(), email: a.cred.email.clone(), remote: a.remote, error: a.error.clone() }).collect()
}

/// The first account's email and id.
pub fn account() -> Option<(String, String)> {
    STATE.lock().unwrap().list.first().map(|a| (a.cred.email.clone(), a.cred.id.clone()))
}

/// Signs account `id` out: one on the vault is removed there,
/// for every OAK (when it can't be reached, it stays, and Err says why).
pub fn sign_out(id: &str) -> Result<(), String> {
    let remote = STATE.lock().unwrap().list.iter().find(|a| a.cred.id == id).map(|a| (a.remote, a.cred.token.clone()));
    if let Some((true, token)) = remote {
        crate::api::vault_remove("freebuff", &crate::provider::key_id(&token))?;
    }
    forget(id);
    Ok(())
}

/// Drops account `id` on this machine, with its runs.
fn forget(id: &str) {
    let mut s = STATE.lock().unwrap();
    s.list.retain(|a| a.cred.id != id);
    s.pending = None;
    s.note = None;
    save(&s);
    drop(s);
    TURNS.lock().unwrap().retain(|k, _| !k.starts_with(&format!("{id}\n")));
    crate::rotation::forget_account(crate::api::Provider::Freebuff, id);
}

/// Keeps account `cred` (`remote`: the vault's), beside the
/// others; one already kept is updated.
fn keep(cred: Cred, remote: bool) {
    let mut s = STATE.lock().unwrap();
    match s.list.iter_mut().find(|a| a.cred.id == cred.id) {
        Some(a) if a.cred.token == cred.token => a.remote = remote,
        Some(a) => *a = Acct::new(cred, remote),
        None => s.list.push(Acct::new(cred, remote)),
    }
    save(&s);
}

/// The vault's accounts (`key` the token, `label` the email,
/// `user` the id): Err when it can't say.
fn remote_accounts() -> Result<Vec<Cred>, String> {
    Ok(crate::api::vault_list("freebuff")?
        .iter()
        .filter_map(|k| {
            let token = k["key"].as_str().filter(|t| !t.is_empty())?;
            Some(Cred { token: token.into(), id: k["user"].as_str().unwrap_or("").into(), email: k["label"].as_str().unwrap_or("").into() })
        })
        .collect())
}

/// Brings the accounts in step with the vault's.
pub fn sync() {
    let Ok(list) = remote_accounts() else { return };
    let mut s = STATE.lock().unwrap();
    let before: Vec<(String, bool)> = s.list.iter().map(|a| (a.cred.id.clone(), a.remote)).collect();
    s.list.retain(|a| !a.remote || list.iter().any(|c| c.token == a.cred.token));
    for c in list {
        match s.list.iter_mut().find(|a| a.cred.token == c.token) {
            Some(a) => a.remote = true,
            None => s.list.push(Acct::new(c, true)),
        }
    }
    if s.list.iter().map(|a| (a.cred.id.clone(), a.remote)).collect::<Vec<_>>() != before {
        save(&s);
    }
}

/// Disconnected from the vault: its accounts go.
pub fn forget_remote() {
    let mut s = STATE.lock().unwrap();
    let n = s.list.len();
    s.list.retain(|a| !a.remote);
    if s.list.len() != n {
        save(&s);
    }
}

fn upload(c: &Cred) -> Result<(), String> {
    crate::api::vault_add("freebuff", &json!({"key": c.token, "label": c.email, "user": c.id})).map(|_| ())
}

/// Account `id` is on this machine while a vault is connected.
pub fn movable(id: &str) -> bool {
    crate::api::vault_on() && STATE.lock().unwrap().list.iter().any(|a| a.cred.id == id && !a.remote)
}

/// Why an account stayed on this machine, when it was meant for the
/// vault.
pub fn note() -> Option<String> {
    STATE.lock().unwrap().note.clone()
}

/// Puts account `id` on the vault, for every OAK.
pub fn move_to_vault(id: &str) -> Result<(), String> {
    let c = STATE.lock().unwrap().list.iter().find(|a| a.cred.id == id).map(|a| a.cred.clone()).ok_or("There is no such Freebuff account.")?;
    upload(&c)?;
    keep(c, true);
    STATE.lock().unwrap().note = None;
    Ok(())
}

/// Starts the browser login: returns the URL to open. The token is collected
/// by `login_poll` once the user approves it in the browser.
pub fn login_start() -> Result<String, String> {
    let fingerprint = fingerprint();
    let resp = Wire::post(format!("{WEB}/api/auth/cli/code"))
        .header("Content-Type", "application/json")
        .json(&json!({ "fingerprintId": fingerprint }))
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(|_| "Couldn't reach Freebuff.".to_string())?;
    let v: Value = resp.json().map_err(|_| "Freebuff sent an invalid login answer.".to_string())?;
    let Some(url) = v["loginUrl"].as_str() else { return Err("Freebuff sent an invalid login answer.".into()) };
    let pending = Pending {
        fingerprint,
        hash: v["fingerprintHash"].as_str().unwrap_or("").to_string(),
        expires: {
            let e = &v["expiresAt"];
            e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())
        },
    };
    STATE.lock().unwrap().pending = Some(pending);
    Ok(url.to_string())
}

/// One poll of the login; on success the token is saved and `true` returned.
pub fn login_poll() -> bool {
    let pending = STATE.lock().unwrap().pending.clone();
    let Some(p) = pending else { return false };
    let url = format!(
        "{WEB}/api/auth/cli/status?fingerprintId={}&fingerprintHash={}&expiresAt={}",
        crate::web::percent(&p.fingerprint),
        crate::web::percent(&p.hash),
        crate::web::percent(&p.expires)
    );
    let Ok(resp) = Wire::get(url).timeout(Duration::from_secs(15)).send() else { return false };
    let Ok(v) = resp.json::<Value>() else { return false };
    let Some(user) = v["user"].as_object() else { return false };
    let Some(token) = user.get("authToken").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) else { return false };
    let cred = Cred {
        token: token.to_string(),
        id: user.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        email: user.get("email").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    };
    STATE.lock().unwrap().pending = None;
    // Connected to a vault, the account goes there; one from
    // before it kept Freebuff accounts leaves it here.
    let mut note = None;
    let remote = crate::api::signing_in_remotely()
        && match upload(&cred) {
            Ok(()) => true,
            Err(e) => {
                note = Some(format!("Kept on this machine: {e}"));
                false
            }
        };
    keep(cred, remote);
    STATE.lock().unwrap().note = note;
    true
}

// ---------------------------------------------------------------------------
// Catalog and session
// ---------------------------------------------------------------------------

fn cred() -> Option<Cred> {
    with_acct(|a| a.cred.clone())
}

pub fn error() -> Option<String> {
    with_acct(|a| a.error.clone()).flatten()
}

/// The catalog's fetch id, for a signed request (`GET /freebuff/models`
/// returns it alongside the rows). The signature binds it, and the request
/// names it in `x-freebuff-catalog-fetch`.
fn fetch_id() -> Option<String> {
    with_acct(|a| a.catalog.as_ref().and_then(|c| c["fetchId"].as_str().map(String::from))).flatten()
}

/// Whether a catalog is held: the CLI only signs (and registers its device
/// key) while one is.
fn catalog_held() -> bool {
    with_acct(|a| a.catalog.is_some()).unwrap_or(false)
}

/// The headers a session call carries while a catalog is held.
fn catalog_headers(req: Wire) -> Wire {
    match with_acct(|a| a.catalog.clone()).flatten() {
        None => req,
        Some(c) => match c["fetchId"].as_str() {
            Some(f) => req.header("x-freebuff-catalog-protocol", "1").header("x-freebuff-catalog-fetch", f),
            None => req.header("x-freebuff-catalog-protocol", "1"),
        },
    }
}

/// The signed device headers, when a catalog is held.
fn sign(req: Wire, cred: &Cred, method: &str, url: &str, body: Option<&[u8]>) -> Wire {
    if !catalog_held() {
        return req;
    }
    let fetch = fetch_id();
    let mut req = req;
    for (k, v) in device_headers(cred, method, url, body, fetch.as_deref(), true) {
        req = req.header(&k, v);
    }
    req
}

/// The signed model catalog (fetched at most every 30 minutes). Signing is
/// what makes the service return the full catalog instead of the one-row
/// free picker; the first fetch carries no device headers, because the CLI
/// only signs while it already holds a catalog.
pub fn catalog() -> Option<Value> {
    let held = with_acct(|a| a.catalog.clone().filter(|_| crate::api::now_ms() < a.catalog_until)).flatten();
    held.or_else(fetch_catalog)
}

/// Fetches a fresh catalog without dropping the held one first (also on the
/// server's `freebuff_catalog_stale` answer): the held catalog's fetch id
/// signs the refresh, and it stays in place if the refresh fails.
fn fetch_catalog() -> Option<Value> {
    let cred = cred()?;
    let url = format!("{API}/api/v1/freebuff/models");
    let req = Wire::get(&url)
        .bearer(&cred.token)
        .header("x-freebuff-catalog-protocol", "1")
        .header("x-freebuff-client", "cli")
        .timeout(Duration::from_secs(20));
    // A refresh signs with the fetch id it already holds; the first fetch
    // goes unsigned.
    let fetched: Option<Value> = sign(req, &cred, "GET", &url, None)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json().ok());
    with_acct(|s| match fetched {
        Some(v) => {
            let now = crate::api::now_ms();
            s.catalog_until = v["refreshAt"]
                .as_str()
                .and_then(crate::time::parse_time)
                .unwrap_or(now + CATALOG_TTL);
            s.catalog = Some(v.clone());
            Some(v)
        }
        None => s.catalog.clone(),
    })
    .flatten()
}

/// The wire model id: the catalog handle when one is known, else the legacy id.
pub fn wire_model(id: &str) -> String {
    if std::env::var("OAK_FREEBUFF_LEGACY").is_ok() {
        return id.to_string();
    }
    catalog_row(id).and_then(|r| r["handle"].as_str().map(String::from)).unwrap_or_else(|| id.to_string())
}

/// The stored session payload (`GET /api/v1/freebuff/session`), refreshed at
/// most every 8 seconds.
pub fn session() -> Option<Value> {
    if let Some(v) = with_acct(|s| s.session.clone().filter(|_| crate::api::now_ms() - s.session_at < SESSION_TTL)).flatten() {
        return Some(v);
    }
    refresh_session();
    with_acct(|s| s.session.clone()).flatten()
}

pub fn refresh_session() {
    let Some(cred) = cred() else { return };
    let url = format!("{API}/api/v1/freebuff/session");
    let req = session_call(reqwest::Method::GET, &url, &cred, 20, |r| {
        r.header("x-freebuff-heartbeat", "1").header("x-freebuff-include-unused-rate-limits", "1")
    });
    let Ok(resp) = req.send() else { return };
    if !resp.status().is_success() {
        with_acct(|s| s.session = None);
        return;
    }
    let Ok(v) = resp.json::<Value>() else { return };
    with_acct(|s| {
        s.session = Some(v);
        s.session_at = crate::api::now_ms();
    });
}

/// `2026-11-04T07:00:00.000Z` (a step's `startTime`).
fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `cli:<uuid>`, kept in Oak's folder: every run reuses one instance id so
/// it can take over (and release) the session its earlier run left.
static INSTANCE: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(load_instance()));

fn load_instance() -> String {
    let path = crate::store::dir().join("freebuff-instance.json");
    if let Some(v) = crate::store::read_json::<Value>(&path)
        && let Some(id) = v["instanceId"].as_str().filter(|i| valid_instance(i))
    {
        return id.to_string();
    }
    save_instance()
}

/// A new instance id, saved as this machine's claim.
fn save_instance() -> String {
    let id = format!("cli:{}", crate::api::new_uuid());
    let path = crate::store::dir().join("freebuff-instance.json");
    let _ = crate::store::write_private(&path, serde_json::to_string(&json!({"instanceId": id})).unwrap_or_default());
    id
}

/// `cli:` plus a version-4 UUID, the claim format the server accepts.
fn valid_instance(id: &str) -> bool {
    let Some(u) = id.strip_prefix("cli:") else { return false };
    let g: Vec<&str> = u.split('-').collect();
    g.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&g).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
        && g[2].starts_with('4')
        && matches!(g[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

/// This machine's instance id (`cli:<uuid>`), persisted as the claim.
fn instance() -> String {
    INSTANCE.lock().unwrap().clone()
}

fn attempt() -> String {
    instance().strip_prefix("cli:").map(String::from).unwrap_or_else(instance)
}

/// The server retired our claim: persist a new id before trying again.
fn reset_instance() -> String {
    let id = save_instance();
    *INSTANCE.lock().unwrap() = id.clone();
    id
}

/// A session call (`/freebuff/session…`, timing out after `secs`) with what
/// every one carries: the client's environment, instance and version, the
/// time zone and the catalog's headers, `own` adding its own, then signed.
/// (Wire sorts the headers, so their order here doesn't matter.)
fn session_call(method: reqwest::Method, url: &str, cred: &Cred, secs: u64, own: impl FnOnce(Wire) -> Wire) -> Wire {
    let req = Wire::new(method.clone(), url)
        .bearer(&cred.token)
        .header("x-freebuff-env", CLIENT_ENV)
        .header("x-freebuff-multi-session", "1")
        .header("x-freebuff-purchase-continuity", "1")
        .header("x-freebuff-instance-id", instance())
        .header("x-freebuff-client-version", CLIENT_VERSION)
        .timeout(Duration::from_secs(secs));
    sign(catalog_headers(tz_header(own(req))), cred, method.as_str(), url, None)
}

/// The admission request for `wire`, taking over from `holder` when given.
fn admission(cred: &Cred, url: &str, wire: &str, holder: Option<&str>) -> Wire {
    session_call(reqwest::Method::POST, url, cred, 30, |r| {
        let r = r.header("x-freebuff-desktop-attempt-id", attempt()).header("x-freebuff-model", wire).header("x-freebuff-wallet-spend-limit", "session");
        match holder {
            Some(h) => r.header("x-freebuff-takeover-instance-id", h),
            None => r,
        }
    })
}

/// `DELETE /api/v1/freebuff/session/attempt`: this instance's claim.
fn delete_attempt(cred: &Cred) -> Option<reqwest::blocking::Response> {
    let url = format!("{API}/api/v1/freebuff/session/attempt");
    session_call(reqwest::Method::DELETE, &url, cred, 10, |r| r.header("x-freebuff-desktop-attempt-id", attempt())).send().ok()
}

/// Starts an hour on `model`. Returns the server's answer.
/// Held while a session is being admitted or restarted: agents starting
/// at once would otherwise cancel each other's attempts
/// (admission_attempt_closed), and each reset the instance.
static ADMITTING: Mutex<()> = Mutex::new(());

pub fn admit(model: &str) -> Result<Value, String> {
    let _one = ADMITTING.lock().unwrap_or_else(|e| e.into_inner());
    admit_locked(model)
}

fn admit_locked(model: &str) -> Result<Value, String> {
    let cred = cred().ok_or("Sign in to Freebuff first.")?;
    let wire = wire_model(model);
    // The client's poll loop hits GET before it admits: that registers this
    // instance's claim, and it tells us whether an hour is already running
    // for this model (then there is nothing to admit).
    refresh_session();
    let current = with_acct(|s| s.session.clone()).flatten();
    if let Some(v) = &current
        && v["status"].as_str() == Some("active")
    {
        let m = v["model"].as_str().unwrap_or("");
        // Only our own instance's hour counts: another tab's does not cover
        // our requests, and the completion would be refused.
        let ours = [v["instanceId"].as_str(), v["currentInstanceId"].as_str()]
            .iter()
            .flatten()
            .any(|id| *id == instance().as_str());
        if (m == wire || m == model) && ours {
            with_acct(|s| {
                s.hour_started.get_or_insert(crate::api::now_ms());
            });
            return Ok(v.clone());
        }
        // Another model holds the hour: release this attempt before admitting.
        let _ = delete_attempt(&cred);
    }
    let url = format!("{API}/api/v1/freebuff/session/admission");
    let resp = admission(&cred, &url, &wire, None).send().map_err(|_| "Couldn't reach Freebuff.".to_string())?;
    let mut status = resp.status();
    let mut v: Value = resp.json().unwrap_or(Value::Null);
    // A session held by another instance (an Oak that died mid-hour, another
    // tab): take it over, naming the holder the refusal named.
    // A retired claim: persist a new instance id and try again.
    if status.as_u16() == 409 && v["status"].as_str() == Some("purchase_claim_released") {
        reset_instance();
    }
    let holder = v["currentInstanceId"].as_str().map(String::from);
    let closed = status.as_u16() == 409 && v["error"].as_str() == Some("admission_attempt_closed");
    if closed {
        reset_instance();
    }
    if (status.as_u16() == 409 || status.as_u16() == 410) && (holder.is_some() || closed)
        && let Ok(r) = admission(&cred, &url, &wire, holder.as_deref()).send()
    {
        status = r.status();
        v = r.json().unwrap_or(Value::Null);
    }
    if status.is_success() {
        with_acct(|s| {
            s.hour_started = Some(crate::api::now_ms());
            s.grace_until = None;
            s.session = Some(v.clone());
            s.session_at = crate::api::now_ms();
        });
    }
    if !status.is_success() {
        if let Some(hours) = v["availableHours"].as_str() {
            let name = crate::freebuff::model(model).map(|m| m.name).unwrap_or(model);
            return Err(format!("{name} can't be started on this Freebuff plan: {hours}."));
        }
        let msg = v["error"].as_str().or(v["message"].as_str()).unwrap_or("Freebuff refused the session.");
        return Err(format!("{msg} ({} {})", status.as_u16(), v.to_string().head(200)));
    }
    Ok(v)
}

/// Releases this machine's session slot.
pub fn release() {
    let Some(cred) = cred() else { return };
    let _ = delete_attempt(&cred);
    with_acct(|s| {
        s.hour_started = None;
        s.grace_until = None;
        s.session = None;
    });
    reset_alerts();
}

// ---------------------------------------------------------------------------
// The catalog and the session hour
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hour {
    /// A session hour is running.
    Active { left_ms: i64 },
    /// The hour ran out while the model was still working.
    Grace { left_ms: i64 },
    /// Nothing running.
    None,
}

/// Whether a model is in the account's current catalog (free or paid).
/// Legacy ids the free catalog dropped are refused with "Included with a
/// paid plan" when admitted, so the picker says so up front.
pub fn in_catalog(id: &str) -> bool {
    if catalog().is_none_or(|c| c["rows"].as_array().is_none()) {
        return true;
    }
    !premium(id)
}

/// The catalog row for a model id, by key or legacy digest.
fn catalog_row(id: &str) -> Option<Value> {
    let c = catalog()?;
    let rows = c["rows"].as_array()?;
    if let Some(r) = rows.iter().find(|r| r["key"].as_str() == Some(id)) {
        return Some(r.clone());
    }
    let digest = legacy_digest(id);
    rows.iter().find(|r| r["legacyDigests"].as_array().is_some_and(|a| a.iter().any(|d| d.as_str() == Some(&digest)))).cloned()
}

/// When a model opens on this account, if it is not open yet.
pub fn opens_at(id: &str) -> Option<i64> {
    let row = catalog_row(id)?;
    let at = row["opensAt"].as_i64()?;
    (at > crate::api::now_ms()).then_some(at)
}

/// Whether the catalog lists this model for a paid plan only.
pub fn premium(id: &str) -> bool {
    catalog_row(id).is_some_and(|r| r["premium"].as_bool().unwrap_or(false))
}

/// The state of the session hour: the server's expiry when the session is
/// active, else the local hour started by `admit`.
pub fn hour() -> Hour {
    let now = crate::api::now_ms();
    let server = session().unwrap_or(Value::Null);
    let expires = server["expiresAt"].as_str().and_then(crate::time::parse_time).or_else(|| server["session"]["expiresAt"].as_str().and_then(crate::time::parse_time));
    // A desktop purchase carries the real end of the hour: it is what the
    // server counts down, so the timer cannot outlive the session.
    let purchase = server["desktopPurchases"].as_array().and_then(|a| {
        a.iter()
            .filter_map(|p| p["expiresAt"].as_str().and_then(crate::time::parse_time))
            .filter(|t| *t > now)
            .max()
    });
    let started = with_acct(|s| s.hour_started).flatten();
    let active = server["status"].as_str() == Some("active");
    if active || started.is_some() {
        let end = expires.or(purchase).or_else(|| started.map(|s| s + HOUR_MS));
        if let Some(end) = end {
            if end > now {
                return Hour::Active { left_ms: end - now };
            }
            let grace = with_acct(|s| s.grace_until).flatten().unwrap_or(end + GRACE_MS);
            if grace > now {
                return Hour::Grace { left_ms: grace - now };
            }
        }
    }
    Hour::None
}

// ---------------------------------------------------------------------------
// Chat completions
// ---------------------------------------------------------------------------

use crate::api::Sse;
use crate::oak::{self, Event, Request};
use std::io::BufRead;
use std::sync::mpsc::Sender;

/// The Freebuff system prompt: the CLI's own text, which the service checks.
/// It is the compiled base3 prompt with its two per-run values filled in:
/// the date, and the model name the prompt tells the model it is.
pub fn system_prompt(model: &str) -> String {
    fill_prompt(include_str!("../assets/freebuff-system.txt"), &prompt_label(model))
}

fn fill_prompt(base: &str, label: &str) -> String {
    let date = current_date();
    let mut out = String::with_capacity(base.len() + 16);
    for (i, line) in base.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.starts_with("Current date: ") {
            out.push_str("Current date: ");
            out.push_str(&date);
            out.push('.');
        } else if line.starts_with("You are running on the ") && line.ends_with(" model.") {
            out.push_str("You are running on the ");
            out.push_str(label);
            out.push_str(" model.");
        } else {
            out.push_str(line);
        }
    }
    if base.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `Intl.DateTimeFormat('en-US', {year, month:'long', day}).format(now)`.
fn current_date() -> String {
    use chrono::Datelike;
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let now = chrono::Local::now();
    format!("{} {}, {}", MONTHS[now.month0() as usize], now.day(), now.year())
}

/// The name the prompt says the model runs on: the catalog row's display name
/// in catalog mode (`freebuffCatalogRootAgent`), else the compiled model id.
fn prompt_label(model: &str) -> String {
    if let Some(row) = catalog_row(model)
        && let Some(name) = row["displayName"].as_str().filter(|n| !n.is_empty()) {
            return name.to_string();
        }
    model.to_string()
}

/// The CLI's tool schemas, which the service also expects.
fn cli_tools() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("../assets/freebuff-tools.json"))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// OAK's messages as chat-completions messages (Freebuff runs its own
/// system prompt, so OAK's isn't sent). Tool calls go back as the Freebuff
/// calls they came from, answered as the CLI answers them
/// (`freebuff_tools`); an answer always comes right after its call.
pub(crate) fn chat_messages(msgs: &[Value]) -> Vec<Value> {
    use crate::freebuff_tools::{self as ft, Origin, Part};
    let blocks = |m: &Value| m["content"].as_array().cloned().unwrap_or_default();
    let (mut origins, mut results) = (std::collections::HashMap::new(), std::collections::HashMap::new());
    for b in msgs.iter().flat_map(blocks) {
        match b["type"].as_str() {
            Some("tool_call") => drop(origins.insert(b["id"].as_str().unwrap_or("").to_string(), ft::origin(&b))),
            Some("tool_result") => drop(results.insert(b["call_id"].as_str().unwrap_or("").to_string(), b)),
            _ => {}
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for m in msgs {
        let role = m["role"].as_str().unwrap_or("user");
        let mut text = String::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut answers: Vec<Value> = Vec::new();
        // Images read_files attaches: user messages after all the answers,
        // as the CLI sends them (a tool message can't carry one).
        let mut media: Vec<Value> = Vec::new();
        for b in blocks(m) {
            match b["type"].as_str().unwrap_or("") {
                "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                "tool_call" => {
                    let id = b["id"].as_str().unwrap_or("");
                    if let Some(Origin::Call(c)) = origins.get(id)
                        && results.contains_key(id)
                    {
                        calls.push(json!({"id": id, "type": "function", "function": {"name": c.name, "arguments": c.args.to_string()}}));
                    }
                }
                "tool_result" => {
                    let id = b["call_id"].as_str().unwrap_or("");
                    // A part's answer goes with its call's; one without a call is dropped.
                    let Some(Origin::Call(c)) = origins.get(id) else { continue };
                    let parts = (0..c.parts.max(1)).map(|k| results.get(&if k == 0 { id.to_string() } else { format!("{id}_{k}") }).map(Part::of)).collect();
                    let (content, images) = ft::answer(c, parts);
                    answers.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                    media.extend(images.into_iter().map(|i| json!({"role": "user", "content": [i]})));
                }
                _ => {}
            }
        }
        out.extend(answers);
        out.extend(media);
        if !text.is_empty() || !calls.is_empty() {
            let mut msg = json!({"role": role, "content": text});
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(calls);
            }
            out.push(msg);
        }
    }
    out
}

/// What a request to `model` sends besides the conversation: Freebuff's own
/// system prompt (OAK's isn't sent) and the CLI's tools.
pub fn preamble(model: &str) -> (String, Vec<Value>) {
    (system_prompt(model), cli_tools())
}

/// Ends the session and starts a fresh hour, waiting for the server to settle.
fn restart_session(model: &str) -> Result<(), String> {
    let _one = ADMITTING.lock().unwrap_or_else(|e| e.into_inner());
    // Another request (a subagent's) restarted it a moment ago: that hour
    // is the fresh one, not to be released again.
    if with_acct(|s| s.hour_started).flatten().is_some_and(|t| crate::api::now_ms() - t < 60_000) {
        return Ok(());
    }
    release();
    std::thread::sleep(Duration::from_millis(700));
    match admit_locked(model) {
        Ok(_) => {
            std::thread::sleep(Duration::from_millis(400));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// `POST /api/v1/agent-runs` START: Freebuff refuses a run id it hasn't
/// seen. Err: the server's reason when it refuses (shown to the user:
/// "didn't register this run" alone can't be acted on).
fn start_run_why(agent: &str) -> Result<String, String> {
    let mut why = String::new();
    for _ in 0..3 {
        match start_run_once(agent) {
            Ok(id) => return Ok(id),
            Err(e) => why = e,
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    Err(why)
}

fn start_run_once(agent: &str) -> Result<String, String> {
    let cred = cred().ok_or("not signed in")?;
    let resp = Wire::post(format!("{API}/api/v1/agent-runs"))
        .bearer(&cred.token)
        .header("x-freebuff-acting-user-id", &cred.id)
        .json(&json!({"action": "START", "agentId": agent, "ancestorRunIds": []}))
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(|_| "couldn't reach Freebuff".to_string())?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    v["runId"].as_str().map(String::from).ok_or_else(|| {
        let msg = v["message"].as_str().or(v["error"].as_str()).map(String::from).unwrap_or_else(|| text.head(160));
        format!("{status}: {msg}")
    })
}

fn finish_run(run: &str, steps: Vec<Value>) {
    let Some(cred) = cred() else { return };
    let total = steps.len();
    let _ = Wire::post(format!("{API}/api/v1/agent-runs"))
        .bearer(&cred.token)
        .header("x-freebuff-acting-user-id", &cred.id)
        .json(&json!({
            "action": "FINISH", "runId": run, "status": "completed",
            "totalSteps": total, "directCredits": 0, "totalCredits": 0, "steps": steps,
        }))
        .timeout(Duration::from_secs(20))
        .send();
}

// ---------------------------------------------------------------------------
// The turn: one agent run per user turn, one step per model reply
// ---------------------------------------------------------------------------

/// In catalog mode the CLI runs one root, `base3-free-catalog`, for every
/// row; a compiled model keeps its own root.
const CATALOG_ROOT_AGENT: &str = "base3-free-catalog";

fn run_agent(m: &Model) -> &'static str {
    if catalog_row(m.id).is_some() { CATALOG_ROOT_AGENT } else { m.agent }
}

#[derive(Clone)]
struct Turn {
    /// What makes this user turn: the prompt count and the prompt text.
    key: String,
    run: String,
    step: u32,
    trace: String,
    client_id: String,
    profile: Option<String>,
    snapshot: Option<String>,
    steps: Vec<Value>,
}

/// One entry per conversation; a chat's turns share a trace id and each turn
/// has its own run, client id and input profile.
static TURNS: LazyLock<Mutex<std::collections::HashMap<String, Turn>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn hash_hex(s: &str) -> String {
    crate::api::hex(&Sha256::digest(s.as_bytes()))
}

/// The last typed text block of a user message (Oak's context block comes
/// first, so the prompt is the last text block; a rolling-summary
/// instruction sits after it and is not part of the prompt).
fn typed_text(m: &Value) -> Option<String> {
    m["content"]
        .as_array()?
        .iter()
        .rev()
        .filter(|b| b["type"].as_str() == Some("text"))
        .filter_map(|b| b["text"].as_str())
        .find(|t| !t.trim().is_empty() && !crate::rolling::is_housekeeping(t))
        .map(str::to_string)
}

/// The user prompts in the conversation: how many, the first and the last.
fn prompts(body: &Request) -> (u32, Option<String>, Option<String>) {
    let mut count = 0;
    let mut first = None;
    let mut last = None;
    for m in &body.messages {
        if m["role"].as_str() != Some("user") {
            continue;
        }
        if let Some(t) = typed_text(m) {
            count += 1;
            first.get_or_insert_with(|| t.clone());
            last = Some(t);
        }
    }
    (count, first, last)
}

fn random_base36(n: usize) -> String {
    use rand::RngExt;
    const CH: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut r = rand::rng();
    (0..n).map(|_| CH[r.random_range(0..CH.len())] as char).collect()
}

/// A UUID-shaped id derived from a seed (the CLI keeps one trace id per
/// conversation).
fn seeded_uuid(seed: &str) -> String {
    let hex = hash_hex(seed);
    let mut b: Vec<char> = hex.chars().take(32).collect();
    b[12] = '4';
    b[16] = match b[16] {
        '8' | '9' | 'a' | 'b' => b[16],
        _ => '8',
    };
    let s: String = b.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &s[0..8], &s[8..12], &s[12..16], &s[16..20], &s[20..32])
}

/// Starts the turn's run (or continues it) and gives the request its metadata.
fn begin_turn(body: &Request, m: &Model) -> Result<Turn, String> {
    let (count, first, last) = prompts(body);
    let hash = hash_hex(&format!("chat\n{}", first.unwrap_or_default()));
    // The same key `end_turn` and `add_step` use (chat_key): under the bare
    // hash a turn was never found again, so its run was FINISHed only when
    // the conversation's next turn began, and a deleted agent's last run
    // stayed open on the server, holding one of the account's 5 tabs.
    let chat = chat_key(body);
    let key = hash_hex(&format!("{count}\n{}", last.clone().unwrap_or_default()));
    let previous = {
        let mut turns = TURNS.lock().unwrap();
        if let Some(t) = turns.get_mut(&chat)
            && t.key == key {
                t.step += 1;
                return Ok(t.clone());
            }
        turns.remove(&chat)
    };
    if let Some(t) = previous {
        finish_run(&t.run, t.steps);
    }
    let profile = last.as_deref().and_then(take_input_profile);
    let snapshot = repo_snapshot(body.folder.as_deref());
    let run = start_run_why(run_agent(m)).map_err(|e| format!("Freebuff didn't register this run ({e}). Try again."))?;
    let turn = Turn {
        key,
        run,
        step: 1,
        trace: seeded_uuid(&format!("freebuff-trace\n{hash}")),
        client_id: random_base36(13),
        profile,
        snapshot,
        steps: Vec::new(),
    };
    TURNS.lock().unwrap().insert(chat, turn.clone());
    Ok(turn)
}

/// The conversation key a turn belongs to: its account's (a conversation
/// moved to another account starts its runs anew there).
fn chat_key(body: &Request) -> String {
    let (_, first, _) = prompts(body);
    let account = cred().map(|c| c.id).unwrap_or_default();
    format!("{account}\n{}", hash_hex(&format!("chat\n{}", first.unwrap_or_default())))
}

fn add_step(chat: &str, run: &str, step: Value) {
    if let Some(t) = TURNS.lock().unwrap().get_mut(chat)
        && t.run == run
    {
        t.steps.push(step);
    }
}

/// Ends the turn's run, if it is still the current one.
fn end_turn(chat: &str, run: &str) {
    let turn = {
        let mut turns = TURNS.lock().unwrap();
        match turns.get(chat) {
            Some(t) if t.run == run => turns.remove(chat),
            _ => None,
        }
    };
    if let Some(t) = turn {
        finish_run(&t.run, t.steps);
    }
}

/// The server restarted the session (or forgot the run): the turn keeps its
/// place in the conversation but gets a fresh run and a fresh step count.
fn new_run_for_turn(chat: &str, m: &Model) -> Option<Turn> {
    let old = TURNS.lock().unwrap().remove(chat);
    let old = old?;
    finish_run(&old.run, old.steps);
    let run = start_run_why(run_agent(m)).ok()?;
    let turn = Turn {
        run,
        step: 1,
        client_id: random_base36(13),
        steps: Vec::new(),
        ..old
    };
    TURNS.lock().unwrap().insert(chat.to_string(), turn.clone());
    Some(turn)
}

// ---------------------------------------------------------------------------
// Input profile
// ---------------------------------------------------------------------------

/// How the current composer text arrived, counted the way the CLI counts it.
#[derive(Default)]
struct InputCounter {
    typed: usize,
    keys: usize,
    multi: usize,
    pasted: usize,
    pastes: usize,
    first: Option<i64>,
    window: std::collections::VecDeque<(i64, usize)>,
    window_chars: usize,
    max_window: usize,
}

static INPUT: LazyLock<Mutex<InputCounter>> = LazyLock::new(Mutex::default);
/// Sealed profiles by the exact submitted text, oldest first (at most 8).
static SEALED: LazyLock<Mutex<Vec<(String, String)>>> = LazyLock::new(Mutex::default);

/// The composer inserted `chars` from a key event.
pub fn note_typed(chars: usize) {
    if chars == 0 {
        return;
    }
    let now = crate::api::now_ms();
    let mut c = INPUT.lock().unwrap();
    c.first.get_or_insert(now);
    c.typed += chars;
    c.keys += 1;
    if chars > 1 {
        c.multi += 1;
    }
    c.window.push_back((now, chars));
    c.window_chars += chars;
    while let Some((t, n)) = c.window.front() {
        if now - t < 1000 {
            break;
        }
        let (_, n) = (*t, *n);
        c.window_chars -= n;
        c.window.pop_front();
    }
    c.max_window = c.max_window.max(c.window_chars);
}

/// The composer received `chars` as one bracketed paste.
pub fn note_paste(chars: usize) {
    let now = crate::api::now_ms();
    let mut c = INPUT.lock().unwrap();
    c.first.get_or_insert(now);
    c.pastes += 1;
    c.pasted += chars;
}

fn encode_profile(c: &InputCounter) -> String {
    let now = crate::api::now_ms();
    let cap = |n: i64| n.clamp(0, 10_000_000);
    let mut out = String::from("v1");
    out += &format!(";tc={}", cap(c.typed as i64));
    out += &format!(";ke={}", cap(c.keys as i64));
    out += &format!(";mc={}", cap(c.multi as i64));
    out += &format!(";pc={}", cap(c.pasted as i64));
    out += &format!(";pe={}", cap(c.pastes as i64));
    if let Some(t) = c.first {
        out += &format!(";ms={}", cap(now - t));
    }
    out += &format!(";cps={}", cap(c.max_window as i64));
    out
}

/// Submitting seals the counts under the submitted text; the run that sends
/// that text takes them.
pub fn seal_input_profile(text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let profile = {
        let mut c = INPUT.lock().unwrap();
        let p = encode_profile(&c);
        *c = InputCounter::default();
        p
    };
    let mut sealed = SEALED.lock().unwrap();
    sealed.retain(|(t, _)| t != text);
    sealed.push((text.to_string(), profile));
    if sealed.len() > 8 {
        sealed.remove(0);
    }
}

fn take_input_profile(text: &str) -> Option<String> {
    let mut sealed = SEALED.lock().unwrap();
    let at = sealed.iter().position(|(t, _)| t == text.trim())?;
    Some(sealed.remove(at).1)
}

// ---------------------------------------------------------------------------
// Repository snapshot
// ---------------------------------------------------------------------------

/// `repo_snapshot`: the aggregate git facts the CLI sends with a run, from
/// this project. Cached for ten minutes (the CLI collects it once per thread).
fn repo_snapshot(root: Option<&str>) -> Option<String> {
    let root = root.filter(|r| !r.is_empty())?;
    static CACHE: LazyLock<Mutex<Option<(String, i64, String)>>> = LazyLock::new(|| Mutex::new(None));
    let now = crate::api::now_ms();
    if let Some((r, at, s)) = CACHE.lock().unwrap().as_ref()
        && r == root && now - *at < 10 * 60_000
    {
        return Some(s.clone());
    }
    let root = std::path::Path::new(root);
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let available = git(&["rev-parse", "--is-inside-work-tree"]).is_some();
    if !available {
        return None;
    }
    let files: Vec<String> = git(&["ls-files", "--cached", "--others", "--exclude-standard"])
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    let mut changed: Vec<String> = Vec::new();
    for args in [
        &["diff", "--name-only", "--"][..],
        &["diff", "--cached", "--name-only", "--"][..],
        &["ls-files", "--others", "--exclude-standard"][..],
    ] {
        if let Some(out) = git(args) {
            for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if !changed.iter().any(|c| c == line) {
                    changed.push(line.to_string());
                }
            }
        }
    }
    changed.sort();
    let commit_count = git(&["rev-list", "--count", "HEAD"]).and_then(|s| s.trim().parse::<u64>().ok());
    let shallow = git(&["rev-parse", "--is-shallow-repository"]).map(|s| s.trim() == "true").unwrap_or(false);
    let log = git(&["log", "--use-mailmap", "--format=%aN%x09%aE%x09%cs%x09%s", "HEAD"]);
    let mut dates: Vec<String> = Vec::new();
    let (mut humans, mut bots): (std::collections::HashSet<String>, std::collections::HashSet<String>) =
        (Default::default(), Default::default());
    let mut prs: std::collections::HashSet<String> = Default::default();
    for line in log.as_deref().unwrap_or("").lines() {
        let mut it = line.splitn(4, '\t');
        let name = it.next().unwrap_or("").trim();
        let email = it.next().unwrap_or("").trim();
        let date = it.next().unwrap_or("").trim();
        let subject = it.next().unwrap_or("").trim();
        if name.is_empty() && email.is_empty() {
            continue;
        }
        if date.len() == 10 && date.as_bytes().get(4) == Some(&b'-') {
            dates.push(date.to_string());
        }
        let who = format!("{name} <{email}>");
        let lower = who.to_lowercase();
        if lower.contains("[bot]") || lower.contains("bot@") || lower.contains("-bot") || lower.contains("(bot)") {
            bots.insert(who);
        } else {
            humans.insert(who);
        }
        if (subject.starts_with("Merge pull request #") || (subject.contains("(#") && subject.ends_with(')')))
            && let Some(hash) = subject.split('#').nth(1).map(|s| s.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").to_string()).filter(|s| !s.is_empty()) {
                prs.insert(hash);
            }
    }
    dates.sort();
    let pick = |p: f64| -> Option<String> {
        if dates.is_empty() {
            return None;
        }
        let i = ((dates.len() as f64 - 1.0) * p).round() as usize;
        Some(dates[i.min(dates.len() - 1)].clone())
    };
    let mut m = serde_json::Map::new();
    m.insert("gitAvailable".into(), json!(true));
    m.insert("repositoryVisibility".into(), json!("unknown"));
    m.insert("fileCount".into(), json!(files.len()));
    m.insert("fileCountIsLowerBound".into(), json!(false));
    m.insert("testFileCount".into(), json!(files.iter().filter(|f| is_test_file(f)).count()));
    if let Some(c) = commit_count {
        m.insert("commitCount".into(), json!(c));
    }
    m.insert("historyIsShallow".into(), json!(shallow));
    m.insert("historyScanTruncated".into(), json!(false));
    if let (Some(p0), Some(p25), Some(p50), Some(p75), Some(p100)) =
        (pick(0.0), pick(0.25), pick(0.5), pick(0.75), pick(1.0))
    {
        m.insert("commitDatePercentiles".into(), json!({"p0": p0, "p25": p25, "p50": p50, "p75": p75, "p100": p100}));
    }
    m.insert("mergedPullRequestCount".into(), json!(prs.len()));
    m.insert("humanContributorCount".into(), json!(humans.len()));
    m.insert("botContributorCount".into(), json!(bots.len()));
    m.insert("contributorCount".into(), json!(humans.len() + bots.len()));
    m.insert("changedFileCount".into(), json!(changed.len()));
    m.insert("changedFileScanTruncated".into(), json!(false));
    let s = Value::Object(m).to_string();
    *CACHE.lock().unwrap() = Some((root.to_string_lossy().into_owned(), now, s.clone()));
    Some(s)
}

fn is_test_file(rel: &str) -> bool {
    let lower = rel.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.contains("/__tests__/")
        || lower.contains("/tests/")
        || lower.contains("/test/")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("_test.go")
}

fn request_body(body: &Request, m: &Model, turn: &Turn) -> Value {
    build_request(body, &wire_model(m.id), &prompt_label(m.id), explicit_effort(body, m.id), turn)
}

/// The CLI reports an effort only when the user picked one for this model
/// (its own default effort is not reported); Oak's value that differs from
/// the catalog row's default is the closest equivalent.
fn explicit_effort(body: &Request, model: &str) -> Option<String> {
    let effort = body.effort.as_deref().filter(|e| !e.is_empty())?;
    let default = catalog_row(model).and_then(|r| r["defaultEffort"].as_str().map(String::from));
    (default.as_deref() != Some(effort)).then(|| effort.to_string())
}

fn build_request(body: &Request, wire: &str, label: &str, effort: Option<String>, turn: &Turn) -> Value {
    let mut msgs = vec![json!({"role": "system", "content": fill_prompt(include_str!("../assets/freebuff-system.txt"), label)})];
    msgs.extend(chat_messages(&body.messages_with_instruction()));
    let mut meta = json!({
        "freebuff_instance_id": instance(),
        "freebuff_multi_session": "1",
        "surface": "cli",
        "freebuff_client_env": CLIENT_ENV,
        "trace_session_id": turn.trace,
        "run_id": turn.run,
        "client_id": turn.client_id,
        "cost_mode": "free",
        "llm_step_number": turn.step.to_string(),
    });
    if let Some(effort) = effort {
        meta["freebuff_reasoning_effort"] = json!(effort);
    }
    if let Some(profile) = &turn.profile {
        meta["freebuff_input_profile"] = json!(profile);
    }
    if let Some(snapshot) = &turn.snapshot {
        meta["repo_snapshot"] = json!(snapshot);
    }
    let mut req = json!({
        "model": wire,
        "codebuff_metadata": meta,
        "provider": {"data_collection": "deny"},
        "messages": msgs,
        "stream": true,
    });
    let tools = cli_tools();
    if !tools.is_empty() {
        req["tools"] = Value::Array(tools);
    }
    req
}

/// Says a retry is coming (the client's own event), and how long it waits.
fn retry_event(tx: &Sender<Sse>, tries: u32) -> u64 {
    let wait = crate::api::retry_wait(tries);
    let _ = tx.send(Sse::Event(Event::Retry { attempt: tries, wait, reason: String::new() }));
    wait
}

fn open(req: &Value, tx: &Sender<Sse>) -> Result<reqwest::blocking::Response, Option<Sse>> {
    let Some(cred) = cred() else {
        return Err(Some(Sse::Err("Sign in to Freebuff first.".into())));
    };
    let url = format!("{API}/api/v1/chat/completions");
    let body = serde_json::to_vec(req).unwrap_or_default();
    let build = || {
        let mut r = Wire::post(&url)
            .bearer(&cred.token)
            .header("content-type", "application/json")
            .header("user-agent", AI_SDK_UA)
            .header("x-freebuff-sse-gzip-recovery", "1")
            .header("x-freebuff-acting-user-id", &cred.id)
            .header("x-freebuff-client-version", CLIENT_VERSION)
            .body(body.clone())
            .timeout(Duration::from_secs(600));
        if let Some(f) = fetch_id() {
            r = r.header("x-freebuff-catalog-fetch", f);
        }
        sign(r, &cred, "POST", &url, Some(&body)).send()
    };
    let mut tries = 0u32;
    loop {
        let result = match build() {
            Err(e) if crate::api::stale_connection(&e) => build(),
            r => r,
        };
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                tries += 1;
                if tries > crate::api::RETRIES {
                    return Err(Some(Sse::Err(crate::api::network_error(e))));
                }
                let wait = retry_event(tx, tries);
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }
        };
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = v["error"].as_str().unwrap_or("");
        let msg = v["message"].as_str().unwrap_or("");
        if status == 403 && code == "free_mode_cli_required" {
            return Err(Some(Sse::Err(format!("Freebuff refused this session: {msg}"))));
        }
        if status == 409 && (code == "freebuff_catalog_stale" || v["status"].as_str() == Some("freebuff_catalog_stale")) {
            return Err(Some(Sse::Err("freebuff_catalog_stale".into())));
        }
        if matches!(status, 408 | 429 | 500..=599) && tries < crate::api::RETRIES {
            tries += 1;
            let wait = retry_event(tx, tries);
            std::thread::sleep(Duration::from_secs(wait));
            continue;
        }
        let reason = if msg.is_empty() { text.head(300) } else { msg.to_string() };
        return Err(Some(Sse::Err(format!("Freebuff: {reason}"))));
    }
}

/// A Freebuff tool call as it streams in.
struct Tool {
    /// Its index in the stream's tool_calls, and its block's.
    n: usize,
    block: i64,
    id: String,
    name: String,
    args: String,
    /// Whether its block has opened (`freebuff_tools::early_name`).
    opened: bool,
}

#[derive(Default)]
struct Blocks {
    text: Option<i64>,
    think: Option<i64>,
    tools: Vec<Tool>,
    next: i64,
    started: bool,
    finished: bool,
    /// The response's `chatcmpl-…` id (the step's `messageId`).
    message_id: Option<String>,
    /// Whether the reply ended by asking for tools.
    tool_calls: bool,
    count: crate::nodeclub::ThinkCount,
}

impl Blocks {
    fn index(&mut self) -> i64 {
        let i = self.next;
        self.next += 1;
        i
    }

    /// One chunk of the reply as the Oak events it brings: blocks that
    /// open and close, and at its `finish_reason` the usage and stop reason.
    fn feed(&mut self, ev: &Value) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();
        if let Some(id) = ev["id"].as_str().filter(|i| !i.is_empty()) {
            self.message_id = Some(id.to_string());
        }
        for choice in ev["choices"].as_array().into_iter().flatten() {
            let d = &choice["delta"];
            if let Some(t) = d["reasoning_content"].as_str().or(d["reasoning"].as_str()).filter(|t| !t.is_empty()) {
                if self.think.is_none() {
                    let i = self.index();
                    self.think = Some(i);
                    out.push(Event::Block { index: i as usize, block: json!({"type": "reasoning", "text": "", "by": "freebuff"}) });
                }
                self.count.thought(t);
                out.push(Event::Reasoning { index: self.think.unwrap() as usize, text: t.into() });
            }
            if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                stop(&mut self.think, &mut out);
                if self.text.is_none() {
                    let i = self.index();
                    self.text = Some(i);
                    out.push(Event::Block { index: i as usize, block: oak::text("") });
                }
                self.count.wrote(t);
                out.push(Event::Text { index: self.text.unwrap() as usize, text: t.into() });
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let n = tc["index"].as_u64().unwrap_or(0) as usize;
                let name = tc["function"]["name"].as_str().unwrap_or("");
                let args = tc["function"]["arguments"].as_str().unwrap_or("");
                let id = tc["id"].as_str().unwrap_or("");
                stop(&mut self.text, &mut out);
                stop(&mut self.think, &mut out);
                let pos = match self.tools.iter().position(|t| t.n == n) {
                    Some(p) => p,
                    None => {
                        let block = self.index();
                        let id = if id.is_empty() { format!("call_{n}") } else { id.to_string() };
                        let early = crate::freebuff_tools::early_name(name);
                        if let Some(oak) = early {
                            out.push(tool_start(block, &id, oak, name));
                        }
                        self.tools.push(Tool { n, block, id, name: name.to_string(), args: String::new(), opened: early.is_some() });
                        self.tools.len() - 1
                    }
                };
                self.count.wrote(name);
                self.count.wrote(args);
                self.tools[pos].args.push_str(args);
            }
            if choice["finish_reason"].is_string() {
                self.finished = true;
                let calls = !self.tools.is_empty() || choice["finish_reason"] == "tool_calls";
                self.tool_calls = calls;
                close_all(self, &mut out);
                let stop = match choice["finish_reason"].as_str().unwrap_or("stop") {
                    "length" => oak::stop::MAX_TOKENS,
                    "content_filter" => oak::stop::REFUSAL,
                    _ if calls => oak::stop::TOOL_CALLS,
                    _ => oak::stop::END,
                };
                let u = &ev["usage"];
                let input = u["prompt_tokens"].as_u64().unwrap_or(0);
                let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(input);
                let output = u["completion_tokens"].as_u64().unwrap_or(0);
                let thought = self.count.tokens(crate::nodeclub::reported_thinking(u), output);
                out.push(Event::Usage(oak::Usage { input: input - cached, cache_read: cached, output, reasoning: (thought > 0).then_some(thought), ..Default::default() }));
                out.push(Event::Stop { reason: stop.into(), detail: None });
            }
        }
        out
    }
}

fn tool_start(index: i64, id: &str, oak: &str, shown: &str) -> Event {
    Event::Block { index: index as usize, block: json!({"type": "tool_call", "id": id, "name": oak, "_oak_shown": shown, "input": {}}) }
}

/// Ends the block `open` names.
fn stop(open: &mut Option<i64>, out: &mut Vec<Event>) {
    if let Some(i) = open.take() {
        out.push(Event::BlockEnd { index: i as usize });
    }
}

/// Ends the open blocks. A tool call's arguments are complete only now, so
/// this is where it becomes the OAK calls that run it, the first in the
/// block it opened, the others in new ones.
fn close_all(b: &mut Blocks, out: &mut Vec<Event>) {
    stop(&mut b.text, out);
    stop(&mut b.think, out);
    for t in std::mem::take(&mut b.tools) {
        for (k, (id, oak, input)) in crate::freebuff_tools::to_oak(&t.id, &t.name, &t.args).into_iter().enumerate() {
            let index = if k == 0 { t.block } else { b.index() };
            if k > 0 || !t.opened {
                out.push(tool_start(index, &id, &oak, &t.name));
            }
            out.push(Event::Input { index: index as usize, json: input.to_string() });
            out.push(Event::BlockEnd { index: index as usize });
        }
    }
}

/// Streams one Oak request through Freebuff, sending its events to `tx`.
pub fn stream(body: Request, tx: Sender<Sse>) {
    // The conversation's account (Freebuff is free: they weigh the same,
    // and conversations spread over them).
    let conv = body.seat_conv();
    let now = crate::api::now_ms();
    let cands: Vec<crate::rotation::Candidate> = STATE
        .lock()
        .unwrap()
        .list
        .iter()
        .map(|a| crate::rotation::Candidate { id: a.cred.id.clone(), left: 100.0, usable: true, resets_at: i64::MAX, preferred: true })
        .collect();
    let id = crate::rotation::pick(crate::api::Provider::Freebuff, &conv, &cands, now, !crate::api::rotation_on());
    if let Some(id) = &id {
        crate::rotation::used(crate::api::Provider::Freebuff, &conv, id, crate::oak::Ttl::FiveMinutes.ms(), now);
    }
    USING.with(|u| *u.borrow_mut() = id);
    stream_once(body, tx, 0);
}

/// Up to three attempts: a Freebuff hour is restarted when the server says it
/// has ended, with a pause so the fresh claim is in place before the resend.
fn stream_once(body: Request, tx: Sender<Sse>, attempt: u8) {
    let id = body.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err("Freebuff: unknown model.".into())));
    };
    // A session hour covers this request (starting one when none runs).
    if hour() == Hour::None
        && let Err(e) = admit(m.id)
    {
        return drop(tx.send(Sse::Err(e)));
    }
    let chat = chat_key(&body);
    // The smart-context summary is a step of the current turn.
    let summary = body.purpose == oak::Purpose::Summary;
    let mut turn = match begin_turn(&body, m) {
        Ok(t) => t,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    // A request that fails, or is dropped (a stopped or deleted agent), ends
    // its turn's run: an unfinished run holds one of the account's tabs.
    let mut guard = TurnGuard { chat: chat.clone(), keep: false };
    let step_started = iso_now();
    let Some(resp) = open_turn(&body, m, &chat, &mut turn, &tx, attempt) else { return };
    let mut b = Blocks::default();
    for line in std::io::BufReader::new(resp).lines() {
        let Ok(line) = line else { return drop(tx.send(Sse::Err("Stream broke.".into()))) };
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
        if !b.started {
            b.started = true;
            let _ = tx.send(Sse::Event(Event::Start { id: ev["id"].as_str().unwrap_or("").into(), model: id.clone() }));
        }
        if let Some(e) = ev.get("error") {
            let msg = e["message"].as_str().unwrap_or("unknown error").to_string();
            // Only when nothing has been shown yet: the resend starts over.
            let empty = b.text.is_none() && b.think.is_none() && b.tools.is_empty();
            if empty && attempt < 2 && msg.contains("freebuff_catalog_stale") {
                fetch_catalog();
                guard.keep = true;
                return stream_once(body.clone(), tx, attempt + 1);
            }
            if msg.contains("free session has ended") && attempt < 2 && empty && restart_session(m.id).is_ok() {
                guard.keep = true;
                return stream_once(body.clone(), tx, attempt + 1);
            }
            return drop(tx.send(Sse::Err(format!("Freebuff: {msg}"))));
        }
        let out = b.feed(&ev);
        for v in out {
            if tx.send(Sse::Event(v)).is_err() {
                return;
            }
        }
    }
    if !b.started {
        return drop(tx.send(Sse::Err("Stream ended before the reply began".into())));
    }
    if !b.finished {
        let mut out = vec![];
        close_all(&mut b, &mut out);
        for v in out {
            let _ = tx.send(Sse::Event(v));
        }
    }
    if b.finished {
        // One step per model reply, with the response's own id, as the CLI
        // records it.
        let step = json!({
            "id": crate::api::new_uuid(),
            "stepNumber": turn.step,
            "credits": 0,
            "childRunIds": [],
            "messageId": b.message_id.clone(),
            "status": "completed",
            "startTime": step_started,
        });
        add_step(&chat, &turn.run, step);
    }
    let _ = tx.send(Sse::Done);
    guard.keep = true;
    // A reply that asks for tools keeps the turn (and its run) going; the
    // final answer closes it. A rolling-summary request is a step of the same
    // turn, so it never closes it.
    if b.finished && !b.tool_calls && !summary {
        end_turn(&chat, &turn.run);
    }
}

/// Opens the reply stream for `turn`, trying twice more when Freebuff
/// says the hour ended, the run was lost or the catalog's handle is stale
/// (the turn then runs again under a new run or handle). None when it
/// failed, said on `tx`, or the request was dropped.
fn open_turn(body: &Request, m: &Model, chat: &str, turn: &mut Turn, tx: &Sender<Sse>, attempt: u8) -> Option<reqwest::blocking::Response> {
    let mut req = request_body(body, m, turn);
    let mut tries = 0;
    loop {
        match open(&req, tx) {
            Ok(r) => return Some(r),
            Err(Some(Sse::Err(msg)))
                if tries < 2
                    && attempt < 2
                    && (msg.contains("free session has ended")
                        || msg.contains("runId Not Found")
                        || msg.contains("did not complete a usable response")
                        || msg.contains("freebuff_catalog_stale")) =>
            {
                tries += 1;
                // A rotated handle: refetch the catalog, then run the
                // same turn again under the row's new handle.
                if msg.contains("freebuff_catalog_stale") {
                    fetch_catalog();
                    req = request_body(body, m, turn);
                    continue;
                }
                // The hour ran out (or was taken over): start a new one
                // and send the same message again, as the client does.
                // A lost run only needs a fresh registration.
                if msg.contains("free session has ended")
                    && let Err(e) = restart_session(m.id)
                {
                    let _ = tx.send(Sse::Err(e));
                    return None;
                }
                // The old run belonged to the session that just ended.
                let Some(t) = new_run_for_turn(chat, m) else {
                    let _ = tx.send(Sse::Err("Freebuff didn't register this run. Try again.".into()));
                    return None;
                };
                *turn = t;
                req = request_body(body, m, turn);
            }
            Err(Some(s)) => {
                let _ = tx.send(s);
                return None;
            }
            Err(None) => return None,
        }
    }
}

/// Ends the conversation's current turn (FINISHing its run) when dropped,
/// unless `keep`: see `stream_once`.
struct TurnGuard {
    chat: String,
    keep: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let turn = TURNS.lock().map(|mut t| t.remove(&self.chat)).ok().flatten();
        if let Some(t) = turn {
            finish_run(&t.run, t.steps);
        }
    }
}

// ---------------------------------------------------------------------------
// Hour and grace alerts
// ---------------------------------------------------------------------------

/// A line for the transcript: the hour ran out (grace began), or the grace
/// ran out (the model must be asked to carry on).
#[derive(Clone, PartialEq)]
pub enum Alert {
    Grace { left_ms: i64 },
    GraceOver,
}

/// What the monitor has seen so far, so each change is reported once.
/// 0 = no hour seen, 1 = running, 2 = in grace.
static ALERT_STATE: LazyLock<Mutex<(u8, Vec<Alert>)>> = LazyLock::new(|| Mutex::new((0, Vec::new())));

/// Checks the hour and queues the lines it crossed. Called while a Freebuff
/// turn is running; the agent loop drains it with `take_alerts`.
pub fn poll_alerts() {
    let mut s = ALERT_STATE.lock().unwrap();
    match hour() {
        Hour::Active { .. } => s.0 = 1,
        Hour::Grace { left_ms } => {
            if s.0 != 2 && !s.1.iter().any(|a| matches!(a, Alert::Grace { .. })) {
                s.1.push(Alert::Grace { left_ms });
            }
            s.0 = 2;
        }
        Hour::None => {
            // Only a grace that ran out ends a session; an hour that was never
            // seen ending (a fresh session, or one released) says nothing.
            if s.0 == 2 && !s.1.iter().any(|a| matches!(a, Alert::GraceOver)) {
                s.1.push(Alert::GraceOver);
            }
            s.0 = 0;
        }
    }
}

/// Forgets the hour state: a new session starts, or one was released.
pub fn reset_alerts() {
    let mut s = ALERT_STATE.lock().unwrap();
    s.0 = 0;
    s.1.clear();
}

pub fn take_alerts() -> Vec<Alert> {
    std::mem::take(&mut ALERT_STATE.lock().unwrap().1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn() -> Turn {
        Turn {
            key: "k".into(),
            run: "run-1".into(),
            step: 3,
            trace: "5f0c5d1e-1111-4222-8333-444455556666".into(),
            client_id: "abc123def4567".into(),
            profile: Some("v1;tc=5;ke=5;mc=0;pc=0;pe=0;ms=120;cps=4".into()),
            snapshot: Some("{\"gitAvailable\":true}".into()),
            steps: Vec::new(),
        }
    }

    fn body() -> Request {
        Request {
            messages: vec![
                json!({"role": "user", "content": [
                    {"type": "text", "text": "<environment>context</environment>"},
                    {"type": "text", "text": "fix it"},
                ]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}),
            ],
            effort: Some("high".into()),
            ..Default::default()
        }
    }

    #[test]
    fn metadata_is_the_cli_set() {
        *INSTANCE.lock().unwrap() = "cli:test".into();
        let req = build_request(&body(), "fbm1.handle", "DeepSeek V4.1 Flash 10/17", Some("high".into()), &turn());
        let meta = req["codebuff_metadata"].as_object().unwrap();
        let mut keys: Vec<&str> = meta.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "client_id",
                "cost_mode",
                "freebuff_client_env",
                "freebuff_input_profile",
                "freebuff_instance_id",
                "freebuff_multi_session",
                "freebuff_reasoning_effort",
                "llm_step_number",
                "repo_snapshot",
                "run_id",
                "surface",
                "trace_session_id",
            ]
        );
        assert_eq!(meta["run_id"], "run-1");
        assert_eq!(meta["client_id"], "abc123def4567");
        assert_eq!(meta["llm_step_number"], "3");
        assert_eq!(meta["freebuff_reasoning_effort"], "high");
        assert_eq!(meta["cost_mode"], "free");
        assert_eq!(req["model"], "fbm1.handle");
    }

    #[test]
    fn a_plain_model_omits_the_optional_metadata() {
        let mut t = turn();
        t.profile = None;
        t.snapshot = None;
        let mut b = body();
        b.effort = None;
        let req = build_request(&b, "deepseek/deepseek-v4-flash", "deepseek/deepseek-v4-flash", None, &t);
        let meta = req["codebuff_metadata"].as_object().unwrap();
        assert!(!meta.contains_key("freebuff_input_profile"));
        assert!(!meta.contains_key("repo_snapshot"));
        assert!(!meta.contains_key("freebuff_reasoning_effort"));
        assert!(!meta.contains_key("agent_id"));
        assert!(!meta.contains_key("model"));
        assert!(!meta.keys().any(|k| k.starts_with("compaction_")));
    }

    #[test]
    fn the_prompt_has_todays_date_and_the_row_name() {
        let p = fill_prompt(include_str!("../assets/freebuff-system.txt"), "DeepSeek V4.1 Flash 10/17");
        assert!(p.contains(&format!("Current date: {}.", current_date())));
        assert!(p.contains("You are running on the DeepSeek V4.1 Flash 10/17 model."));
        assert!(!p.contains("November 3, 2026"));
    }

    #[test]
    fn bun_writes_the_headers_in_its_order() {
        let head = Wire::post("https://www.codebuff.com/api/v1/chat/completions")
            .header("x-freebuff-client-version", "0.2.35")
            .header("content-type", "application/json")
            .bearer("tok")
            .json(&json!({"a": 1}))
            .headers();
        let names: Vec<&str> = head.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "Authorization",
                "content-type",
                "x-freebuff-client-version",
                "Connection",
                "User-Agent",
                "Accept",
                "Host",
                "Accept-Encoding",
                "Content-Length",
            ]
        );
        assert_eq!(head[1].1, "application/json");
        assert_eq!(head[4].1, "Bun/1.3.14");
        assert_eq!(head[6].1, "www.codebuff.com");
        assert_eq!(head[7].1, "gzip, deflate, br, zstd");
        assert_eq!(head[8].1, "7");
    }

    #[test]
    fn a_set_user_agent_is_not_replaced() {
        let head = Wire::get("https://www.codebuff.com/x").header("user-agent", AI_SDK_UA).headers();
        assert!(head.iter().any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v == AI_SDK_UA));
        assert!(!head.iter().any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v == BUN_UA));
    }

    #[test]
    fn trace_ids_are_stable_per_conversation_and_uuid_shaped() {
        let a = seeded_uuid("freebuff-trace\nchat-1");
        assert_eq!(a, seeded_uuid("freebuff-trace\nchat-1"));
        assert_ne!(a, seeded_uuid("freebuff-trace\nchat-2"));
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
    }

    #[test]
    fn input_profiles_encode_like_the_cli() {
        let mut c = InputCounter::default();
        assert_eq!(encode_profile(&c), "v1;tc=0;ke=0;mc=0;pc=0;pe=0;cps=0");
        c.typed = 42;
        c.keys = 40;
        c.multi = 2;
        c.pasted = 10;
        c.pastes = 1;
        c.first = Some(crate::api::now_ms() - 2500);
        c.max_window = 7;
        let s = encode_profile(&c);
        assert!(s.starts_with("v1;tc=42;ke=40;mc=2;pc=10;pe=1;ms="));
        assert!(s.ends_with(";cps=7"));
    }

    #[test]
    fn the_snapshot_comes_from_git() {
        let root = env!("CARGO_MANIFEST_DIR");
        let s = repo_snapshot(Some(root)).expect("oak is a git repo");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["gitAvailable"], true);
        assert!(v["fileCount"].as_u64().unwrap() > 10);
        assert!(v["contributorCount"].as_u64().unwrap() >= 1);
        assert!(v.get("commitDatePercentiles").is_some());
    }
}
