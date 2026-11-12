//! What the model providers share: a picture's data URL, the lines a
//! stream is read by, the retry events every provider sends, and a key kept on this machine or on the vault.

use crate::tools::Head;
use crate::api::{self, Sse};
use crate::oak::Event;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// An Oak image or document block's data as a data URL.
pub fn data_url(b: &Value) -> Option<String> {
    Some(format!("data:{};base64,{}", b["media_type"].as_str()?, b["data"].as_str()?))
}

/// Sends what `build` builds, once more when a kept-alive connection turned
/// out to be stale (`api::stale_connection`).
pub fn send(build: impl Fn() -> reqwest::blocking::RequestBuilder) -> reqwest::Result<reqwest::blocking::Response> {
    match build().send() {
        Err(e) if api::stale_connection(&e) => build().send(),
        r => r,
    }
}

/// Whether a failed request is worth sending again within `api::RETRIES`:
/// network failures (no status), rate limits, and server errors.
pub fn retryable(status: Option<u16>) -> bool {
    status.is_none_or(|s| matches!(s, 408 | 429 | 500..=599))
}

/// Seconds to wait before retry number `tries`: what the server asked
/// (`after`, its `retry-after`) when it said, else `api::retry_wait`, at
/// most a minute.
pub fn retry_in(after: Option<u64>, tries: u32) -> u64 {
    after.unwrap_or(api::retry_wait(tries)).min(60)
}

/// Says a retry is coming (`reason` when there is one to show). False when
/// the receiver is gone.
fn say_retry(tx: &Sender<Sse>, tries: u32, wait: u64, reason: Option<String>) -> bool {
    tx.send(Sse::Event(Event::Retry { attempt: tries, wait, reason: reason.unwrap_or_default() })).is_ok()
}

/// The response's headers are in, for the benchmark timeline.
pub fn say_headers(tx: &Sender<Sse>, status: u16) -> bool {
    !crate::bench::on() || tx.send(Sse::Event(Event::Headers(status))).is_ok()
}

/// Asks to wait `wait` seconds before retry number `tries` (`reason` shown
/// to the user), and sleeps for the wait. False when the receiver is gone.
pub fn wait_between(tx: &Sender<Sse>, tries: u32, wait: u64, reason: Option<String>) -> bool {
    if !say_retry(tx, tries, wait, reason) {
        return false;
    }
    std::thread::sleep(Duration::from_secs(wait));
    true
}

/// Sends what `build` builds (`api::send_fresh`) until the answer is not
/// worth sending again: network failures, rate limits and server errors
/// (`retryable`) go again within `api::RETRIES` (counted in `tries`), after
/// the wait the server asks for (at most a minute), the receiver told why
/// (`why`: what a reply says). Err when the receiver is gone.
pub fn send_retrying(
    tx: &Sender<Sse>,
    tries: &mut u32,
    build: impl Fn() -> reqwest::blocking::RequestBuilder,
    why: impl Fn(reqwest::blocking::Response) -> String,
) -> Result<Result<reqwest::blocking::Response, String>, ()> {
    loop {
        let result = api::send_fresh(&build);
        *tries += 1;
        if !(retryable(result.as_ref().ok().map(|r| r.status().as_u16())) && *tries <= api::RETRIES) {
            return Ok(result);
        }
        let (wait, reason) = match result {
            Ok(r) => (retry_in(api::retry_after(r.headers()), *tries), why(r)),
            Err(e) => (api::retry_wait(*tries), e),
        };
        if !wait_between(tx, *tries, wait, Some(reason)) {
            return Err(());
        }
    }
}

/// What an error reply says: the message its JSON body carries (wherever
/// the API puts it, `extra` when only it does), else the body's first 500
/// characters.
pub fn error_msg(v: &Value, body: &str, extra: Option<&str>) -> String {
    v["error"]["message"]
        .as_str()
        .or(v["error"].as_str())
        .or(v["detail"].as_str())
        .or(v["error_description"].as_str())
        .or(extra)
        .map(String::from)
        .unwrap_or_else(|| body.head(500))
}

/// What an image becomes on a model that takes none.
pub const NO_IMAGE: &str = "[An image was here; this model can't see images.]";

