//! The socket Quick's chat runs on: a WebSocket over TLS (rustls), Quick's
//! frames in and out, and the turn's loop: send the user turn, read frames
//! until the message ends, and answer each tool call by running it in Oak's
//! working folder.
//!
//! The socket frames go through [`crate::cdp::frame`] and
//! [`crate::cdp::ws_read`], the same code the browser pane uses. They are
//! written here without the permessage-deflate extension, so what comes back
//! is plain text.

use super::*;
use std::io::{BufReader, Read, Write};
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
        let mut s = Socket { tls: rustls::StreamOwned::new(conn, tcp), closed: Arc::new(AtomicBool::new(false)), held: std::collections::VecDeque::new() };
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

    /// Connects to a full `wss://` address (the chat path is built by the
    /// caller; the data plane has its own).
    pub(crate) fn connect_url(url: &str) -> Result<Socket, String> {
        let rest = url.strip_prefix("wss://").ok_or_else(|| format!("not a wss address: {url}"))?;
        let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
        Socket::connect(host, &path)
    }

    /// Sends a frame as it is, not in the chat envelope (the data plane's
    /// protocol is its own).
    pub(crate) fn send_raw(&mut self, v: &Value) -> Result<(), String> {
        let text = v.to_string();
        let frame = crate::cdp::frame(0x2, text.as_bytes(), true);
        self.write_all(&frame).map_err(|e| format!("Quick's socket closed: {e}"))?;
        self.flush().ok();
        Ok(())
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

    /// Reads one WebSocket frame into `held`. False when the socket ended.
    fn fill(&mut self) -> bool {
        loop {
            let mut reader = BufReader::new(&mut self.tls);
            let (op, payload) = match crate::cdp::ws_read(&mut reader) {
                Ok((op, p)) => (op, p),
                Err(_) => {
                    self.closed.store(true, Ordering::SeqCst);
                    return false;
                }
            };
            match op {
                0x8 => {
                    let code = payload.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]])).unwrap_or(0);
                    let why = String::from_utf8_lossy(payload.get(2..).unwrap_or(&[])).to_string();
                    eprintln!("Quick closed the socket: {code} {why}");
                    self.closed.store(true, Ordering::SeqCst);
                    return false;
                }
                0x9 => {
                    let pong = crate::cdp::frame(0xA, &payload, true);
                    let _ = self.write_all(&pong);
                    let _ = self.flush();
                    continue;
                }
                0x1 => {
                    if let Some(v) = unwrap_frame(&payload) {
                        self.held.push_back(v);
                        return true;
                    }
                    continue;
                }
                0x2 => {
                    // Quick answers in AWS event-stream messages, whose body
                    // is the JSON the app reads; one frame carries several.
                    let mut any = false;
                    for body in crate::quick::event_bodies(&payload) {
                        if let Some(v) = unwrap_frame(&body) {
                            self.held.push_back(v);
                            any = true;
                        }
                    }
                    if any {
                        return true;
                    }
                    continue;
                }
                _ => continue,
            }
        }
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
