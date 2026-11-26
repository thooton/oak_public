//! Holds a login's outpost ONLINE while its turns still run.
//!
//! Quick takes an outpost's ONLINE status from the newest chat socket that
//! names it: closing an older one changes nothing, but when the newest closes
//! the outpost goes OFFLINE, though older sockets naming it are still connected
//! (`outpost_test::quick_outpost_shared_sockets` measured it: on one account,
//! not across two). Every Oak turn opens its own socket naming the login's
//! outpost, so conversations sharing a login (rotation puts several on one) lost
//! the device whenever another one's turn ended, until a new turn connected:
//! a long turn then found this machine OFFLINE for minutes.
//!
//! So each turn's socket holds its login's outpost (`hold`, the socket keeps
//! the `Hold`), and when one closes while others still hold it, a keeper
//! connects: a socket naming the outpost, with no thread of its own, which is
//! then the newest. A keeper is replaced the same way at the next close (the
//! old one closes once the new one is connected, so there is no gap), reconnects
//! if Quick drops it, and closes when the login's last turn socket does. It
//! reads what Quick sends it and answers nothing: the turns run their own
//! threads' tool calls (`turn::owner`).

use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// What a keeper's socket names as its session, so it holds nothing itself.
const KEEP: &str = "oak-keep-";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    login: String,
    outpost: String,
}

#[derive(Default, Debug)]
struct Entry {
    /// Turn sockets open on it.
    open: usize,
    /// The keeper that should be holding it (0: none).
    keeper: u64,
    /// The newest keeper that has connected.
    connected: u64,
}

static HELD: LazyLock<Mutex<HashMap<Key, Entry>>> = LazyLock::new(Default::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A turn's socket holding its login's outpost; let go when the socket goes.
pub(crate) struct Hold {
    key: Key,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(generation) = released(&self.key) {
            let key = self.key.clone();
            std::thread::spawn(move || keep(key, generation));
        }
    }
}

/// The value of `name` in `path`'s query.
fn query<'a>(path: &'a str, name: &str) -> Option<&'a str> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v))
}

/// The hold of a socket opened at `path` by the login this thread uses: one
/// when the socket names an outpost and is a turn's (not a keeper's).
pub(crate) fn hold(path: &str) -> Option<Hold> {
    hold_as(path, login::current()?)
}

fn hold_as(path: &str, login: String) -> Option<Hold> {
    let outpost = query(path, "outpostId").filter(|o| !o.is_empty())?;
    if query(path, "workingSessionId").is_some_and(|s| s.starts_with(KEEP)) {
        return None;
    }
    let key = Key { login, outpost: outpost.to_string() };
    HELD.lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().open += 1;
    Some(Hold { key })
}

/// A turn's socket on `key` closed: the keeper to start (its generation) when
/// others still hold the outpost; none when it was the last (any keeper then
/// closes).
fn released(key: &Key) -> Option<u64> {
    let mut all = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let e = all.get_mut(key)?;
    e.open = e.open.saturating_sub(1);
    if e.open == 0 {
        all.remove(key);
        return None;
    }
    let generation = NEXT.fetch_add(1, Ordering::SeqCst);
    e.keeper = generation;
    Some(generation)
}

/// Whether keeper `generation` stays: its outpost is still held, and it is the
/// one that should hold it, or the one that should hasn't connected yet.
fn stays(key: &Key, generation: u64) -> bool {
    HELD.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .is_some_and(|e| e.open > 0 && (e.keeper == generation || e.connected < e.keeper))
}

/// Whether keeper `generation` is the one that should hold `key` (so it
/// reconnects when Quick drops it); notes it connected when `connected`.
fn current(key: &Key, generation: u64, connected: bool) -> bool {
    let mut all = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let Some(e) = all.get_mut(key).filter(|e| e.open > 0 && e.keeper == generation) else {
        return false;
    };
    if connected {
        e.connected = e.connected.max(generation);
    }
    true
}