/// A stream's `data:` events: Err when a line couldn't be read, None for
/// `[DONE]`; payloads that aren't JSON are skipped.
pub struct Events(std::io::Lines<std::io::BufReader<reqwest::blocking::Response>>);

pub fn events(resp: reqwest::blocking::Response) -> Events {
    use std::io::BufRead;
    Events(std::io::BufReader::new(resp).lines())
}

impl Iterator for Events {
    type Item = Result<Option<Value>, String>;

    fn next(&mut self) -> Option<Self::Item> {
        for line in self.0.by_ref() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Some(Err(format!("Stream broke: {e}"))),
            };
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                return Some(Ok(None));
            }
            if let Ok(v) = serde_json::from_str(data) {
                return Some(Ok(Some(v)));
            }
        }
        None
    }
}

// ---------------------------------------------------------------- the keys

/// A provider's key files, environment variable and vault kind.
pub struct KeyCfg {
    /// The provider as the user reads it ("NodeClub").
    pub name: &'static str,
    pub kind: crate::providers::Kind,
    /// The keys' file in OAK's folder; the variable holds one more, when
    /// set (used, never saved).
    pub file: &'static str,
    pub env: &'static str,
    /// What a vault keeps them as (`oak_sdk::Vault`'s kind): "openrouter".
    pub vault: &'static str,
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    #[serde(default)]
    keys: Vec<StoredKey>,
    /// From before there could be more than one.
    #[serde(default, skip_serializing)]
    key: Option<String>,
    #[serde(default, skip_serializing)]
    remote: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredKey {
    key: String,
    /// A copy of the vault's key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    remote: bool,
    /// What OAK spent through it, all told (dollars).
    #[serde(default, skip_serializing_if = "is_zero")]
    spent: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// A key's id: the start of its hash, the same on every OAK and the
/// vault.
pub fn key_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    api::hex(&Sha256::digest(key.as_bytes())[..6])
}

/// "sk-…3f9a": enough of a key to tell it apart.
pub fn short_key(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let head: String = key.chars().take_while(|c| *c != '-').take(6).collect();
    if key.chars().count() <= 8 { "…".into() } else if key.contains('-') { format!("{head}-…{tail}") } else { format!("…{tail}") }
}

/// One key, and what the provider keeps beside it (`E`).
#[derive(Clone)]
pub struct Key<E> {
    pub id: String,
    pub key: String,
    /// The vault's.
    pub remote: bool,
    /// From the environment variable (not saved).
    pub env: bool,
    /// What OAK spent through it, all told (dollars; saved).
    pub spent: f64,
    /// It turned a request down for its limit or credits: none goes
    /// through it before then (ms since 1970).
    pub limited_until: i64,
    /// The provider turned the key itself down (401/403: revoked, deleted,
    /// mistyped), with what it said: no request goes through it until it
    /// works again (its credits fetch) or is added again.
    pub rejected: Option<String>,
    pub extras: E,
}

impl<E> Key<E> {
    /// Requests can go through it now.
    pub fn usable(&self, now: i64) -> bool {
        self.limited_until <= now && self.rejected.is_none()
    }
}

struct KeyState<E> {
    list: Vec<Key<E>>,
    /// Why a key is on this machine although a vault is
    /// connected, when that was not asked for.
    note: Option<String>,
}

/// A provider's keys: on this machine, and on the vault when one
/// is connected, for every OAK. Requests pick one per conversation
/// (`crate::rotation`).
pub struct Keys<E> {
    cfg: KeyCfg,
    state: Mutex<KeyState<E>>,
}

impl<E: Default + Send + Clone> Keys<E> {
    pub fn new(cfg: KeyCfg) -> Keys<E> {
        Keys { cfg, state: Mutex::new(KeyState { list: vec![], note: None }) }
    }

    fn path(&self) -> std::path::PathBuf {
        crate::store::dir().join(self.cfg.file)
    }

    fn entry(key: &str, remote: bool, spent: f64) -> Key<E> {
        Key { id: key_id(key), key: key.into(), remote, env: false, spent, limited_until: 0, rejected: None, extras: E::default() }
    }

