//! The socket Quick's chat runs on: a WebSocket over TLS (rustls), Quick's
//! frames in and out, and the turn's loop: send the user turn, read frames
//! until the message ends, and answer each tool call by running it in Oak's
//! working folder.
//!
//! The socket frames go through [`crate::cdp::frame`] and
//! [`crate::cdp::ws_read`], the same code the browser pane uses. They are
//! written here without the permessage-deflate extension, so what comes back
//! is plain text. Frames are read straight off the TLS stream, never through
//! a reader that reads ahead (what it read ahead would be lost with it).

use super::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A live chat socket.
pub(crate) struct Socket {
    tls: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    closed: Arc<AtomicBool>,
    /// Messages read from the frame in hand but not yet returned (a frame
    /// carries several).
    held: std::collections::VecDeque<Value>,
    /// Quick's numbered frames, put back in order.
    replay: Replay,
    /// When Oak last told Quick how far it has read (`ping` with `ack`).
    acked_at: std::time::Instant,
}

/// The `replayContext` a socket sets its delivery up with
/// (`v1;setup;<token>`, the token random, as the app makes it).
pub(crate) fn replay_setup() -> String {
    format!("v1;setup;{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()))
}

/// How long a gap in Quick's numbered frames is waited on before the frames
/// after it go on without it.
const GAP_WAIT: Duration = Duration::from_secs(3);

/// The most frames asked for again at once (the app's batch).
const RETRANSMIT_BATCH: usize = 1000;

/// Quick's delivery layer, as its app reads it: each frame carries its
/// number (`_transport.sequence`, from 1), and frames may come out of
/// order, twice, or not at all. Frames go on in their numbers' order; one
/// seen already is dropped; a missing one is asked for again
/// (`_transport_retransmit`) when Quick said it can resend
/// (`_transport_setup_ok`), and a gap that stays open past `GAP_WAIT` is
/// given up on rather than holding the reply back for good. Frames without
/// a number (keepalives, the session's own) go on as they come.
#[derive(Default)]
pub(crate) struct Replay {
    /// The last frame gone on in order.
    last: u64,
    /// Frames come ahead of a gap, by number.
    ahead: std::collections::BTreeMap<u64, Value>,
    /// Quick can resend frames.
    resends: bool,
    /// The numbers asked for again.
    asked: std::collections::BTreeSet<u64>,
    /// Since when the oldest open gap has waited.
    gap_since: Option<std::time::Instant>,
    /// Frames numbered at all (the delivery layer is on).
    pub(crate) numbered: bool,
}

impl Replay {
    /// Takes one frame from Quick: the frames that may go on now, in order,
    /// and the numbers to ask Quick for again.
    pub(crate) fn take(&mut self, mut v: Value) -> (Vec<Value>, Vec<u64>) {
        let kind = v["type"].as_str().unwrap_or("").to_string();
        match kind.as_str() {
            "_transport_setup_ok" => {
                self.resends = v["features"].as_array().is_some_and(|f| f.iter().any(|x| x == "retransmit"));
                return (vec![], vec![]);
            }
            // Quick has sent all it can of what was asked for: what it
            // couldn't is given up on.
            "_transport_retransmit_complete" => {
                let gone = v["unavailable"].as_array().is_some_and(|u| !u.is_empty());
                return (if gone { self.skip_gap() } else { vec![] }, vec![]);
            }
            k if k.starts_with("_transport_") => {
                if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                    eprintln!("TRANSPORT {v}");
                }
                // A restart or a refusal: what is held goes on as it is.
                return (if k == "_transport_restart" || k == "_transport_rejected" { self.flush() } else { vec![] }, vec![]);
            }
            _ => {}
        }
        let Some(seq) = v.get("_transport").and_then(|t| t["sequence"].as_str().and_then(|s| s.parse::<u64>().ok()).or(t["sequence"].as_u64())) else {
            return (vec![v], vec![]);
        };
        self.numbered = true;
        if let Some(o) = v.as_object_mut() {
            o.remove("_transport");
        }
        if seq <= self.last || self.ahead.contains_key(&seq) {
            return (vec![], vec![]); // seen already
        }
        self.asked.remove(&seq);
        if seq == self.last + 1 {
            self.last = seq;
            let mut out = vec![v];
            out.extend(self.drain());
            return (out, vec![]);
        }
        self.ahead.insert(seq, v);
        self.gap_since.get_or_insert_with(std::time::Instant::now);
        let mut ask = vec![];
        if self.resends {
            for n in self.last + 1..seq {
                if ask.len() >= RETRANSMIT_BATCH {
                    break;
                }
                if !self.ahead.contains_key(&n) && self.asked.insert(n) {
                    ask.push(n);
                }
            }
        }
        (vec![], ask)
    }

    /// The frames held past a gap that has waited too long (`now`).
    pub(crate) fn overdue(&mut self, now: std::time::Instant) -> Vec<Value> {
        match self.gap_since {
            Some(t) if now.duration_since(t) >= GAP_WAIT => self.skip_gap(),
            _ => vec![],
        }
    }

    /// How far Oak has read, as the app acks it.
    pub(crate) fn ack(&self) -> String {
        self.last.to_string()
    }

    /// Everything held, in order (the socket ended, or Quick restarted).
    pub(crate) fn flush(&mut self) -> Vec<Value> {
        let out: Vec<Value> = std::mem::take(&mut self.ahead).into_values().collect();
        self.gap_since = None;
        self.asked.clear();
        out
    }

    /// Gives up on the oldest gap: the frames after it go on.
    fn skip_gap(&mut self) -> Vec<Value> {
        let Some(&next) = self.ahead.keys().next() else {
            self.gap_since = None;
            return vec![];
        };
        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
            eprintln!("TRANSPORT frames {}..{} never came; going on without them", self.last + 1, next - 1);
        }
        self.asked.retain(|n| *n >= next);
        self.last = next - 1;
        self.gap_since = None;
        self.drain()
    }

    /// The frames that follow on from `last`, taken out of `ahead`.
    fn drain(&mut self) -> Vec<Value> {
        let mut out = vec![];
        while let Some(v) = self.ahead.remove(&(self.last + 1)) {
            self.last += 1;
            out.push(v);
        }
        self.gap_since = if self.ahead.is_empty() { None } else { Some(std::time::Instant::now()) };
        out
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    use rustls::RootCertStore;
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        // reqwest's own TLS brings in a second provider (aws-lc-rs), so
        // rustls can't pick one on its own: this is Oak's socket, so it
        // says which.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        Arc::new(config)
    });
    CONFIG.clone()
}

