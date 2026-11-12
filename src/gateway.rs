//! The model gateway: Oak's providers for other programs on this machine
//! (a swarm of agents with its own tool loop, say), over HTTP on 127.0.0.1.
//! A program sends one request in the Oak format (`oak`) and gets the whole
//! reply back; the tool loop, budgets and scheduling are the caller's.
//!
//! It runs inside the app when `gateway.json` (Oak's folder) says
//! `"in_app": true`, and on its own with `oak --gateway` (no window; the
//! accounts are the same, read from the same files and the vault).
//! Only one binds the port; the other says so. `gateway.json`:
//! `{"port": 47801, "token": "...", "in_app": false}`, written with a fresh
//! token the first time. Every request needs `Authorization: Bearer <token>`.
//!
//! Routes:
//!
//! - `GET /v1/status`: the providers, their accounts or keys, and each
//!   account's plan limits (`?refresh=1` fetches them first, which takes a
//!   few seconds).
//! - `POST /v1/reply`: `{provider, model, system, messages, tools, max_tokens,
//!   thinking, effort, conv, key, max_retries}` (only `provider`, `model` and
//!   `messages` are needed). `key` pins a NodeClub request to one key (its id
//!   in `/v1/status`), so the caller can keep each key under its own limits.
//!   `max_retries` (default 2) stops a request the provider keeps turning
//!   down (rate limits, overload) instead of waiting it out: the caller knows
//!   better when to try again. The answer: `{ok, content, stop, detail,
//!   usage, usd, model, served_by, key, retries, error, limit_until}`, where
//!   `content` holds the reply's blocks in the Oak format, reasoning (with
//!   its `sig`) included, to go back in the next request.
//!
//! The server is a plain HTTP/1.1 one, a thread per connection, one request
//! per connection: no new dependency for a handful of local calls a minute.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::api::{Provider, Sse};
use crate::oak::{Event, Request, Usage};

const DEFAULT_PORT: u16 = 47801;
/// Requests larger than this are refused (a prompt is a few hundred KB).
const MAX_BODY: usize = 64 << 20;

static RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct Config {
    #[serde(default)]
    port: u16,
    #[serde(default)]
    token: String,
    #[serde(default)]
    in_app: bool,
}

fn config_path() -> std::path::PathBuf {
    crate::store::dir().join("gateway.json")
}

/// The saved configuration, made (with a fresh token) when there is none.
fn config() -> Config {
    let mut c: Config = crate::store::read_json(&config_path()).unwrap_or_default();
    let mut changed = false;
    if c.port == 0 {
        c.port = DEFAULT_PORT;
        changed = true;
    }
    if c.token.len() < 16 {
        let bytes: [u8; 24] = rand::random();
        c.token = format!("oakg_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
        changed = true;
    }
    if changed {
        let _ = crate::store::write_private_json(&config_path(), &c);
    }
    c
}

/// `oak --gateway`: serves until killed. Exit code 1 when the port is taken
/// (the app, or another `oak --gateway`, serves it).
/// Set when this process serves the gateway inside the app (never exit there).
pub static IN_APP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A request found the HTTP client broken: the headless gateway exits after replying.
static DYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn main(_args: &[String]) -> i32 {
    crate::api::load_auth();
    let c = config();
    let listener = match TcpListener::bind(("127.0.0.1", c.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("oak --gateway: 127.0.0.1:{} is taken ({e}); the running Oak may serve it already.", c.port);
            return 1;
        }
    };
    // Accounts and their limits, as the app keeps them fresh.
    std::thread::spawn(|| {
        loop {
            crate::api::refresh_accounts();
            std::thread::sleep(Duration::from_secs(120));
        }
    });
    eprintln!("oak gateway on http://127.0.0.1:{} (token in {})", c.port, config_path().display());
    serve(listener, c.token);
    0
}

/// In the app: starts the gateway when `gateway.json` asks for it.
pub fn start_in_app() {
    let Some(c) = crate::store::read_json::<Config>(&config_path()) else { return };
    if !c.in_app {
        return;
    }
    let c = config();
    IN_APP.store(true, Ordering::Relaxed);
    std::thread::spawn(move || {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", c.port)) {
            serve(l, c.token);
        }
    });
}

/// The gateway is serving in this process.
#[allow(dead_code)]
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

