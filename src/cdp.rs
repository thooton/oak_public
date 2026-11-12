//! A small DevTools-protocol client: one websocket to one target (a tab),
//! with a reader thread that hands each answer to the call waiting for it
//! and events to whoever listens. duck.ai's browser and the browser pane
//! both drive Chromium with it.

use crate::tools::Head;
use crate::duck::Error;
use base64::Engine as _;
use serde_json::{Value, json};
use std::io::BufRead;
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Reads a starting browser's `stderr` on a thread named `name`: the first
/// line with `prefix` in it (its DevTools address, "ws://…") comes back from
/// where `prefix` starts; the rest of what it prints is read and dropped,
/// so its pipe never fills.
pub(crate) fn devtools_address(stderr: std::process::ChildStderr, name: &str, prefix: &'static str) -> std::io::Result<Receiver<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut tx = Some(tx);
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if let Some(at) = line.find(prefix)
                && let Some(t) = tx.take()
            {
                let _ = t.send(line[at..].to_string());
            }
        }
    })?;
    Ok(rx)
}

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// A call's answer, unless the caller gives its own timeout.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

// ------------------------------------------------------------- websocket

/// A masked client-to-server frame.
fn ws_frame(opcode: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 14);
    out.push(0x80 | opcode);
    let n = data.len();
    if n < 126 {
        out.push(0x80 | n as u8);
    } else if n < 65536 {
        out.push(0x80 | 126);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend((n as u64).to_be_bytes());
    }
    let mask: [u8; 4] = rand::random();
    out.extend(mask);
    out.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// One whole server-to-client message: (opcode, payload). Fragments are
/// joined; control frames come back as their own messages.
fn ws_read(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut data = vec![];
    let mut opcode = None;
    loop {
        let mut h = [0u8; 2];
        r.read_exact(&mut h)?;
        let (fin, op, masked) = (h[0] & 0x80 != 0, h[0] & 0x0f, h[1] & 0x80 != 0);
        let len = match h[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                r.read_exact(&mut b)?;
                u16::from_be_bytes(b) as u64
            }
            127 => {
                let mut b = [0u8; 8];
                r.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => n as u64,
        };
        let mut mask = [0u8; 4];
        if masked {
            r.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload)?;
        if masked {
            payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
        }
        if op >= 0x8 {
            return Ok((op, payload));
        }
        opcode.get_or_insert(op);
        data.extend(payload);
        if fin {
            return Ok((opcode.unwrap_or(1), data));
        }
    }
}

// ------------------------------------------------------------------- CDP

/// A DevTools session with one tab. A reader thread hands each answer to
/// the call waiting for it, and events to whoever is listening.
pub(crate) struct Cdp {
    writer: Mutex<TcpStream>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    listeners: Mutex<Vec<(u64, Sender<Value>)>>,
    closed: AtomicBool,
}

/// Events while it lives (`Cdp::listen`).
pub(crate) struct Listener {
    pub(crate) rx: Receiver<Value>,
    id: u64,
    cdp: Arc<Cdp>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.cdp.listeners.lock().unwrap().retain(|(id, _)| *id != self.id);
    }
}

impl Cdp {
    pub(crate) fn connect(ws_url: &str) -> Result<Arc<Cdp>, Error> {
        let down = |e: String| Error::Down(format!("Couldn't talk to the browser: {e}"));
        let rest = ws_url.strip_prefix("ws://").ok_or_else(|| down(format!("unexpected address {ws_url}")))?;
        let (host, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
        let mut sock = TcpStream::connect(host).map_err(|e| down(e.to_string()))?;
        let _ = sock.set_nodelay(true);
        let key = B64.encode(rand::random::<[u8; 16]>());
        let hello = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n");
        sock.write_all(hello.as_bytes()).map_err(|e| down(e.to_string()))?;
        // The headers, a byte at a time, so nothing of the first frame is read.
        sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut head = vec![];
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if sock.read(&mut b).map_err(|e| down(format!("handshake: {e}")))? != 1 {
                return Err(down("handshake: no answer".into()));
            }
            head.push(b[0]);
            if head.len() > 16_384 {
                return Err(down("handshake: answer too long".into()));
            }
        }
        let status = String::from_utf8_lossy(&head);
        if !status.lines().next().is_some_and(|l| l.contains(" 101 ")) {
            return Err(down(format!("handshake refused: {}", status.lines().next().unwrap_or(""))));
        }
        sock.set_read_timeout(None).ok();
        let reader = sock.try_clone().map_err(|e| down(e.to_string()))?;
        let cdp = Arc::new(Cdp {
            writer: Mutex::new(sock),
            next: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            listeners: Mutex::new(vec![]),
            closed: AtomicBool::new(false),
        });
        let c = cdp.clone();
        std::thread::Builder::new()
            .name("cdp".into())
            .spawn(move || c.read_loop(reader))
            .map_err(|e| down(e.to_string()))?;
        Ok(cdp)
    }

