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
        let mut s = Socket { tls: rustls::StreamOwned::new(conn, tcp), closed: Arc::new(AtomicBool::new(false)) };
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
        loop {
            let mut reader = BufReader::new(&mut self.tls);
            let (op, payload) = match crate::cdp::ws_read(&mut reader) {
                Ok((op, p)) => (op, p),
                Err(_) => {
                    self.closed.store(true, Ordering::SeqCst);
                    return None;
                }
            };
            match op {
                0x8 => {
                    let code = payload.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]])).unwrap_or(0);
                    let why = String::from_utf8_lossy(payload.get(2..).unwrap_or(&[])).to_string();
                    eprintln!("Quick closed the socket: {code} {why}");
                    self.closed.store(true, Ordering::SeqCst);
                    return None;
                }
                0x9 => {
                    // Ping: answer it, then carry on.
                    let pong = crate::cdp::frame(0xA, &payload, true);
                    let _ = self.write_all(&pong);
                    let _ = self.flush();
                    continue;
                }
                0x1 => {
                    let v = unwrap_frame(&payload);
                    if v.is_none() {
                        eprintln!("Quick sent a frame Oak couldn't read: {}", String::from_utf8_lossy(&payload[..payload.len().min(300)]));
                    }
                    return v;
                }
                0x2 => {
                    // Quick answers in AWS event-stream messages, whose body
                    // is the JSON the app reads.
                    for body in crate::quick::event_bodies(&payload) {
                        match crate::quick::unwrap_frame(&body) {
                            Some(v) => return Some(v),
                            None => eprintln!("Quick sent a message Oak couldn't read: {}", String::from_utf8_lossy(&body[..body.len().min(300)])),
                        }
                    }
                    continue;
                }
                other => {
                    eprintln!("Quick frame opcode {other}, {} bytes", payload.len());
                    continue;
                }
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