    /// Loads the keys: their file in OAK's folder, and the environment
    /// variable's. True when there is one.
    pub fn load(&self) -> bool {
        let file = crate::store::read_json::<KeyFile>(&self.path());
        let mut list: Vec<Key<E>> = vec![];
        if let Some(f) = file {
            let old = f.key.map(|key| StoredKey { key, remote: f.remote, spent: 0.0 });
            for k in f.keys.into_iter().chain(old) {
                let key = k.key.trim();
                if !key.is_empty() && !list.iter().any(|e| e.key == key) {
                    list.push(Self::entry(key, k.remote, k.spent));
                }
            }
        }
        if let Some(key) = std::env::var(self.cfg.env).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
            && !list.iter().any(|e| e.key == key)
        {
            list.push(Key { env: true, ..Self::entry(&key, false, 0.0) });
        }
        let mut s = self.state.lock().unwrap();
        // What was learned of a key stays.
        for k in &mut list {
            if let Some(old) = s.list.iter().find(|o| o.id == k.id) {
                (k.extras, k.limited_until, k.rejected) = (old.extras.clone(), old.limited_until, old.rejected.clone());
            }
        }
        s.list = list;
        !s.list.is_empty()
    }

    fn save(&self, s: &KeyState<E>) -> Result<(), String> {
        let keys: Vec<StoredKey> = s.list.iter().filter(|k| !k.env).map(|k| StoredKey { key: k.key.clone(), remote: k.remote, spent: k.spent }).collect();
        if keys.is_empty() {
            let _ = std::fs::remove_file(self.path());
            return Ok(());
        }
        let file = KeyFile { keys, key: None, remote: false };
        crate::store::write_private_json(&self.path(), &file).map_err(|e| format!("Couldn't save the key: {e}"))
    }

    /// A key is set.
    pub fn on(&self) -> bool {
        !self.state.lock().unwrap().list.is_empty()
    }

    /// The keys, in order.
    pub fn list(&self) -> Vec<Key<E>> {
        self.state.lock().unwrap().list.clone()
    }

    /// Any key, for what isn't a conversation's (model lists, checks).
    pub fn any_key(&self) -> Result<String, String> {
        let s = self.state.lock().unwrap();
        let now = crate::api::now_ms();
        s.list.iter().find(|k| k.usable(now)).or(s.list.first()).map(|k| k.key.clone()).ok_or_else(|| self.none())
    }

    /// What a request is told when the provider rejected every key.
    fn rejection(&self, list: &[Key<E>]) -> String {
        let why = list[0].rejected.as_deref().unwrap_or("");
        match list {
            [k] => format!("{} rejected key {} ({why}): remove it on Home, or add one that works.", self.cfg.name, short_key(&k.key)),
            _ => format!("{} rejected every key ({why}): remove them on Home, or add one that works.", self.cfg.name),
        }
    }

    fn none(&self) -> String {
        format!("No {} key: add one on Home.", self.cfg.name)
    }

    /// The key conversation `conv`'s request goes through (its seat, see
    /// `crate::rotation`), each key weighed by what it has left (`left`).
    /// Err when there is none; with every key turned down for its limit,
    /// says until when.
    pub fn pick(&self, conv: &str, left: impl Fn(&Key<E>) -> f64) -> Result<Key<E>, String> {
        let now = crate::api::now_ms();
        let s = self.state.lock().unwrap();
        if s.list.is_empty() {
            return Err(self.none());
        }
        let remote = s.list.iter().any(|k| k.remote);
        let cands: Vec<crate::rotation::Candidate> = s
            .list
            .iter()
            .map(|k| crate::rotation::Candidate { id: k.id.clone(), left: left(k), usable: k.usable(now), resets_at: k.limited_until, preferred: k.remote || !remote })
            .collect();
        let id = crate::rotation::pick(self.cfg.kind, conv, &cands, now, !crate::api::rotation_on());
        match id.and_then(|id| s.list.iter().find(|k| k.id == id)) {
            Some(k) if k.usable(now) => {
                // Its cache there (where the provider keeps one) is fresh
                // from now.
                crate::rotation::used(self.cfg.kind, conv, &k.id, crate::oak::Ttl::FiveMinutes.ms(), now);
                Ok(k.clone())
            }
            _ if s.list.iter().all(|k| k.rejected.is_some()) => Err(self.rejection(&s.list)),
            _ => {
                let until = s.list.iter().filter(|k| k.rejected.is_none()).map(|k| k.limited_until).min().unwrap_or(now);
                Err(format!("Every {} key is at its limit until {}.", self.cfg.name, crate::time::clock(until)))
            }
        }
    }

