//! A model on the user's own llama.cpp server (`llama-server`), through its
//! Anthropic Messages endpoint. The user starts the server themselves (a
//! .bat of theirs); OAK only connects, to the URL kept in local.json with
//! the tokens used so far, which the account box shows.
//!
//! The model is small and slow next to the others, and its context short,
//! so it gets a short system prompt and the basic tools only
//! (`tools::LOCAL_TOOLS`), summaries start early (`rolling::config_for`),
//! and thinking is off at low effort and for summaries and titles.

use crate::api::{self, Sse};
use crate::oak::{self, Request};
use crate::provider;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// What OAK names the server's model: "local/" and the server's alias.
pub const PREFIX: &str = "local/";

/// The URL a llama-server listens on unless told otherwise.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// The context window when the server can't say (`--ctx-size` of the
/// launcher), and the most a reply may write.
const DEFAULT_CONTEXT: u64 = 32_768;
const MAX_OUTPUT: u64 = 8_192;

/// local.json: the server, its model as it was last seen, and the tokens
/// used through it, all time.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Server {
    pub url: String,
    /// The server's model alias (`--alias`), e.g. "qwen3.8-27b".
    pub alias: String,
    /// Its context window (the server's `n_ctx`).
    pub context: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub requests: u64,
}

impl Server {
    /// Every token through it: prompts (new and from its cache) and replies.
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.output
    }
}

static STATE: Mutex<Option<Server>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    crate::store::dir().join("local.json")
}

fn save(s: &Server) {
    let _ = crate::store::write_private_json(&path(), s);
}

/// Loads local.json. True when a server was added.
pub fn load() -> bool {
    let s = crate::store::read_json::<Server>(&path()).filter(|s| !s.url.is_empty());
    let on = s.is_some();
    *STATE.lock().unwrap() = s;
    on
}

/// A server was added.
pub fn on() -> bool {
    STATE.lock().unwrap().is_some()
}

/// The server as kept, if one was added.
pub fn server() -> Option<Server> {
    STATE.lock().unwrap().clone()
}

/// `url` without a trailing slash or a `/v1` the user pasted.
fn base(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix("/v1").unwrap_or(u);
    if u.contains("://") { u.to_string() } else { format!("http://{u}") }
}

/// Adds the server at `url` (its model and context from it when it is
/// running, else the defaults; they are read again at each start).
pub fn add(url: &str) {
    let url = base(if url.trim().is_empty() { DEFAULT_URL } else { url });
    let old = server().filter(|s| s.url == url).unwrap_or_default();
    let mut s = Server { url, alias: old.alias, context: old.context, ..old };
    if let Ok((alias, context)) = probe(&s.url) {
        s.alias = alias;
        s.context = context;
    }
    if s.alias.is_empty() {
        s.alias = "local".into();
    }
    if s.context == 0 {
        s.context = DEFAULT_CONTEXT;
    }
    save(&s);
    *STATE.lock().unwrap() = Some(s);
}

/// Forgets the server (and its token count).
pub fn remove() {
    let _ = std::fs::remove_file(path());
    *STATE.lock().unwrap() = None;
}

/// The server's model alias and context window, from /v1/models and /props.
fn probe(url: &str) -> Result<(String, u64), String> {
    let get = |p: &str| -> Result<Value, String> {
        let r = api::http().get(format!("{url}{p}")).timeout(Duration::from_secs(3)).send().map_err(|e| api::error_chain(&e))?;
        r.json::<Value>().map_err(|e| e.to_string())
    };
    let models = get("/v1/models")?;
    let alias = models["data"][0]["id"].as_str().or(models["models"][0]["name"].as_str()).unwrap_or("").to_string();
    let props = get("/props").unwrap_or_default();
    let context = props["default_generation_settings"]["n_ctx"].as_u64().unwrap_or(0);
    Ok((alias, context))
}

/// Reads the server's model and context again, when it is running (at
/// start, in the background), keeping what is known when it isn't.
pub fn refresh() {
    let Some(s) = server() else { return };
    if let Ok((alias, context)) = probe(&s.url) {
        let mut st = STATE.lock().unwrap();
        if let Some(cur) = st.as_mut().filter(|c| c.url == s.url) {
            let changed = (!alias.is_empty() && alias != cur.alias) || (context > 0 && context != cur.context);
            if !alias.is_empty() {
                cur.alias = alias;
            }
            if context > 0 {
                cur.context = context;
            }
            if changed {
                save(cur);
            }
        }
    }
}

/// The model's id in OAK ("local/qwen3.8-27b"), when a server was added.
pub fn model_id() -> Option<String> {
    server().map(|s| format!("{PREFIX}{}", s.alias))
}

/// Whether `model` is the local server's.
pub fn serves(model: &str) -> bool {
    model.starts_with(PREFIX)
}

/// "qwen3.8-27b (local)".
pub fn model_name(model: &str) -> String {
    format!("{} (local)", model.strip_prefix(PREFIX).unwrap_or(model))
}

/// Its context window: the server's, as last seen.
pub fn context(model: &str) -> u64 {
    let s = server();
    s.filter(|s| model.strip_prefix(PREFIX) == Some(s.alias.as_str())).map_or(DEFAULT_CONTEXT, |s| s.context.max(4_096))
}

/// The most a reply may write: a quarter of the context at most.
pub fn max_output(model: &str) -> u64 {
    MAX_OUTPUT.min(context(model) / 4)
}

/// Its effort levels: low thinks not at all, high thinks.
pub const EFFORTS: &[&str] = &["low", "high"];

