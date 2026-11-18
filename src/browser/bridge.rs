//! The link to the user's own browser (Brave, Chrome, Edge) through Oak's
//! extension (`assets/brave-extension`): a WebSocket server on
//! 127.0.0.1:47615 that only the extension's origin may open (a web page's
//! origin can't be faked). The extension lists the user's tabs and, for a
//! tab the user allowed (BROWSER action=attach, `approval.rs`), attaches
//! the browser's debugger and passes DevTools commands and events: each
//! attached tab is a `Cdp` session over this link (`TabLink`), which the
//! conversation's browser (`browser.rs`) drives as it drives its own.
//!
//! Messages, JSON: Oak sends `{id, op: tabs|attach|detach|cdp|ping, tab,
//! method, params}` and gets `{id, result}` or `{id, error}`; the
//! extension sends `{op: hello}`, `{op: event, tab, method, params}` and
//! `{op: detached, tab, reason}`. The newest connection wins.

use crate::cdp::{self, Cdp, Link};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::channel;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::Duration;

/// Where Oak listens for the extension.
pub(crate) const PORT: u16 = 47615;
/// The extension's id (fixed by the key in its manifest).
pub(crate) const EXTENSION_ID: &str = "joanmlpfnnffmclkjfohnibiklmmedmd";
/// The extension's files, compiled in (Home > Protocols writes them out).
pub(crate) const FILES: [(&str, &str); 2] = [
    ("manifest.json", include_str!("../../assets/brave-extension/manifest.json")),
    ("background.js", include_str!("../../assets/brave-extension/background.js")),
];

type Answer = Box<dyn FnOnce(Result<Value, String>) + Send>;

#[derive(Default)]
struct Conn {
    /// Counts connections, so an old one's end doesn't end a newer one.
    serial: u64,
    writer: Option<TcpStream>,
    /// The browser it runs in ("Brave", "Edge", "Chrome").
    browser: String,
    next: u64,
    pending: HashMap<u64, Answer>,
    /// Attached tabs' sessions.
    sessions: HashMap<i64, Weak<Cdp>>,
}

static CONN: LazyLock<Mutex<Conn>> = LazyLock::new(Mutex::default);

/// Starts listening for the extension (once; quietly not when the port is
/// taken, as by another Oak).
pub(crate) fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", PORT)) else { return };
        let _ = std::thread::Builder::new().name("browser-bridge".into()).spawn(move || {
            for s in listener.incoming().flatten() {
                std::thread::spawn(move || serve(s));
            }
        });
    });
}

/// Whether the extension is connected, and the browser it runs in.
pub(crate) fn connected() -> Option<String> {
    let c = CONN.lock().unwrap();
    c.writer.is_some().then(|| if c.browser.is_empty() { "your browser".into() } else { c.browser.clone() })
}

fn serve(mut s: TcpStream) {
    // The handshake: the extension's origin only.
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = vec![];
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).ok() != Some(1) || head.len() > 16_384 {
            return;
        }
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    let origin_ok = header("Origin").is_some_and(|o| o == format!("chrome-extension://{EXTENSION_ID}"));
    let (Some(key), true) = (header("Sec-WebSocket-Key"), origin_ok && head.starts_with("GET /oak ")) else {
        let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let accept = base64::engine::general_purpose::STANDARD.encode(sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    let ok = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
    if s.write_all(ok.as_bytes()).is_err() {
        return;
    }
    let _ = s.set_read_timeout(None);
    let _ = s.set_nodelay(true);
    let Ok(writer) = s.try_clone() else { return };
    let (old, serial) = {
        let mut c = CONN.lock().unwrap();
        let old = std::mem::take(&mut *c);
        c.serial = rand::random::<u64>() | 1;
        c.writer = Some(writer);
        (old, c.serial)
    };
    drop_conn(old);
    // Kept awake: the extension's service worker sleeps when nothing comes.
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(20));
            if CONN.lock().unwrap().serial != serial {
                break;
            }
            request(json!({"op": "ping"}), |_| {});
        }
    });
    let mut r = std::io::BufReader::new(s);
    while let Ok((op, payload)) = cdp::ws_read(&mut r) {
        match op {
            0x8 => break,
            0x9 => {
                if let Some(w) = CONN.lock().unwrap().writer.as_mut() {
                    let _ = w.write_all(&cdp::frame(0xA, &payload, false));
                }
                continue;
            }
            0x1 => {}
            _ => continue,
        }
        let Ok(v) = serde_json::from_slice::<Value>(&payload) else { continue };
        incoming(v);
    }
    let mut c = CONN.lock().unwrap();
    if c.serial == serial {
        let old = std::mem::take(&mut *c);
        drop(c);
        drop_conn(old);
    }
}