fn serve(listener: TcpListener, token: String) {
    RUNNING.store(true, Ordering::Relaxed);
    for conn in listener.incoming().flatten() {
        let token = token.clone();
        std::thread::spawn(move || {
            let _ = handle(conn, &token);
            if DYING.load(Ordering::Relaxed) {
                eprintln!("oak --gateway: the HTTP client is broken; exiting so a fresh gateway starts");
                // Let the other replies in flight finish their write first.
                std::thread::sleep(Duration::from_secs(2));
                std::process::exit(3);
            }
        });
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    auth: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> std::io::Result<HttpRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let (mut length, mut auth) = (0usize, None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                length = v.parse().unwrap_or(0);
            } else if k == "authorization" {
                auth = v.strip_prefix("Bearer ").map(str::to_string);
            }
        }
    }
    if length > MAX_BODY {
        return Err(std::io::Error::other("body too large"));
    }
    let mut body = vec![0; length];
    r.read_exact(&mut body)?;
    Ok(HttpRequest { method, path, query, auth, body })
}

fn respond(mut stream: &TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let text = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", text.len())?;
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

fn handle(stream: TcpStream, token: &str) -> std::io::Result<()> {
    let req = match read_request(&stream) {
        Ok(r) => r,
        Err(e) => return respond(&stream, 400, &json!({"ok": false, "error": e.to_string()})),
    };
    // Constant-time enough for a local token: the length is not secret.
    let authorized = req.auth.as_deref().is_some_and(|a| a.len() == token.len() && a.bytes().zip(token.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0);
    if !authorized {
        return respond(&stream, 401, &json!({"ok": false, "error": "missing or wrong gateway token (gateway.json in Oak's folder)"}));
    }
    stream.set_read_timeout(None)?;
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/status") => {
            if req.query.split('&').any(|p| p == "refresh=1") {
                crate::api::refresh_accounts();
            }
            respond(&stream, 200, &status())
        }
        ("POST", "/v1/reply") => {
            let body: Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return respond(&stream, 400, &json!({"ok": false, "error": format!("body is not JSON: {e}")})),
            };
            match request_of(&body) {
                Ok((r, key, max_retries)) => respond(&stream, 200, &reply(r, key, max_retries)),
                Err(e) => respond(&stream, 400, &json!({"ok": false, "error": e})),
            }
        }
        _ => respond(&stream, 404, &json!({"ok": false, "error": "no such route"})),
    }
}

fn limits_json(limits: &[crate::api::Limit]) -> Value {
    Value::Array(limits.iter().map(|l| json!({"label": l.label, "percent": l.percent, "resets_at": l.resets_at})).collect())
}

fn account_json(a: &crate::api::Account) -> Value {
    json!({
        "email": a.email, "plan": a.plan, "limits": limits_json(&a.limits), "fetched": a.fetched,
        "error": a.error, "usage_at": a.usage_at, "api_key": a.api_key, "fallback": a.fallback,
    })
}

/// What `/v1/status` answers.
fn status() -> Value {
    let claude: Vec<Value> = crate::api::accounts()
        .iter()
        .map(|a| {
            let mut v = account_json(a);
            v["id"] = json!(a.id);
            v
        })
        .collect();
    let codex: Vec<Value> = crate::codex::accounts()
        .iter()
        .map(|(key, a)| {
            let mut v = account_json(a);
            v["id"] = json!(key);
            v
        })
        .collect();
    let now = crate::api::now_ms();
    let nodeclub: Vec<Value> = crate::nodeclub::KEYS
        .list()
        .iter()
        .map(|k| json!({"id": k.id, "label": crate::provider::short_key(&k.key), "remote": k.remote, "usable": k.usable(now), "limited_until": k.limited_until, "rejected": k.rejected}))
        .collect();
    let freebuff: Vec<Value> = crate::freebuff::accounts().iter().map(|a| json!({"id": a.id, "email": a.email, "error": a.error})).collect();
    json!({
        "ok": true,
        "version": crate::update::VERSION,
        "claude": claude,
        "codex": codex,
        "nodeclub": nodeclub,
        "freebuff": freebuff,
        "local": crate::local::on(),
        "models": {
            "nodeclub": crate::nodeclub::MODELS.iter().map(|m| json!({"id": m.id, "context": m.context, "max_output": m.max_output})).collect::<Vec<_>>(),
            "freebuff": crate::freebuff::MODELS.iter().map(|m| json!({"id": m.id, "context": m.context})).collect::<Vec<_>>(),
        },
    })
}

fn provider_of(id: &str) -> Option<Provider> {
    serde_json::from_value(json!(id)).ok()
}

