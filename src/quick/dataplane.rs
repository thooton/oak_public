//! This machine's device data plane: the connection that keeps Oak's outpost
//! online with Quick's server, so the tools Quick's agent asks for are routed
//! to Oak (which runs them) instead of its cloud sandbox.
//!
//! Quick's desktop app runs this itself (`qw_outpost/_dataplane_service.py` in
//! its agent): a WebSocket from the machine to the server, on which the
//! machine says it is there and the server hands it work. Its outpost is
//! `ONLINE` while that connection lives and `OFFLINE` when it goes — which is
//! why an outpost cannot be brought up by a REST call, and why registering one
//! without a data plane leaves it a name with no device behind it.
//!
//! The connection's own handshake is what this module is working out: the app
//! opens it with a token and a device identity, and its frames are the
//! protocol's, not the chat socket's. Until that is known, `connect` reports
//! what it tried rather than pretending to be online.

use super::*;

/// Where the data plane connects: the same host as the chat socket.
pub fn endpoint(host: &str) -> String {
    format!("wss://{host}/outpost/dataplane")
}

/// Opens this machine's data plane, returning what happened. Err explains
/// what Quick answered (the handshake is not yet right).
pub fn connect(host: &str, token: &str) -> Result<(), String> {
    let outpost = outpost::id();
    let url = endpoint(host);
    println!("Quick data plane: {url} as outpost {outpost} (token {}…)", &token[..token.len().min(12)]);
    let mut sock = socket::Socket::connect_url(&url)?;
    // What the app says first: this machine, signed in, ready for work.
    sock.send_raw(&json!({
        "type": "connect",
        "outpostId": outpost,
        "deviceName": std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Oak".into()),
    }))?;
    match sock.next() {
        Some(v) => Err(format!("Quick answered the data plane's connect with: {}", serde_json::to_string(&v).unwrap_or_default())),
        None => Err("Quick closed the data plane's connection without answering.".into()),
    }
}