/// Ends what a connection had: its calls fail, its tabs' sessions end.
fn drop_conn(old: Conn) {
    for (_, a) in old.pending {
        a(Err("The browser extension disconnected.".into()));
    }
    for s in old.sessions.values().filter_map(Weak::upgrade) {
        s.ended();
    }
    if let Some(w) = old.writer {
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
}

fn incoming(v: Value) {
    if let Some(id) = v["id"].as_u64() {
        let a = CONN.lock().unwrap().pending.remove(&id);
        if let Some(a) = a {
            a(match v.get("error") {
                Some(e) => Err(e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())),
                None => Ok(v["result"].clone()),
            });
        }
        return;
    }
    let tab = v["tab"].as_i64().unwrap_or(-1);
    let session = || CONN.lock().unwrap().sessions.get(&tab).and_then(Weak::upgrade);
    match v["op"].as_str() {
        Some("hello") => CONN.lock().unwrap().browser = v["browser"].as_str().unwrap_or("").chars().take(40).collect(),
        Some("event") => {
            if let Some(s) = session() {
                s.deliver(json!({"method": v["method"], "params": v["params"]}));
            }
        }
        Some("detached") => {
            let s = CONN.lock().unwrap().sessions.remove(&tab).and_then(|w| w.upgrade());
            if let Some(s) = s {
                s.ended();
            }
        }
        _ => {}
    }
}

/// Sends `msg` (given an id) to the extension; `answer` gets its reply.
fn request(mut msg: Value, answer: impl FnOnce(Result<Value, String>) + Send + 'static) {
    let mut c = CONN.lock().unwrap();
    if c.writer.is_none() {
        drop(c);
        return answer(Err(NOT_CONNECTED.into()));
    }
    c.next += 1;
    let id = c.next;
    msg["id"] = json!(id);
    let frame = cdp::frame(0x1, msg.to_string().as_bytes(), false);
    // (Under the lock: the reply can't come in before its answer is kept.)
    let sent = c.writer.as_mut().map(|w| w.write_all(&frame));
    if let Some(Err(e)) = sent {
        drop(c);
        return answer(Err(format!("Couldn't reach the browser extension: {e}")));
    }
    c.pending.insert(id, Box::new(answer));
}

/// `request`, waiting up to `timeout` for the reply.
fn call(msg: Value, timeout: Duration) -> Result<Value, String> {
    let (tx, rx) = channel();
    request(msg, move |r| {
        let _ = tx.send(r);
    });
    rx.recv_timeout(timeout).map_err(|_| "The browser extension didn't answer in time.".to_string())?
}

pub(crate) const NOT_CONNECTED: &str = "Oak's browser extension isn't connected: the user needs it in their browser (Home > Protocols, the Browser row's menu: Set up browser extension) and the browser open.";

/// One of the user's tabs.
pub(crate) struct Tab {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) active: bool,
}

/// The user's tabs that can be attached (web pages), waiting a little for
/// the extension to connect.
pub(crate) fn tabs() -> Result<Vec<Tab>, String> {
    start();
    for _ in 0..40 {
        if connected().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let v = call(json!({"op": "tabs"}), Duration::from_secs(10))?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(Tab {
                id: t["id"].as_i64()?,
                title: t["title"].as_str().unwrap_or("").to_string(),
                url: t["url"].as_str().unwrap_or("").to_string(),
                active: t["active"].as_bool().unwrap_or(false),
            })
        })
        .collect())
}

/// A session with the user's tab `tab`, through the debugger.
pub(crate) fn attach(tab: i64) -> Result<Arc<Cdp>, String> {
    call(json!({"op": "attach", "tab": tab}), Duration::from_secs(10)).map_err(|e| format!("Couldn't attach to the tab: {e}"))?;
    let me: Arc<OnceLock<Weak<Cdp>>> = Arc::default();
    let cdp = Cdp::over(Box::new(TabLink { tab, cdp: me.clone() }));
    let _ = me.set(Arc::downgrade(&cdp));
    CONN.lock().unwrap().sessions.insert(tab, Arc::downgrade(&cdp));
    Ok(cdp)
}

/// A tab's session's link: its calls become the extension's `cdp` ones.
struct TabLink {
    tab: i64,
    cdp: Arc<OnceLock<Weak<Cdp>>>,
}

impl Link for TabLink {
    fn send_text(&mut self, text: &str) -> std::io::Result<()> {
        let v: Value = serde_json::from_str(text).map_err(std::io::Error::other)?;
        let id = v["id"].clone();
        let cdp = self.cdp.clone();
        let msg = json!({"op": "cdp", "tab": self.tab, "method": v["method"], "params": v["params"]});
        if CONN.lock().unwrap().writer.is_none() {
            return Err(std::io::Error::other(NOT_CONNECTED));
        }
        request(msg, move |r| {
            let Some(s) = cdp.get().and_then(Weak::upgrade) else { return };
            s.deliver(match r {
                Ok(result) => json!({"id": id, "result": result}),
                Err(e) => json!({"id": id, "error": {"message": e}}),
            });
        });
        Ok(())
    }

    fn shut(&mut self) {
        let tab = self.tab;
        CONN.lock().unwrap().sessions.remove(&tab);
        request(json!({"op": "detach", "tab": tab}), |_| {});
    }
}

/// SHA-1 (the WebSocket handshake's).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut m = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend(bits.to_be_bytes());
    for chunk in m.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_hash_is_sha1() {
        let hex = |b: [u8; 20]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        // RFC 6455's example.
        let accept = base64::engine::general_purpose::STANDARD.encode(sha1(b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11"));
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn the_manifest_names_its_key() {
        let m: Value = serde_json::from_str(FILES[0].1).unwrap();
        assert!(m["key"].as_str().is_some_and(|k| k.len() > 300));
        assert!(m["permissions"].as_array().unwrap().iter().any(|p| p == "debugger"));
    }
}