/// The Oak request a `/v1/reply` body asks for, the NodeClub key it pins,
/// and how many retries it allows.
fn request_of(b: &Value) -> Result<(Request, Option<String>, u32), String> {
    let model = b["model"].as_str().ok_or("`model` is missing")?.to_string();
    let provider = match b["provider"].as_str() {
        Some(p) => provider_of(p).ok_or_else(|| format!("no provider {p}"))?,
        None => crate::providers::Kind::of(&model),
    };
    let messages = b["messages"].as_array().cloned().ok_or("`messages` is missing")?;
    let tools = b["tools"].as_array().cloned().unwrap_or_default();
    let conv = b["conv"].as_str().map(str::to_string);
    // `account`: the Claude account (id) or Codex login the conversation goes through. It
    // seats the conversation there, warm for an hour, so rotation keeps it there while the
    // account can make requests (the caller keeps each account under its own usage caps).
    let account = b["account"].as_str().map(str::to_string).or_else(|| b["account"].as_u64().map(|n| n.to_string()));
    if let (Some(acc), Some(c)) = (&account, &conv) {
        if matches!(provider, crate::providers::Kind::Claude | crate::providers::Kind::Codex) {
            crate::rotation::used(provider, c, acc, 3_600_000, crate::api::now_ms());
        }
    }
    let r = Request {
        provider,
        model,
        max_tokens: b["max_tokens"].as_u64().unwrap_or(0),
        system: b["system"].as_str().unwrap_or("").to_string(),
        messages: crate::oak::clean(messages),
        tools,
        tools_off: b["tools_off"].as_bool().unwrap_or(false),
        thinking: b["thinking"].as_bool().unwrap_or(false),
        effort: b["effort"].as_str().map(str::to_string),
        conv,
        ..Default::default()
    };
    let key = b["key"].as_str().map(str::to_string);
    let retries = b["max_retries"].as_u64().unwrap_or(2) as u32;
    Ok((r, key, retries))
}

/// Sends `r` and gathers its whole reply.
fn reply(r: Request, key: Option<String>, max_retries: u32) -> Value {
    let started = std::time::Instant::now();
    let conv = r.conv.clone().unwrap_or_default();
    if let Some(k) = &key {
        crate::nodeclub::pin(&conv, Some(k));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || crate::api::stream_messages(r, tx));
    let mut a = Assembler::default();
    let mut error = None;
    let mut limit_until = None;
    let mut rotated = vec![];
    let mut ended = false;
    for s in rx.iter() {
        match s {
            Sse::Event(e) => {
                if let Event::Retry { attempt, wait, reason } = &e {
                    a.retries.push(json!({"attempt": attempt, "wait": wait, "reason": reason}));
                    if a.retries.len() as u32 > max_retries {
                        error = Some(format!("gave up after {} retries: {reason}", a.retries.len() - 1));
                        break;
                    }
                }
                a.feed(e);
            }
            Sse::Done => {
                ended = true;
                break;
            }
            Sse::Err(e) => {
                ended = true;
                error = Some(e);
                break;
            }
            Sse::Limit(at) => {
                ended = true;
                limit_until = Some(at);
                error = Some("every account is at its usage limit".into());
                break;
            }
            Sse::Rotated(to) => rotated.push(to),
        }
    }
    if error.is_some() {
        ended = true;
    }
    // Dropping the receiver ends the stream at its next event.
    drop(rx);
    if error.is_none() {
        let died = worker.join().is_err();
        if !ended || died {
            // The request thread panicked before saying anything (its sender dropped): never
            // a success. When the HTTP client's own runtime has died ("core thread panicked"),
            // every later request fails the same way: the process ends after this reply so
            // its wrapper starts a fresh one.
            error = Some("the request thread died without a reply (gateway HTTP client broken; the gateway restarts)".into());
            if !crate::gateway::IN_APP.load(Ordering::Relaxed) {
                DYING.store(true, Ordering::Relaxed);
            }
        }
    }
    if key.is_some() {
        crate::nodeclub::pin(&conv, None);
    }
    let (content, stop, detail) = a.finish();
    let u = a.usage;
    json!({
        "ok": error.is_none(),
        "error": error,
        "limit_until": limit_until,
        "content": content,
        "stop": stop,
        "detail": detail,
        "usage": {"input": u.input, "cache_read": u.cache_read, "write": u.write, "output": u.output, "reasoning": u.reasoning},
        "usd": a.usd,
        "model": a.model,
        "served_by": a.served_by,
        "key": key,
        "rotated": rotated,
        "retries": a.retries,
        "status": a.status,
        "ms": started.elapsed().as_millis() as u64,
    })
}

/// A reply's blocks, put together from its events.
#[derive(Default)]
struct Assembler {
    blocks: Vec<Option<Value>>,
    inputs: Vec<String>,
    stop: String,
    detail: Option<String>,
    usage: Usage,
    usd: f64,
    model: String,
    served_by: Option<String>,
    retries: Vec<Value>,
    status: Option<u16>,
}

impl Assembler {
    fn slot(&mut self, i: usize) {
        if self.blocks.len() <= i {
            self.blocks.resize(i + 1, None);
            self.inputs.resize(i + 1, String::new());
        }
    }