    /// The provider turned key `id` itself down (`why`): no request goes
    /// through it until it works again. True when another key can take the
    /// request.
    pub fn reject(&self, id: &str, why: &str) -> bool {
        let now = crate::api::now_ms();
        let mut s = self.state.lock().unwrap();
        if let Some(k) = s.list.iter_mut().find(|k| k.id == id) {
            k.rejected = Some(why.to_string());
        }
        s.list.iter().any(|k| k.usable(now))
    }

    /// Key `id` worked (its credits or a check came back): it takes
    /// requests again if it was rejected.
    pub fn accepted(&self, id: &str) {
        if let Some(k) = self.state.lock().unwrap().list.iter_mut().find(|k| k.id == id) {
            k.rejected = None;
        }
    }

    /// Key `id` turned a request down for its limit: none goes through it
    /// until `until`. True when another key could take the request.
    pub fn limited(&self, id: &str, until: i64) -> bool {
        let now = crate::api::now_ms();
        let mut s = self.state.lock().unwrap();
        if let Some(k) = s.list.iter_mut().find(|k| k.id == id) {
            k.limited_until = k.limited_until.max(until);
        }
        s.list.iter().any(|k| k.usable(now))
    }

    /// Keeps `key` (`remote`: the vault's) beside the others.
    pub fn keep(&self, key: &str, remote: bool) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        match s.list.iter_mut().find(|k| k.key == key) {
            Some(k) => (k.remote, k.env, k.rejected) = (remote, false, None),
            None => s.list.push(Self::entry(key, remote, 0.0)),
        }
        self.save(&s)
    }

    /// The vault's keys.
    fn remote_keys(&self) -> Result<Vec<String>, String> {
        let list = crate::api::vault_list(self.cfg.vault)?;
        Ok(list.iter().filter_map(|v| v["key"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(String::from)).collect())
    }

    /// Puts `key` in the vault.
    fn upload(&self, key: &str) -> Result<(), String> {
        crate::api::vault_add(self.cfg.vault, &json!({"key": key})).map(|_| ())
    }

    /// Saves `key` (already checked) on the vault when one is
    /// connected, on this machine otherwise (`note` says why, when it was
    /// meant to go there).
    pub fn take_remote(&self, key: &str) -> Result<(), String> {
        let mut note = None;
        let remote = crate::api::signing_in_remotely()
            && match self.upload(key) {
                Ok(()) => true,
                Err(e) if crate::api::vault_cannot_keep(&e) => {
                    note = Some(format!("Kept on this machine: {e}"));
                    false
                }
                Err(e) => return Err(e),
            };
        self.keep(key, remote)?;
        self.state.lock().unwrap().note = note;
        Ok(())
    }

    /// Why a key stayed on this machine, when it was meant for the
    /// vault.
    pub fn note(&self) -> Option<String> {
        self.state.lock().unwrap().note.clone()
    }

    /// Brings the keys in step with the vault's: its keys are
    /// used (one on this machine too becomes its), and ones removed there
    /// go.
    pub fn sync(&self) {
        let Ok(keys) = self.remote_keys() else { return }; // unreachable
        let mut s = self.state.lock().unwrap();
        let before: Vec<(String, bool)> = s.list.iter().map(|k| (k.id.clone(), k.remote)).collect();
        s.list.retain(|k| !k.remote || keys.contains(&k.key));
        for key in &keys {
            match s.list.iter_mut().find(|k| &k.key == key) {
                Some(k) => (k.remote, k.env) = (true, false),
                None => s.list.push(Self::entry(key, true, 0.0)),
            }
        }
        if s.list.iter().map(|k| (k.id.clone(), k.remote)).collect::<Vec<_>>() != before {
            let _ = self.save(&s);
        }
    }

    /// Disconnected from the vault: its keys go from this
    /// machine. True when any did.
    pub fn forget_remote(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        let n = s.list.len();
        s.list.retain(|k| !k.remote);
        let gone = s.list.len() != n;
        if gone {
            let _ = self.save(&s);
        }
        gone
    }

    /// Key `id` is on this machine while a vault is connected: it
    /// can move there.
    pub fn movable(&self, id: &str) -> bool {
        crate::api::vault_on() && self.state.lock().unwrap().list.iter().any(|k| k.id == id && !k.remote)
    }

    /// Puts key `id` (on this machine) on the vault, for every OAK.
    pub fn move_to_vault(&self, id: &str) -> Result<(), String> {
        let key = self.state.lock().unwrap().list.iter().find(|k| k.id == id).map(|k| k.key.clone()).ok_or_else(|| self.none())?;
        self.upload(&key)?;
        self.keep(&key, true)?;
        self.state.lock().unwrap().note = None;
        Ok(())
    }

    /// Forgets key `id` (with the last, its provider's models leave the
    /// picker); the vault's is removed there, for every OAK (when
    /// it can't be reached, the key stays, and Err says why).
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let remote = self.state.lock().unwrap().list.iter().any(|k| k.id == id && k.remote);
        if remote {
            crate::api::vault_remove(self.cfg.vault, id)?;
        }
        let mut s = self.state.lock().unwrap();
        s.list.retain(|k| k.id != id);
        s.note = None;
        crate::rotation::forget_account(self.cfg.kind, id);
        self.save(&s)
    }