    fn read_loop(&self, sock: TcpStream) {
        let mut r = BufReader::with_capacity(1 << 16, sock);
        while let Ok((op, payload)) = ws_read(&mut r) {
            match op {
                0x8 => break,
                0x9 => {
                    let _ = self.writer.lock().unwrap().write_all(&ws_frame(0xA, &payload));
                    continue;
                }
                0x1 => {}
                _ => continue,
            }
            let Ok(v) = serde_json::from_slice::<Value>(&payload) else { continue };
            if let Some(id) = v["id"].as_u64() {
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(v);
                }
            } else {
                for (_, tx) in self.listeners.lock().unwrap().iter() {
                    let _ = tx.send(v.clone());
                }
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(json!({"__closed__": true}));
        }
        for (_, tx) in self.listeners.lock().unwrap().iter() {
            let _ = tx.send(json!({"method": "__closed__"}));
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn call_for(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Error> {
        self.call_until(method, params, timeout, &|| None)
    }

    /// `call_for` that also gives up as soon as `stop` names a reason
    /// (checked every 50 ms), with that reason as a page error.
    pub(crate) fn call_until(&self, method: &str, params: Value, timeout: Duration, stop: &dyn Fn() -> Option<String>) -> Result<Value, Error> {
        if self.is_closed() {
            return Err(Error::Down("The browser closed its connection.".into()));
        }
        let id = self.take_id();
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"id": id, "method": method, "params": params}).to_string();
        if let Err(e) = self.writer.lock().unwrap().write_all(&ws_frame(0x1, msg.as_bytes())) {
            self.pending.lock().unwrap().remove(&id);
            return Err(Error::Down(format!("Couldn't reach the browser: {e}")));
        }
        let deadline = std::time::Instant::now() + timeout;
        let answer = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left.min(Duration::from_millis(50))) {
                Ok(v) => break Ok(v),
                Err(mpsc::RecvTimeoutError::Timeout) if left.is_zero() => break Err(None),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(why) = stop() {
                        break Err(Some(why));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(json!({"__closed__": true})),
            }
        };
        match answer {
            Ok(v) if v.get("__closed__").is_some() => Err(Error::Down(format!("The browser closed during {method}."))),
            Ok(v) => match v.get("error") {
                Some(e) => Err(Error::Page(format!("{method}: {}", e["message"].as_str().unwrap_or(&e.to_string())))),
                None => Ok(v["result"].clone()),
            },
            Err(why) => {
                self.pending.lock().unwrap().remove(&id);
                match why {
                    Some(why) => Err(Error::Page(why)),
                    None => Err(Error::Down(format!("The browser didn't answer {method} in time."))),
                }
            }
        }
    }

    /// Sends a call without waiting for its answer (input events, acks):
    /// the answer is dropped when it comes.
    pub(crate) fn send(&self, method: &str, params: Value) {
        if self.is_closed() {
            return;
        }
        let id = self.take_id();
        let msg = json!({"id": id, "method": method, "params": params}).to_string();
        let _ = self.writer.lock().unwrap().write_all(&ws_frame(0x1, msg.as_bytes()));
    }

    pub(crate) fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_for(method, params, CALL_TIMEOUT)
    }

    /// Runs `expr` in the page; its value (promises awaited).
    pub(crate) fn eval_for(&self, expr: &str, timeout: Duration) -> Result<Value, Error> {
        let r = self.call_for("Runtime.evaluate", json!({"expression": expr, "returnByValue": true, "awaitPromise": true}), timeout)?;
        if let Some(e) = r.get("exceptionDetails") {
            let text = e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or("");
            return Err(Error::Page(format!("Script error in the page: {}", text.head(300))));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Every event from now until the listener is dropped.
    pub(crate) fn listen(self: &Arc<Self>) -> Listener {
        let (tx, rx) = mpsc::channel();
        let id = self.take_id();
        self.listeners.lock().unwrap().push((id, tx));
        Listener { rx, id, cdp: self.clone() }
    }

    /// The next id for a call or a listener.
    fn take_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}