/// Keeper `generation` of `key`: connects, holds the outpost while it should,
/// reconnecting when its socket drops, and closes when replaced or done.
fn keep(key: Key, generation: u64) {
    // (Unit tests count holds; they never reach Quick, unless a live probe
    // asks: `outpost_test::quick_outpost_keeper_holds`.)
    if cfg!(test) && std::env::var_os("OAK_QUICK_KEEPER").is_none() {
        return;
    }
    login::set_using(Some(key.login.clone()));
    while current(&key, generation, false) {
        let mut sock = match open(&key.outpost) {
            Ok(s) => s,
            Err(e) => {
                if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                    eprintln!("outpost keeper for {} couldn't connect: {e}", key.outpost);
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        current(&key, generation, true);
        while stays(&key, generation) {
            if !sock.keep_alive(Duration::from_secs(1)) {
                break;
            }
            // (What Quick sends it is the turns' to act on.)
            sock.drain_pending();
        }
        if !stays(&key, generation) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A keeper's socket: it names `outpost` and joins its session and home, as a
/// turn's does, but no thread.
fn open(outpost: &str) -> Result<socket::Socket, String> {
    let token = login::access_token()?;
    let host = login::host()?;
    let path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={SURFACE}&token={}&outpostId={outpost}&workingSessionId={KEEP}{}&channel=prod",
        turn::urlencode(&token),
        api::now_ms(),
    );
    let mut sock = socket::Socket::connect(&host, &path)?;
    let base = json!({"surfaceType": SURFACE, "appVersion": APP_VERSION, "outpostId": outpost});
    let mut connect = base.clone();
    connect["actor_id"] = json!("");
    sock.send(&json!({"type": "session_connect", "payload": connect}))?;
    sock.send(&json!({"type": "federate_status", "payload": {}}))?;
    let mut home = base;
    home["session_id"] = json!("home");
    home["focus_only"] = json!(true);
    sock.send(&json!({"type": "session_connect", "payload": home}))?;
    sock.send(&json!({"type": "workstreams_summary", "payload": {"workstream_ids": []}}))?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_path(outpost: &str, session: &str) -> String {
        format!("/streaming/x?surfaceType=s&token=t&outpostId={outpost}&workingSessionId={session}&channel=prod")
    }

    fn entry(login: &str, outpost: &str) -> Option<(usize, u64)> {
        let key = Key { login: login.into(), outpost: outpost.into() };
        HELD.lock().unwrap().get(&key).map(|e| (e.open, e.keeper))
    }

    /// A turn socket closing while another on the same login's outpost is
    /// open starts a keeper; the last one closing leaves none. Keepers' own
    /// sockets and sockets without an outpost hold nothing.
    #[test]
    fn a_turn_closing_under_another_starts_a_keeper() {
        let (login, outpost) = ("keeper-login", "keeper-outpost");
        let a = hold_as(&turn_path(outpost, "t1"), login.into()).unwrap();
        let b = hold_as(&turn_path(outpost, "t2"), login.into()).unwrap();
        assert!(hold_as(&turn_path(outpost, "oak-keep-1"), login.into()).is_none());
        assert!(hold_as("/streaming/x?token=t&workingSessionId=t3", login.into()).is_none());
        // Another login's socket on the same outpost id is its own.
        let other = hold_as(&turn_path(outpost, "t4"), "keeper-other".into()).unwrap();
        assert_eq!(entry(login, outpost), Some((2, 0)));
        drop(a);
        let (open, keeper) = entry(login, outpost).unwrap();
        assert!(open == 1 && keeper > 0, "a keeper holds it for the turn still open");
        drop(b);
        assert_eq!(entry(login, outpost), None, "the last close leaves no keeper");
        assert_eq!(entry("keeper-other", outpost), Some((1, 0)));
        drop(other);
    }

    /// An old keeper stays until the one replacing it has connected, so the
    /// outpost isn't left without the newest socket in between.
    #[test]
    fn a_keeper_stays_until_its_successor_connects() {
        let key = Key { login: "keeper-l2".into(), outpost: "keeper-o2".into() };
        HELD.lock().unwrap().insert(key.clone(), Entry { open: 1, keeper: 7, connected: 0 });
        assert!(current(&key, 7, true));
        HELD.lock().unwrap().get_mut(&key).unwrap().keeper = 9;
        assert!(stays(&key, 7), "9 hasn't connected yet");
        assert!(!current(&key, 7, false), "7 doesn't reconnect any more");
        assert!(current(&key, 9, true));
        assert!(!stays(&key, 7), "9 connected: 7 closes");
        assert!(stays(&key, 9));
        HELD.lock().unwrap().remove(&key);
    }
}