    fn feed(&mut self, e: Event) {
        match e {
            Event::Start { model, .. } => self.model = model,
            Event::Usage(u) => self.usage.merge(&u),
            Event::Block { index, block } => {
                self.slot(index);
                self.blocks[index] = Some(block);
                self.inputs[index].clear();
            }
            Event::Text { index, text } | Event::Reasoning { index, text } => {
                self.slot(index);
                if let Some(b) = self.blocks[index].as_mut() {
                    let t = b["text"].as_str().unwrap_or("").to_string() + &text;
                    b["text"] = json!(t);
                }
            }
            Event::Sig { index, sig } => {
                self.slot(index);
                if let Some(b) = self.blocks[index].as_mut() {
                    b["sig"] = json!(sig);
                }
            }
            Event::Input { index, json } => {
                self.slot(index);
                self.inputs[index].push_str(&json);
            }
            Event::BlockEnd { .. } => {}
            Event::Stop { reason, detail } => {
                self.stop = reason;
                if detail.is_some() {
                    self.detail = detail;
                }
            }
            Event::Cost(usd) => self.usd += usd,
            Event::ServedBy(s) => self.served_by = Some(s),
            // The request went again from the start: what came is void.
            Event::Retry { .. } | Event::ThreadRestart(_) => {
                self.blocks.clear();
                self.inputs.clear();
                self.usage = Usage::default();
            }
            Event::Headers(status) => self.status = Some(status),
        }
    }

    fn finish(&mut self) -> (Value, String, Option<String>) {
        let mut out = vec![];
        for (i, b) in self.blocks.iter().enumerate() {
            let Some(mut b) = b.clone() else { continue };
            if b["type"] == "tool_call" {
                let raw = self.inputs.get(i).map(String::as_str).unwrap_or("");
                b["input"] = if raw.trim().is_empty() {
                    json!({})
                } else {
                    // Input the model wrote as broken JSON goes back as it
                    // was, for the caller to report.
                    serde_json::from_str(raw).unwrap_or_else(|_| json!({"_raw": raw}))
                };
            }
            out.push(b);
        }
        (Value::Array(out), self.stop.clone(), self.detail.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_text_reasoning_and_calls() {
        let mut a = Assembler::default();
        a.feed(Event::Start { id: "r".into(), model: "m".into() });
        a.feed(Event::Block { index: 0, block: json!({"type": "reasoning", "text": ""}) });
        a.feed(Event::Reasoning { index: 0, text: "think".into() });
        a.feed(Event::Sig { index: 0, sig: "s".into() });
        a.feed(Event::Block { index: 1, block: json!({"type": "text", "text": ""}) });
        a.feed(Event::Text { index: 1, text: "he".into() });
        a.feed(Event::Text { index: 1, text: "llo".into() });
        a.feed(Event::Block { index: 2, block: json!({"type": "tool_call", "id": "c1", "name": "compile", "input": {}}) });
        a.feed(Event::Input { index: 2, json: "{\"cpp\":".into() });
        a.feed(Event::Input { index: 2, json: "\"int x;\"}".into() });
        a.feed(Event::Stop { reason: "tool_calls".into(), detail: None });
        let (c, stop, _) = a.finish();
        assert_eq!(stop, "tool_calls");
        assert_eq!(c[0]["text"], "think");
        assert_eq!(c[0]["sig"], "s");
        assert_eq!(c[1]["text"], "hello");
        assert_eq!(c[2]["input"]["cpp"], "int x;");
    }

    #[test]
    fn a_retry_voids_what_came() {
        let mut a = Assembler::default();
        a.feed(Event::Block { index: 0, block: json!({"type": "text", "text": ""}) });
        a.feed(Event::Text { index: 0, text: "half".into() });
        a.feed(Event::Retry { attempt: 1, wait: 4, reason: "overloaded".into() });
        a.feed(Event::Block { index: 0, block: json!({"type": "text", "text": ""}) });
        a.feed(Event::Text { index: 0, text: "whole".into() });
        let (c, _, _) = a.finish();
        assert_eq!(c.as_array().unwrap().len(), 1);
        assert_eq!(c[0]["text"], "whole");
    }

    #[test]
    fn reads_a_reply_body() {
        let (r, key, retries) = request_of(&json!({"provider": "nodeclub", "model": "qwen3.8-27b", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}], "key": "k1", "max_retries": 5})).unwrap();
        assert_eq!(r.provider, Provider::NodeClub);
        assert_eq!(key.as_deref(), Some("k1"));
        assert_eq!(retries, 5);
        assert!(request_of(&json!({"model": "x"})).is_err());
    }
}