/// Images out of `v` (anywhere in the messages), each a text block.
fn without_images(v: &mut Value) {
    match v {
        Value::Array(a) => {
            for b in a.iter_mut() {
                if b["type"] == "image" {
                    *b = json!({"type": "text", "text": provider::NO_IMAGE});
                } else {
                    without_images(b);
                }
            }
        }
        Value::Object(o) => {
            if let Some(c) = o.get_mut("content") {
                without_images(c);
            }
        }
        _ => {}
    }
}

/// An Oak request as llama-server takes it: a Messages body without
/// thinking settings and effort, for the server's model name; thinking off
/// (through the chat template, the only way it heeds) at low effort and for
/// summaries and titles; images as text.
fn body_for(req: &Request, alias: &str) -> Value {
    let think = req.thinking && req.effort.as_deref() != Some("low") && matches!(req.purpose, oak::Purpose::Main | oak::Purpose::Rewrite);
    let mut body = crate::wire_anthropic::body(req, "local", false);
    if let Some(o) = body.as_object_mut() {
        for k in ["thinking", "output_config"] {
            o.remove(k);
        }
        o.insert("model".into(), json!(alias));
        o.insert("chat_template_kwargs".into(), json!({"enable_thinking": think}));
    }
    without_images(&mut body["messages"]);
    body
}

/// llama-server's refusal of a prompt longer than its context, as the
/// Claude API words it ("prompt is too long: N tokens > M maximum"), which
/// the agent answers by summarizing (`Worker::summarize_now`).
fn too_long(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let long = crate::nodeclub::too_long(v["error"]["message"].as_str().unwrap_or(""), &v)?;
    // The error itself carries the numbers; the message rarely does.
    Some(match (v["error"]["n_prompt_tokens"].as_u64(), v["error"]["n_ctx"].as_u64()) {
        (Some(input), Some(max)) => format!("prompt is too long: {input} tokens > {max} maximum"),
        _ => long,
    })
}

/// Streams one Oak request to the server, sending its events to `tx`. A
/// server that isn't up (yet) is tried again for a while, with `Retry`
/// events saying to start it; the tokens go on the total.
pub fn stream(req: Request, tx: Sender<Sse>) {
    let Some(s) = server() else {
        return drop(tx.send(Sse::Err("No local server was added: Home, + Add account, Local llama.cpp server.".into())));
    };
    let kind = req.purpose.id().to_string();
    let model_id = req.model.clone();
    let alias = model_id.strip_prefix(PREFIX).unwrap_or(&s.alias).to_string();
    let body = body_for(&req, &alias);
    let mut tries: u32 = 0;
    let resp = loop {
        let result = api::http()
            .post(format!("{}/v1/messages", s.url))
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .map_err(|e| format!("Can't reach the local server at {} (start it first): {}", s.url, api::error_chain(&e)));
        tries += 1;
        let retry = match &result {
            Ok(r) => matches!(r.status().as_u16(), 503 | 502),
            Err(_) => true,
        };
        if retry && tries <= api::RETRIES + 2 {
            let reason = match result {
                Ok(r) => format!("the local server is loading ({})", r.status().as_u16()),
                Err(e) => e,
            };
            let wait = api::retry_wait(tries).min(30);
            if tx.send(Sse::Event(oak::Event::Retry { attempt: tries, wait, reason })).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(wait));
            continue;
        }
        match result {
            Ok(r) if r.status().is_success() => break r,
            Ok(r) => {
                let status = r.status().as_u16();
                let text = r.text().unwrap_or_default();
                let msg = too_long(&text).unwrap_or_else(|| {
                    let m = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["message"].as_str().map(str::to_string)).unwrap_or(text);
                    format!("Local server error {status}: {m}")
                });
                return drop(tx.send(Sse::Err(msg)));
            }
            Err(e) => return drop(tx.send(Sse::Err(e))),
        }
    };
    let mut meter = crate::usage::Meter::default();
    api::relay(resp, &tx, "local", |_| vec![], |ev| meter.see(ev));
    if meter.seen {
        crate::usage::record_local(&model_id, kind, &meter);
        let mut st = STATE.lock().unwrap();
        if let Some(cur) = st.as_mut() {
            cur.input += meter.input + meter.write;
            cur.cache_read += meter.read;
            cur.output += meter.out;
            cur.requests += 1;
            save(cur);
        }
    }
}

/// "1.2M", "34.5k", "812": a token count, short.
pub fn tokens_text(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_turn_thinking_off_at_low_and_for_summaries() {
        let messages = vec![json!({"role": "user", "content": [{"type": "image", "media_type": "image/png", "data": "A"},
            {"type": "tool_result", "call_id": "t", "content": [{"type": "image", "media_type": "image/png", "data": "A"}]}]})];
        let req = Request { model: "local/q".into(), thinking: true, effort: Some("low".into()), messages, ..Default::default() };
        let b = body_for(&req, "q");
        assert_eq!(b["model"], "q");
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none() && b.get("context_management").is_none());
        assert_eq!(b["messages"][0]["content"][0]["text"], provider::NO_IMAGE);
        assert_eq!(b["messages"][0]["content"][1]["content"][0]["text"], provider::NO_IMAGE);
        let req = Request { model: "local/q".into(), thinking: true, effort: Some("high".into()), ..Default::default() };
        assert_eq!(body_for(&req, "q")["chat_template_kwargs"]["enable_thinking"], true);
        let req = Request { purpose: oak::Purpose::Summary, ..req };
        assert_eq!(body_for(&req, "q")["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn too_long_reads_like_the_claude_api() {
        let e = r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":40123,"n_ctx":32768}}"#;
        assert_eq!(too_long(e).as_deref(), Some("prompt is too long: 40123 tokens > 32768 maximum"));
        assert_eq!(too_long(r#"{"error":{"message":"other"}}"#), None);
    }
}