    /// Changes what the provider keeps beside key `id` (it may be gone).
    pub fn edit(&self, id: &str, f: impl FnOnce(&mut Key<E>)) {
        if let Some(k) = self.state.lock().unwrap().list.iter_mut().find(|k| k.id == id) {
            f(k);
        }
    }

    /// Adds what a request through key `id` cost to what it spent (saved).
    pub fn spend(&self, id: &str, usd: f64) {
        let mut s = self.state.lock().unwrap();
        if let Some(k) = s.list.iter_mut().find(|k| k.id == id) {
            k.spent += usd;
            let _ = self.save(&s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key the provider rejects (401) is skipped: its conversation goes
    /// on through another, and with none left the error names the key.
    #[test]
    fn a_rejected_key_is_skipped() {
        let keys: Keys<()> = Keys::new(KeyCfg { name: "Test", kind: crate::providers::Kind::NodeClub, file: "test-keys.json", env: "OAK_TEST_NO_KEY", vault: "test" });
        let (a, b) = (Keys::<()>::entry("sk-test-aaaaaaaa", true, 0.0), Keys::<()>::entry("sk-test-bbbbbbbb", true, 0.0));
        keys.state.lock().unwrap().list = vec![a.clone(), b.clone()];
        let first = keys.pick("rejected-conv", |_| 50.0).unwrap();
        assert!(keys.reject(&first.id, "401 Unauthorized: User not found"));
        let next = keys.pick("rejected-conv", |_| 50.0).unwrap();
        assert_ne!(next.id, first.id);
        assert!(!keys.reject(&next.id, "401 Unauthorized: User not found"));
        let e = keys.pick("rejected-conv", |_| 50.0).err().unwrap();
        assert!(e.contains("Test rejected every key (401 Unauthorized: User not found)"), "{e}");
        // Working again (its check came back), it takes requests.
        keys.accepted(&a.id);
        assert_eq!(keys.pick("rejected-conv", |_| 50.0).unwrap().id, a.id);
    }

    /// The id is the same the vault gives a key (the start of
    /// its SHA-256), so either side can name it.
    #[test]
    fn keys_are_named_by_their_hash() {
        assert_eq!(key_id("abc"), "ba7816bf8f01");
        assert_eq!(short_key("sk-or-v1-0123456789abcdef"), "sk-…cdef");
        assert_eq!(short_key("nodeclubkey1234"), "…1234");
        assert_eq!(short_key("short"), "…");
    }

    /// A key file from before there could be more than one reads as a
    /// list of one, and is written back as a list.
    #[test]
    fn old_key_files_read_as_a_list() {
        let old: KeyFile = serde_json::from_str(r#"{"key": "sk-1", "remote": true}"#).unwrap();
        assert!(old.keys.is_empty());
        assert_eq!((old.key.as_deref(), old.remote), (Some("sk-1"), true));
        let new = KeyFile { keys: vec![StoredKey { key: "sk-1".into(), remote: true, spent: 0.5 }], key: None, remote: false };
        assert_eq!(serde_json::to_value(&new).unwrap(), json!({"keys": [{"key": "sk-1", "remote": true, "spent": 0.5}]}));
    }
}