impl Socket {
    /// Connects to `host` (no scheme) at `path` (which carries the query),
    /// as Quick's desktop app does: `Origin: file://`.
    pub(crate) fn connect(host: &str, path: &str) -> Result<Socket, String> {
        use rustls::pki_types::ServerName;
        let addr = format!("{host}:443");
        let tcp = TcpStream::connect(&addr).map_err(|e| format!("Couldn't reach Quick ({addr}): {e}"))?;
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(600)));
        let name = ServerName::try_from(host.to_string()).map_err(|_| format!("Quick's host name is unusual: {host}"))?;
        let conn = rustls::ClientConnection::new(tls_config(), name).map_err(|e| format!("TLS to Quick failed: {e}"))?;
        let mut s = Socket {
            tls: rustls::StreamOwned::new(conn, tcp),
            closed: Arc::new(AtomicBool::new(false)),
            held: std::collections::VecDeque::new(),
            replay: Replay::default(),
            acked_at: std::time::Instant::now(),
        };
        let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
        let hello = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nPragma: no-cache\r\nCache-Control: no-cache\r\n\
             User-Agent: {}\r\nUpgrade: websocket\r\nOrigin: file://\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\nAccept-Language: en-US\r\n\r\n",
            user_agent()
        );
        s.write_all(hello.as_bytes()).map_err(|e| format!("Couldn't open Quick's socket: {e}"))?;
        s.flush().ok();
        // The handshake's headers, a byte at a time, so no frame is eaten.
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match s.read(&mut b) {
                Ok(1) => head.push(b[0]),
                Ok(_) => return Err("Quick closed the socket during the handshake.".into()),
                Err(e) => return Err(format!("Quick's handshake failed: {e}")),
            }
            if head.len() > 32_768 {
                return Err("Quick's handshake answer was too long.".into());
            }
        }
        let status = String::from_utf8_lossy(&head);
        let first = status.lines().next().unwrap_or("");
        if !first.contains(" 101 ") {
            let code = first.split_whitespace().nth(1).unwrap_or("?");
            return Err(if code == "401" {
                "Quick refused the sign-in (401): sign in again on Home.".into()
            } else {
                format!("Quick refused the socket: {first}")
            });
        }
        Ok(s)
    }

    /// Sends one JSON frame in Quick's envelope. The app sends them as
    /// binary frames (its socket's writer does that for text too), which is
    /// what the server takes.
    pub(crate) fn send(&mut self, v: &Value) -> Result<(), String> {
        let text = envelope(v);
        let frame = crate::cdp::frame(0x2, text.as_bytes(), true);
        self.write_all(&frame).map_err(|e| format!("Quick's socket closed: {e}"))?;
        self.flush().ok();
        Ok(())
    }

    /// The next frame's JSON (a text frame), or None at the end.
    pub(crate) fn next(&mut self) -> Option<Value> {
        self.next_of(&|_| true)
    }

    /// The next frame `keep` accepts. Quick packs several messages into one
    /// WebSocket frame (a chunk of text deltas, a status, a usage reading
    /// together), so a frame is read to its end before the next one is asked
    /// for: taking one message per frame would drop the rest, which is how a
    /// reply's text arrived in pieces with gaps.
    pub(crate) fn next_of(&mut self, keep: &dyn Fn(&Value) -> bool) -> Option<Value> {
        loop {
            if let Some(v) = self.held.pop_front() {
                if keep(&v) {
                    return Some(v);
                }
                continue;
            }
            if !self.fill() {
                return None;
            }
        }
    }

    /// Reads one WebSocket frame into `held`, in Quick's order
    /// ([`Replay`]). False when the socket ended and nothing is left.
    fn fill(&mut self) -> bool {
        loop {
            // Straight from the TLS stream: a reader of its own would read
            // ahead and drop, with it, the frames that came in the same read
            // (whole stretches of a reply's text went that way).
            let (op, payload) = match crate::cdp::ws_read(&mut self.tls) {
                Ok((op, p)) => (op, p),
                Err(_) => {
                    self.closed.store(true, Ordering::SeqCst);
                    return self.end();
                }
            };
            let mut got = vec![];
            match op {
                0x8 => {
                    let code = payload.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]])).unwrap_or(0);
                    let why = String::from_utf8_lossy(payload.get(2..).unwrap_or(&[])).to_string();
                    eprintln!("Quick closed the socket: {code} {why}");
                    self.closed.store(true, Ordering::SeqCst);
                    return self.end();
                }
                0x9 => {
                    let pong = crate::cdp::frame(0xA, &payload, true);
                    let _ = self.write_all(&pong);
                    let _ = self.flush();
                    continue;
                }
                0x1 => got.extend(unwrap_frame(&payload)),
                // Quick answers in AWS event-stream messages, whose body
                // is the JSON the app reads; one frame carries several.
                0x2 => got.extend(crate::quick::event_bodies(&payload).iter().filter_map(|b| unwrap_frame(b))),
                _ => continue,
            }
            let mut ask = vec![];
            for v in got {
                let (ready, missing) = self.replay.take(v);
                self.held.extend(ready);
                ask.extend(missing);
            }
            self.held.extend(self.replay.overdue(std::time::Instant::now()));
            if !ask.is_empty() {
                if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                    eprintln!("TRANSPORT asking Quick again for {} frame(s) from {}", ask.len(), ask[0]);
                }
                let _ = self.send(&json!({"type": "_transport_retransmit", "payload": {"seqs": ask}}));
            }
            self.ack_now_and_then();
            if !self.held.is_empty() {
                return true;
            }
        }
    }

    /// The socket ended: what was held past a gap goes on, in order.
    fn end(&mut self) -> bool {
        let rest = self.replay.flush();
        let any = !rest.is_empty();
        self.held.extend(rest);
        any
    }

    /// Tells Quick how far Oak has read, every so often, the way the app
    /// does: in a `ping`'s `ack`.
    fn ack_now_and_then(&mut self) {
        if !self.replay.numbered || self.acked_at.elapsed() < Duration::from_secs(15) {
            return;
        }
        self.acked_at = std::time::Instant::now();
        let ack = self.replay.ack();
        let _ = self.send(&json!({"type": "ping", "payload": {"timestamp": crate::api::now_ms(), "ack": ack}}));
    }

    fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.tls.write_all(b)
    }

    /// Reads the handshake's answer (never a frame: the server answers
    /// before it sends one).
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.tls.read(b)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tls.flush()
    }
}

fn user_agent() -> String {
    format!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) AWSQuick/{APP_VERSION} Chrome/150.0.7871.250 Electron/43.8.7 Safari/537.36")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: u64, text: &str) -> Value {
        json!({"type": "text_delta", "payload": {"delta": text}, "_transport": {"version": 1, "sequence": n.to_string()}})
    }

    fn texts(v: &[Value]) -> String {
        v.iter().map(|f| f["payload"]["delta"].as_str().unwrap_or("")).collect()
    }

    /// Frames go on in their numbers' order, each once, without their
    /// numbers; a gap is asked for again when Quick can resend.
    #[test]
    fn quicks_frames_are_put_back_in_order() {
        let mut r = Replay::default();
        assert_eq!(r.take(json!({"type": "_transport_setup_ok", "version": 1, "features": ["retransmit"]})).0.len(), 0);
        let mut out = vec![];
        let (a, ask) = r.take(numbered(1, "a"));
        out.extend(a);
        assert!(ask.is_empty());
        let (a, ask) = r.take(numbered(3, "c"));
        out.extend(a);
        assert_eq!(ask, vec![2], "the missing frame is asked for");
        let (a, ask) = r.take(numbered(4, "d"));
        out.extend(a);
        assert!(ask.is_empty(), "asked once");
        out.extend(r.take(numbered(1, "a")).0); // twice
        out.extend(r.take(numbered(2, "b")).0);
        out.extend(r.take(numbered(3, "c")).0); // twice
        assert_eq!(texts(&out), "abcd");
        assert!(out.iter().all(|f| f.get("_transport").is_none()));
        // Unnumbered frames go on as they come.
        assert_eq!(r.take(json!({"type": "keepalive"})).0.len(), 1);
        assert_eq!(r.ack(), "4");
    }

    /// A gap Quick can't fill holds the reply back only so long.
    #[test]
    fn a_gap_that_never_fills_is_given_up_on() {
        let mut r = Replay::default();
        r.take(numbered(1, "a"));
        assert!(r.take(numbered(3, "c")).0.is_empty());
        let now = std::time::Instant::now();
        assert!(r.overdue(now).is_empty(), "not yet");
        let later = now + GAP_WAIT + Duration::from_millis(10);
        assert_eq!(texts(&r.overdue(later)), "c");
        // Frame 2 arriving now is too late, and isn't taken.
        assert!(r.take(numbered(2, "b")).0.is_empty());
        assert_eq!(texts(&r.take(numbered(4, "d")).0), "d");
        // Quick saying it can't resend gives up at once.
        let mut q = Replay::default();
        q.take(json!({"type": "_transport_setup_ok", "features": ["retransmit"]}));
        q.take(numbered(1, "a"));
        q.take(numbered(5, "e"));
        let (out, _) = q.take(json!({"type": "_transport_retransmit_complete", "unavailable": [2, 3, 4]}));
        assert_eq!(texts(&out), "e");
        // Without resends, nothing is asked for.
        let mut n = Replay::default();
        n.take(numbered(1, "a"));
        assert!(n.take(numbered(3, "c")).1.is_empty());
    }

    /// Frames that come in one read are each read: none is lost with a
    /// reader's read-ahead.
    #[test]
    fn frames_that_come_together_are_all_read() {
        let mut wire = vec![];
        for i in 0..5u8 {
            wire.extend(crate::cdp::frame(0x1, format!("frame {i}").as_bytes(), false));
        }
        let mut r = std::io::Cursor::new(wire);
        for i in 0..5u8 {
            let (op, payload) = crate::cdp::ws_read(&mut r).unwrap();
            assert_eq!((op, payload), (0x1, format!("frame {i}").into_bytes()));
        }
    }
}
