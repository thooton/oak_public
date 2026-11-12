//! NodeClub: an OpenAI-compatible API (chat completions) serving Qwen.
//!
//! This module turns an Oak request into a chat completions request (system
//! message, tool calls and tool messages, images as `image_url` parts, Oak's
//! tools as functions) and the streamed chunks back into Oak events
//! (reasoning from `reasoning_content` or think tags, text, tool calls,
//! usage).
//!
//! The key is kept in nodeclub.json in OAK's folder (Home adds and removes
//! it), or comes from NODECLUB_API_KEY; connected to a vault it is
//! kept there, for every OAK (`provider::Keys`).

use crate::api::{self, Sse};
use crate::oak::{self, Event, Request};
use crate::provider;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

const BASE: &str = "https://api.nodeclub.ai/v1";

/// The API's address: NODECLUB_BASE_URL when set (a proxy, or a test
/// server), else NodeClub's.
fn base() -> String {
    std::env::var("NODECLUB_BASE_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()).unwrap_or_else(|| BASE.into())
}

/// A model OAK offers through NodeClub.
pub struct Model {
    /// The id NodeClub takes, and OAK's id for it.
    pub id: &'static str,
    pub name: &'static str,
    /// Its context window (tokens).
    pub context: u64,
    /// The most a reply may write (less when the prompt leaves less room).
    pub max_output: u64,
    /// The effort levels it tells apart: low thinks not at all, high thinks.
    pub efforts: &'static [&'static str],
}

pub const MODELS: &[Model] = &[Model { id: "qwen3.8-27b", name: "Qwen3.8 27B", context: 128_000, max_output: 32_768, efforts: &["low", "high"] }];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// A model in the picker served through NodeClub.
pub fn serves(model_id: &str) -> bool {
    model(model_id).is_some()
}

// ---------------------------------------------------------------- the key

/// What the account box shows, besides the key.
#[derive(Clone, Default)]
pub(crate) struct Stats {
    /// Why the last check of the key failed.
    error: Option<String>,
    /// The key was checked with NodeClub (and worked) since it was set.
    checked: bool,
    /// Tokens since OAK started: prompts (new, and read from the cache),
    /// replies, and how many requests.
    input: u64,
    cached: u64,
    output: u64,
    requests: u64,
}

pub(crate) static KEYS: LazyLock<provider::Keys<Stats>> = LazyLock::new(|| {
    provider::Keys::new(provider::KeyCfg { name: "NodeClub", kind: api::Provider::NodeClub, file: "nodeclub.json", env: "NODECLUB_API_KEY", vault: "nodeclub" })
});

/// Conversations pinned to a key (its id), by `pin`.
static PINS: LazyLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = LazyLock::new(Default::default);

/// Pins conversation `conv`'s requests to key `id` (`None` unpins): the
/// gateway's callers schedule each key themselves.
pub fn pin(conv: &str, id: Option<&str>) {
    let mut p = PINS.lock().unwrap();
    match id {
        Some(id) => p.insert(conv.to_string(), id.to_string()),
        None => p.remove(conv),
    };
}

/// A NodeClub key is set.
pub fn on() -> bool {
    KEYS.on()
}

/// Checks `key` with NodeClub, then saves it (on the vault when
/// connected to one) beside the others.
pub fn set_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Paste a NodeClub key.".into());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("A key has no spaces in it.".into());
    }
    check(key).map_err(|e| format!("NodeClub refused the key: {e}"))?;
    KEYS.take_remote(key)?;
    KEYS.edit(&provider::key_id(key), |k| (k.extras.error, k.extras.checked) = (None, true));
    Ok(())
}

/// Lists the models with `key`: Ok when NodeClub takes it.
fn check(key: &str) -> Result<Value, String> {
    let resp = api::http()
        .get(format!("{}/models", base()))
        .bearer_auth(key)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(api::network_error)?;
    api::json_or(resp, error_text)
}

/// Checks the keys again, for the account boxes (in the background).
pub fn refresh() {
    for k in KEYS.list() {
        let r = check(&k.key);
        if r.is_ok() {
            KEYS.accepted(&k.id);
        }
        KEYS.edit(&k.id, |k| match r {
            Ok(_) => (k.extras.error, k.extras.checked) = (None, true),
            Err(e) => k.extras.error = Some(e),
        });
    }
}

/// A NodeClub key (or all of them) as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    /// The vault's.
    pub remote: bool,
    /// The key, shortened ("sk-…3f9a").
    pub label: String,
    pub error: Option<String>,
    /// The key was checked and worked.
    pub checked: bool,
    /// Tokens since OAK started: new prompt, read from the cache, written.
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub requests: u64,
}

impl Account {
    pub fn total(&self) -> u64 {
        self.input + self.cached + self.output
    }
}

/// Each key, in order.
pub fn accounts() -> Vec<Account> {
    KEYS.list()
        .into_iter()
        .map(|k| {
            let s = k.extras;
            Account { id: k.id, remote: k.remote, label: provider::short_key(&k.key), error: s.error, checked: s.checked, input: s.input, cached: s.cached, output: s.output, requests: s.requests }
        })
        .collect()
}

/// The key conversation `conv` is on (`crate::rotation`), else all of
/// them together (`account`).
pub fn account_of_conv(conv: &str) -> Option<Account> {
    let id = crate::rotation::seat(api::Provider::NodeClub, conv);
    id.and_then(|id| accounts().into_iter().find(|a| a.id == id)).or_else(account)
}

/// All the keys together (tokens summed; the first error, unless another
/// key works), when one is set; with several, the label says how many.
pub fn account() -> Option<Account> {
    let all = accounts();
    let mut first = all.first()?.clone();
    if all.len() > 1 {
        first.label = format!("{} keys", all.len());
    }
    let sum = |f: fn(&Account) -> u64| all.iter().map(f).sum::<u64>();
    Some(Account {
        error: if all.iter().any(|a| a.error.is_none()) { None } else { all.iter().find_map(|a| a.error.clone()) },
        checked: all.iter().any(|a| a.checked),
        input: sum(|a| a.input),
        cached: sum(|a| a.cached),
        output: sum(|a| a.output),
        requests: sum(|a| a.requests),
        remote: all.iter().any(|a| a.remote),
        ..first
    })
}

// ---------------------------------------------------------------- requests

/// The server turned down `chat_template_kwargs` (thinking stays on), or
/// images: left out of later requests too.
static NO_KWARGS: AtomicBool = AtomicBool::new(false);
static NO_IMAGES: AtomicBool = AtomicBool::new(false);

/// "HTTP 400: message", from an error reply's body; a prompt longer than
/// the context window worded as the Claude API words it ("prompt is too
/// long: N tokens > M maximum"), which the agent answers by summarizing.
fn error_text(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = provider::error_msg(&v, body, v.get("message").and_then(Value::as_str));
    if let Some(long) = too_long(&msg, &v) {
        return long;
    }
    format!("HTTP {status}: {msg}")
}

/// The number right after `pat` in `s`.
fn num_after(s: &str, pat: &str) -> Option<u64> {
    let rest = &s[s.find(pat)? + pat.len()..];
    rest.trim_start().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// The number right before `pat` in `s`.
fn num_before(s: &str, pat: &str) -> Option<u64> {
    let head = s[..s.find(pat)?].trim_end();
    head.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect::<String>().parse().ok()
}

/// The request's size and the window from a context-length error (vLLM's,
/// LiteLLM's and OpenAI's wordings), when they can be read.
fn context_numbers(msg: &str) -> (Option<u64>, Option<u64>) {
    let max = num_after(msg, "maximum context length is").or_else(|| num_after(msg, "context length of"));
    let input = num_before(msg, "in the messages")
        .or_else(|| num_after(msg, "request has"))
        .or_else(|| num_after(msg, "you requested"))
        .or_else(|| num_after(msg, "prompt contains"));
    (input, max)
}

/// With a prompt that fits, the reply limit that does: from an error saying
/// `max_tokens` is too large for what the prompt leaves.
fn room_for_reply(msg: &str) -> Option<u64> {
    let (input, max) = context_numbers(msg);
    let room = max?.checked_sub(input?)?.checked_sub(64)?;
    (room >= 256).then_some(room)
}

/// The reply's size and the window from a body's own say (vLLM's
/// `n_prompt_tokens` and `n_ctx`), when it has them.
fn own_numbers(v: &Value) -> (Option<u64>, Option<u64>) {
    (v["n_prompt_tokens"].as_u64(), v["n_ctx"].as_u64())
}

/// The message of an error about the prompt not fitting, as the agent
/// expects it; None for other errors and for a reply limit that doesn't
/// fit beside a prompt that does (`room_for_reply`).
pub(crate) fn too_long(msg: &str, v: &Value) -> Option<String> {
    let lower = msg.to_lowercase();
    let about = ["context_length_exceeded", "exceed_context_size_error"]
        .iter()
        .any(|c| v["error"]["code"].as_str() == Some(c) || v["error"]["type"].as_str() == Some(c))
        || ["maximum context length", "contextwindowexceeded", "context window", "prompt is too long", "exceeds the available context size"]
            .iter()
            .any(|w| lower.contains(w));
    if !about {
        return None;
    }
    let (input, max) = match own_numbers(v) {
        (None, None) => context_numbers(msg),
        own => own,
    };
    match (input, max) {
        // The prompt fits: only the reply limit needs to come down.
        (Some(input), Some(max)) if input < max && room_for_reply(msg).is_some() => None,
        (Some(input), Some(max)) if input >= max => Some(format!("prompt is too long: {input} tokens > {max} maximum")),
        (Some(input), Some(max)) => Some(format!("prompt is too long: {input} tokens leave no room for a reply in the {max}-token window")),
        _ => Some(format!("prompt is too long: {msg}")),
    }
}

/// A user block as a chat content part (None for one that has no place).
fn part(b: &Value, images: bool) -> Option<Value> {
    match b["type"].as_str()? {
        "text" => b["text"].as_str().filter(|t| !t.is_empty()).map(|t| json!({"type": "text", "text": t})),
        "image" if images => Some(json!({"type": "image_url", "image_url": {"url": provider::data_url(b)?}})),
        "image" => Some(json!({"type": "text", "text": provider::NO_IMAGE})),
        "document" => {
            let title = b["title"].as_str().unwrap_or("document");
            Some(if b["media_type"] == "text/plain" {
                json!({"type": "text", "text": format!("{title}:\n{}", b["data"].as_str().unwrap_or(""))})
            } else {
                json!({"type": "text", "text": format!("[{title} was attached; this model can't read it.]")})
            })
        }
        _ => None,
    }
}

/// Parts as a message's content: a plain string when they are all text
/// (what every server takes), else the parts.
fn content_of(parts: Vec<Value>) -> Value {
    if parts.iter().all(|p| p["type"] == "text") {
        Value::String(parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n"))
    } else {
        Value::Array(parts)
    }
}

/// Oak's messages as chat-completions messages, the system prompt first.
/// A user turn's tool results become tool messages (in order, right after
/// the assistant's calls); their images, which a tool message can't carry,
/// and the turn's own text and images follow in a user message.
fn chat_messages(req: &Request, images: bool) -> Vec<Value> {
    let mut out = vec![];
    if !req.system.is_empty() {
        out.push(json!({"role": "system", "content": req.system}));
    }
    for m in &req.messages_with_instruction() {
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        if m["role"] == "assistant" {
            let text = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n\n");
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_call")
                .map(|b| {
                    let args = if b["input"].is_null() { "{}".to_string() } else { b["input"].to_string() };
                    json!({"id": b["id"], "type": "function", "function": {"name": b["name"], "arguments": args}})
                })
                .collect();
            let mut msg = json!({"role": "assistant", "content": text});
            if !calls.is_empty() {
                if text.is_empty() {
                    msg["content"] = Value::Null;
                }
                msg["tool_calls"] = Value::Array(calls);
            }
            out.push(msg);
            continue;
        }
        let mut parts = vec![];
        for b in &blocks {
            if b["type"] != "tool_result" {
                parts.extend(part(b, images));
                continue;
            }
            let mut text = vec![];
            match &b["content"] {
                Value::String(s) => text.push(s.clone()),
                Value::Array(items) => {
                    for i in items {
                        match part(i, images) {
                            Some(p) if p["type"] == "text" => text.push(p["text"].as_str().unwrap_or("").to_string()),
                            Some(p) => parts.push(p),
                            None => {}
                        }
                    }
                }
                _ => {}
            }
            let mut content = text.join("\n");
            if b["is_error"] == true {
                content = format!("Error: {content}");
            }
            out.push(json!({"role": "tool", "tool_call_id": b["call_id"], "content": content}));
        }
        if !parts.is_empty() {
            out.push(json!({"role": "user", "content": content_of(parts)}));
        }
    }
    out
}

/// Oak's tool definitions as functions.
pub(crate) fn chat_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|t| t["name"].is_string() && t["input_schema"].is_object())
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["input_schema"]}}))
        .collect()
}

/// Whether the reply should think: not at low effort, not for summaries
/// and titles, and not when the request turns thinking off.
fn thinks(req: &Request) -> bool {
    req.effort.as_deref() != Some("low") && matches!(req.purpose, oak::Purpose::Main | oak::Purpose::Rewrite) && req.thinking
}

/// Tokens a request takes, about: its text (images not counted by their
/// bytes, but as about a thousand tokens each).
fn estimate(req: &Value) -> u64 {
    fn walk(v: &Value, chars: &mut usize, images: &mut u64) {
        match v {
            Value::String(s) => *chars += s.len(),
            Value::Array(a) => a.iter().for_each(|x| walk(x, chars, images)),
            Value::Object(o) => {
                for (k, x) in o {
                    if k == "image_url" {
                        *images += 1;
                    } else {
                        walk(x, chars, images);
                    }
                }
            }
            _ => *chars += 4,
        }
    }
    let (mut chars, mut images) = (0, 0);
    walk(req, &mut chars, &mut images);
    crate::rolling::estimate(chars) + images * 1_000
}

/// The chat completions request for an Oak request to model `m`.
fn chat_body(r: &Request, m: &Model) -> Value {
    let images = !NO_IMAGES.load(Ordering::Relaxed);
    let mut req = json!({
        "model": m.id,
        "messages": chat_messages(r, images),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let tools = chat_tools(&r.tools);
    if !tools.is_empty() {
        req["tools"] = Value::Array(tools);
        req["tool_choice"] = json!(if r.tools_off { "none" } else { "auto" });
    }
    if !thinks(r) && !NO_KWARGS.load(Ordering::Relaxed) {
        req["chat_template_kwargs"] = json!({"enable_thinking": false});
    }
    // The reply's limit: what was asked for (the model's most), within what
    // the prompt leaves of the window (servers refuse a request whose
    // prompt and limit together pass it).
    let asked = if r.max_tokens == 0 { m.max_output } else { r.max_tokens.min(m.max_output) };
    let room = m.context.saturating_sub(estimate(&req) * 11 / 10 + 512);
    req["max_tokens"] = json!(asked.min(room).max(1_024.min(asked)));
    req
}

/// Streams one Oak request through NodeClub, sending its events to `tx`:
/// failed requests are retried (with `Retry` events), and the tokens go on
/// the total.
pub fn stream(body: Request, tx: Sender<Sse>) {
    let id = body.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err(format!("NodeClub doesn't serve {id}."))));
    };
    let conv = body.seat_conv();
    // NodeClub is free: the keys weigh the same, and conversations spread
    // over them. A conversation pinned to a key (the gateway's callers keep
    // each key under its own limits) goes through that one.
    let pinned = PINS.lock().unwrap().get(&conv).cloned().and_then(|id| KEYS.list().into_iter().find(|k| k.id == id));
    let mut key = match pinned {
        Some(k) => k,
        None => match KEYS.pick(&conv, |_| 100.0) {
            Ok(k) => k,
            Err(e) => return drop(tx.send(Sse::Err(e))),
        },
    };
    let mut req = chat_body(&body, m);
    let resp = loop {
        match open(&body, m, &mut req, &key.key, &tx) {
            Ok(r) => break r,
            Err(None) => return,
            // A rejected key: another takes the conversation, if one can.
            Err(Some(s)) => match KEYS.pick(&conv, |_| 100.0) {
                Ok(next) if next.id != key.id && KEYS.list().iter().any(|k| k.id == key.id && k.rejected.is_some()) => {
                    key = next;
                    if tx.send(Sse::Rotated(provider::short_key(&key.key))).is_err() {
                        return;
                    }
                }
                _ => return drop(tx.send(s)),
            },
        }
    };
    let mut t = Translator::new(&id);
    for ev in provider::events(resp) {
        let chunk = match ev {
            Ok(Some(c)) => c,
            Ok(None) => {
                t.done = true;
                break;
            }
            Err(e) => return drop(tx.send(Sse::Err(e))),
        };
        match t.feed(&chunk) {
            Ok(events) => {
                for e in events {
                    if tx.send(Sse::Event(e)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => return drop(tx.send(Sse::Err(e))),
        }
    }
    match t.finish() {
        Ok(events) => {
            for e in events {
                if tx.send(Sse::Event(e)).is_err() {
                    return;
                }
            }
            KEYS.edit(&key.id, |k| {
                let s = &mut k.extras;
                s.input += t.usage.0.saturating_sub(t.usage.1);
                s.cached += t.usage.1;
                s.output += t.usage.2;
                s.requests += 1;
            });
            let _ = tx.send(Sse::Done);
        }
        Err(e) => drop(tx.send(Sse::Err(e))),
    }
}

/// Sends `req` until a response comes that can be read as a stream: network
/// failures, rate limits and server errors retried, and a request the
/// server turned down for what it can't take (thinking switch, images, a
/// reply limit past the window) sent again without it. Err carries what
/// the receiver gets, or None when it is gone.
fn open(body: &Request, m: &Model, req: &mut Value, key: &str, tx: &Sender<Sse>) -> Result<reqwest::blocking::Response, Option<Sse>> {
    let mut tries: u32 = 0;
    let mut fixes: u32 = 0;
    loop {
        let build = || {
            api::http()
                .post(format!("{}/chat/completions", base()))
                .bearer_auth(key)
                .header("Accept", "text/event-stream")
                .json(&*req)
                .timeout(Duration::from_secs(900))
        };
        let why = |r: reqwest::blocking::Response| format!("NodeClub {}", error_text(r.status().as_u16(), &r.text().unwrap_or_default()));
        let r = match provider::send_retrying(tx, &mut tries, build, why).map_err(|()| None)? {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => r,
            Err(e) => return Err(Some(Sse::Err(e))),
        };
        let status = r.status().as_u16();
        let text = r.text().unwrap_or_default();
        let lower = text.to_lowercase();
        if status == 401 || status == 403 {
            let msg = error_text(status, &text);
            KEYS.edit(&provider::key_id(key), |k| k.extras.error = Some(format!("NodeClub rejected this key ({msg}): remove it")));
            KEYS.reject(&provider::key_id(key), &msg);
            return Err(Some(Sse::Err(format!("NodeClub rejected key {} ({msg}): remove it on Home, or add one that works.", provider::short_key(key)))));
        }
        if (400..500).contains(&status) && fixes < 3 {
            fixes += 1;
            if req.get("chat_template_kwargs").is_some() && lower.contains("chat_template_kwargs") {
                NO_KWARGS.store(true, Ordering::Relaxed);
                req.as_object_mut().unwrap().remove("chat_template_kwargs");
                continue;
            }
            if req.to_string().contains("\"image_url\"") && ["image", "vision", "multimodal", "multi-modal"].iter().any(|w| lower.contains(w)) {
                NO_IMAGES.store(true, Ordering::Relaxed);
                let max_tokens = req["max_tokens"].clone();
                *req = chat_body(body, m);
                req["max_tokens"] = max_tokens;
                continue;
            }
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let msg = v["error"]["message"].as_str().or(v["detail"].as_str()).unwrap_or(&text).to_string();
            if (lower.contains("max_tokens") || lower.contains("max_completion_tokens") || lower.contains("maximum context length"))
                && let Some(room) = room_for_reply(&msg).filter(|r| Some(*r) < req["max_tokens"].as_u64())
            {
                req["max_tokens"] = json!(room);
                continue;
            }
        }
        return Err(Some(Sse::Err(format!("NodeClub {}", error_text(status, &text)))));
    }
}

/// A tool call so far.
struct Tool {
    /// The call's index in the stream, its id, its block's index, whether
    /// any arguments came, and whether its block is still open.
    at: u64,
    id: String,
    index: usize,
    args: bool,
    open: bool,
}

/// Turns chat completions chunks into Oak events: a start, then blocks
/// (reasoning, text, tool calls) opened and closed as the reply goes from
/// one to the next, and at the end its usage and stop reason.
struct Translator {
    model: String,
    /// The provider whose reasoning it is.
    by: &'static str,
    started: bool,
    /// The open thinking and text blocks (their index).
    think: Option<usize>,
    text: Option<usize>,
    /// The calls so far: one at a time is open, as blocks follow one
    /// another.
    tools: Vec<Tool>,
    /// The next block's index.
    next: usize,
    /// Inside ` thinking` … `</think>` in the content (a server that doesn't
    /// split reasoning out).
    in_tag: bool,
    /// Content came that wasn't thinking.
    wrote: bool,
    /// Calls were made (the stop reason is tool_calls).
    called: bool,
    finish: Option<String>,
    /// Prompt tokens (all), of them read from the cache, and written.
    usage: (u64, u64, u64),
    /// The server's count of thinking tokens, if it gave one.
    reasoning: Option<u64>,
    count: ThinkCount,
    /// The stream said [DONE].
    done: bool,
}

impl Translator {
    fn new(model: &str) -> Translator {
        Translator {
            model: model.into(),
            by: "nodeclub",
            started: false,
            think: None,
            text: None,
            tools: vec![],
            next: 0,
            in_tag: false,
            wrote: false,
            called: false,
            finish: None,
            usage: (0, 0, 0),
            reasoning: None,
            count: ThinkCount::default(),
            done: false,
        }
    }

    fn index(&mut self) -> usize {
        self.next += 1;
        self.next - 1
    }

    fn close_think(&mut self, out: &mut Vec<Event>) {
        if let Some(i) = self.think.take() {
            out.push(Event::BlockEnd { index: i });
        }
    }

    fn close_text(&mut self, out: &mut Vec<Event>) {
        if let Some(i) = self.text.take() {
            out.push(Event::BlockEnd { index: i });
        }
    }

    fn close_tools(&mut self, out: &mut Vec<Event>) {
        for t in self.tools.iter_mut().filter(|t| t.open) {
            if !t.args {
                out.push(Event::Input { index: t.index, json: "{}".into() });
            }
            out.push(Event::BlockEnd { index: t.index });
            t.open = false;
        }
    }

    fn close_all(&mut self, out: &mut Vec<Event>) {
        self.close_think(out);
        self.close_text(out);
        self.close_tools(out);
    }

    fn thinking(&mut self, t: &str, out: &mut Vec<Event>) {
        if t.is_empty() {
            return;
        }
        self.close_text(out);
        self.close_tools(out);
        self.count.thought(t);
        let i = match self.think {
            Some(i) => i,
            None => {
                let i = self.index();
                self.think = Some(i);
                out.push(Event::Block { index: i, block: json!({"type": "reasoning", "text": "", "by": self.by}) });
                i
            }
        };
        out.push(Event::Reasoning { index: i, text: t.into() });
    }

    fn writing(&mut self, t: &str, out: &mut Vec<Event>) {
        // Whitespace between thinking and the reply is no reply.
        if t.is_empty() || (self.text.is_none() && t.trim().is_empty()) {
            return;
        }
        self.close_think(out);
        self.close_tools(out);
        let fresh = self.text.is_none();
        if fresh {
            let i = self.index();
            self.text = Some(i);
            out.push(Event::Block { index: i, block: oak::text("") });
        }
        self.wrote = true;
        self.count.wrote(t);
        out.push(Event::Text { index: self.text.unwrap(), text: if fresh { t.trim_start() } else { t }.into() });
    }

    /// Content, with `` tags split off as thinking.
    fn content(&mut self, mut t: &str, out: &mut Vec<Event>) {
        if !self.wrote && !self.in_tag && self.think.is_none() && t.trim_start().starts_with("\u{3c}think\u{3e}") {
            self.in_tag = true;
            t = t.trim_start().strip_prefix("\u{3c}think\u{3e}").unwrap_or(t);
        }
        if self.in_tag {
            match t.split_once("</think>") {
                Some((thought, rest)) => {
                    self.thinking(thought, out);
                    self.in_tag = false;
                    self.writing(rest, out);
                }
                None => self.thinking(t, out),
            }
            return;
        }
        self.writing(t, out);
    }

    fn feed(&mut self, chunk: &Value) -> Result<Vec<Event>, String> {
        let mut out = vec![];
        if let Some(e) = chunk.get("error").filter(|e| !e.is_null()) {
            let msg = e["message"].as_str().or(e.as_str()).unwrap_or("unknown error");
            return Err(too_long(msg, chunk).unwrap_or_else(|| format!("NodeClub error: {msg}")));
        }
        if !self.started {
            self.started = true;
            out.push(Event::Start { id: chunk["id"].as_str().unwrap_or("").into(), model: self.model.clone() });
        }
        let u = &chunk["usage"];
        if u.is_object() {
            let input = u["prompt_tokens"].as_u64().unwrap_or(0);
            let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(input);
            self.usage = (input, cached, u["completion_tokens"].as_u64().unwrap_or(0));
            self.reasoning = reported_thinking(u);
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let d = &choice["delta"];
            if let Some(t) = d["reasoning_content"].as_str().or(d["reasoning"].as_str()) {
                self.thinking(t, &mut out);
            }
            if let Some(t) = d["content"].as_str() {
                self.content(t, &mut out);
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let at = tc["index"].as_u64().unwrap_or(0);
                let id = tc["id"].as_str().unwrap_or("");
                let name = tc["function"]["name"].as_str().unwrap_or("");
                let args = match &tc["function"]["arguments"] {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    v => v.to_string(), // a server that sends them parsed
                };
                // A new call: a new index, or (servers that number every
                // call 0) a new id.
                let known = self.tools.iter().position(|t| t.at == at && (id.is_empty() || t.id == id));
                let pos = match known {
                    // More of a call whose block was closed (a server
                    // interleaving parallel calls): it can't go on.
                    Some(p) if !self.tools[p].open => continue,
                    Some(p) => p,
                    None => {
                        self.close_all(&mut out);
                        let index = self.index();
                        let call_id = if id.is_empty() { format!("call_{}_{at}", self.next) } else { id.to_string() };
                        out.push(Event::Block { index, block: json!({"type": "tool_call", "id": call_id, "name": name, "input": {}}) });
                        self.tools.push(Tool { at, id: call_id, index, args: false, open: true });
                        self.called = true;
                        self.tools.len() - 1
                    }
                };
                self.count.wrote(name);
                if !args.is_empty() {
                    self.count.wrote(&args);
                    self.tools[pos].args = true;
                    out.push(Event::Input { index: self.tools[pos].index, json: args });
                }
            }
            if let Some(f) = choice["finish_reason"].as_str() {
                self.finish = Some(f.to_string());
            }
        }
        Ok(out)
    }

    /// The stream ended: the blocks closed, the usage and stop reason. Err
    /// when it ended before the reply did.
    fn finish(&mut self) -> Result<Vec<Event>, String> {
        if !self.started {
            return Err("NodeClub's stream ended before the reply began".into());
        }
        if self.finish.is_none() && !self.done {
            return Err("Stream ended before the reply was complete".into());
        }
        let mut out = vec![];
        self.close_all(&mut out);
        let stop = match self.finish.as_deref() {
            Some("length") => oak::stop::MAX_TOKENS,
            Some("content_filter") => oak::stop::REFUSAL,
            _ if self.called => oak::stop::TOOL_CALLS,
            _ => oak::stop::END,
        };
        let (input, cached, output) = self.usage;
        let thought = self.count.tokens(self.reasoning, output);
        out.push(Event::Usage(oak::Usage { input: input - cached, cache_read: cached, output, reasoning: (thought > 0).then_some(thought), ..Default::default() }));
        out.push(Event::Stop { reason: stop.into(), detail: None });
        Ok(out)
    }
}

/// How much of a chat completions reply was thinking, for "Thought for N
/// tokens" (Freebuff's and NodeClub's streams both count it).
#[derive(Default)]
pub(crate) struct ThinkCount {
    think_chars: u64,
    other_chars: u64,
    chunks: u64,
}

impl ThinkCount {
    pub(crate) fn thought(&mut self, t: &str) {
        self.think_chars += t.chars().count() as u64;
        self.chunks += 1;
    }

    pub(crate) fn wrote(&mut self, t: &str) {
        self.other_chars += t.chars().count() as u64;
    }

    /// The thinking tokens: the server's count where it gives one, else the
    /// reply's output tokens shared out by characters (thinking against text
    /// and tool calls), else one per streamed chunk of thinking. 0 when it
    /// didn't think.
    pub(crate) fn tokens(&self, reported: Option<u64>, output: u64) -> u64 {
        if self.think_chars == 0 {
            return 0;
        }
        if let Some(n) = reported.filter(|&n| n > 0) {
            return n;
        }
        if output > 0 {
            return (output * self.think_chars / (self.think_chars + self.other_chars)).max(1);
        }
        self.chunks
    }
}

/// The server's own count of a reply's thinking tokens, in a chat
/// completions usage.
pub(crate) fn reported_thinking(u: &Value) -> Option<u64> {
    u["completion_tokens_details"]["reasoning_tokens"].as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Request {
        Request {
            provider: crate::providers::Kind::NodeClub,
            model: "qwen3.8-27b".into(),
            max_tokens: 32_000,
            system: "You are Oak.".into(),
            tools: vec![json!({"name": "READ", "description": "Read a file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}})],
            thinking: true,
            effort: Some("high".into()),
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "media_type": "image/png", "data": "AAA"}]}),
                json!({"role": "assistant", "content": [{"type": "reasoning", "text": "hm", "by": "nodeclub"}, {"type": "tool_call", "id": "c1", "name": "READ", "input": {"path": "a"}},
                    {"type": "tool_call", "id": "c2", "name": "SCREENSHOT", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "file a"},
                    {"type": "tool_result", "call_id": "c2", "is_error": false, "content": [{"type": "text", "text": "shot"}, {"type": "image", "media_type": "image/jpeg", "data": "B"}]},
                    {"type": "text", "text": "and?"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "Done."}]}),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn request_shape() {
        let req = chat_body(&body(), &MODELS[0]);
        let m = req["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "You are Oak."}));
        assert_eq!(m[1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAA");
        // Thinking is left out; the calls go with no content.
        assert!(m[2]["content"].is_null());
        assert_eq!(m[2]["tool_calls"][0]["function"], json!({"name": "READ", "arguments": "{\"path\":\"a\"}"}));
        assert_eq!(m[2]["tool_calls"][1]["function"]["arguments"], "{}");
        // Tool messages right after, then the image and the user's text.
        assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "c1", "content": "file a"}));
        assert_eq!(m[4], json!({"role": "tool", "tool_call_id": "c2", "content": "shot"}));
        assert_eq!(m[5]["content"][0]["image_url"]["url"], "data:image/jpeg;base64,B");
        assert_eq!(m[5]["content"][1]["text"], "and?");
        assert_eq!(m[6], json!({"role": "assistant", "content": "Done."}));
        assert_eq!(req["tools"][0]["function"]["parameters"], json!({"type": "object", "properties": {"path": {"type": "string"}}}));
        assert_eq!(req["max_tokens"], 32_000);
        assert_eq!(req["stream_options"]["include_usage"], true);
        // Low effort, titles and summaries: thinking off.
        let mut b = body();
        b.effort = Some("low".into());
        assert_eq!(chat_body(&b, &MODELS[0])["chat_template_kwargs"]["enable_thinking"], false);
        // Images as text when the server takes none.
        let m = chat_messages(&body(), false);
        assert_eq!(m[1]["content"], format!("look\n\n{}", provider::NO_IMAGE));
    }

    fn run(chunks: &[Value]) -> Vec<Event> {
        let mut t = Translator::new("qwen3.8-27b");
        let mut out = vec![];
        for c in chunks {
            out.extend(t.feed(c).unwrap());
        }
        t.done = true;
        out.extend(t.finish().unwrap());
        out
    }

    fn delta(d: Value) -> Value {
        json!({"id": "x", "choices": [{"index": 0, "delta": d, "finish_reason": null}]})
    }

    fn usage(out: &[Event]) -> oak::Usage {
        out.iter().find_map(|e| if let Event::Usage(u) = e { Some(*u) } else { None }).unwrap()
    }

    fn stop(out: &[Event]) -> String {
        out.iter().find_map(|e| if let Event::Stop { reason, .. } = e { Some(reason.clone()) } else { None }).unwrap()
    }

    #[test]
    fn stream_translation() {
        let out = run(&[
            delta(json!({"role": "assistant", "reasoning_content": "Let me "})),
            delta(json!({"reasoning_content": "look."})),
            delta(json!({"content": "\n\n"})),
            delta(json!({"content": "Reading it."})),
            delta(json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function", "function": {"name": "READ", "arguments": ""}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"a\"}"}}]})),
            delta(json!({"tool_calls": [{"index": 1, "id": "call_b", "function": {"name": "TASK", "arguments": "{}"}}]})),
            json!({"id": "x", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"id": "x", "choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 50, "prompt_tokens_details": {"cached_tokens": 800}}}),
        ]);
        assert_eq!(out.len(), 17);
        assert!(matches!(&out[0], Event::Start { .. }));
        assert_eq!(out[1], Event::Block { index: 0, block: json!({"type": "reasoning", "text": "", "by": "nodeclub"}) });
        assert_eq!(out[6], Event::Text { index: 1, text: "Reading it.".into() });
        assert_eq!(out[8], Event::Block { index: 2, block: json!({"type": "tool_call", "id": "call_a", "name": "READ", "input": {}}) });
        let args: String = out.iter().filter_map(|e| if let Event::Input { index: 2, json } = e { Some(json.as_str()) } else { None }).collect();
        assert_eq!(args, "{\"path\":\"a\"}");
        assert!(matches!(&out[12], Event::Block { index: 3, block } if block["name"] == "TASK"));
        assert_eq!(stop(&out), "tool_calls");
        // No count from the server: the 50 shared out by characters, 12
        // thinking against 33 of text, tool names and arguments.
        assert_eq!(usage(&out), oak::Usage { input: 200, cache_read: 800, output: 50, reasoning: Some(13), ..Default::default() });
        // The server's count wins; no thinking, no count.
        let out = run(&[delta(json!({"reasoning_content": "hm"})), delta(json!({"content": "Hi"})),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 40, "completion_tokens_details": {"reasoning_tokens": 31}}})]);
        assert_eq!(usage(&out).reasoning, Some(31));
        let out = run(&[delta(json!({"content": "Hi"})), json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 4}})]);
        assert_eq!(usage(&out).reasoning, None);
        assert_eq!(stop(&out), "end");
        // Reasoning inside the content, in tags.
        let out = run(&[delta(json!({"content": "\u{3c}think\u{3e}"})), delta(json!({"content": "hmm"})), delta(json!({"content": "\u{3c}/think\u{3e}\n\nHello"})),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})]);
        assert!(matches!(&out[1], Event::Block { block, .. } if block["type"] == "reasoning"));
        assert_eq!(out[2], Event::Reasoning { index: 0, text: "hmm".into() });
        assert!(matches!(&out[4], Event::Block { block, .. } if block["type"] == "text"));
        assert_eq!(out[5], Event::Text { index: 1, text: "Hello".into() });
        // Every call numbered 0, told apart by id; a call without arguments.
        let out = run(&[
            delta(json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "READ", "arguments": "{\"path\":\"x\"}"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "id": "b", "function": {"name": "NOTE"}}]})),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]);
        assert_eq!(out.iter().filter(|e| matches!(e, Event::Block { .. })).count(), 2);
        assert!(out.contains(&Event::Input { index: 1, json: "{}".into() }));
        assert_eq!(stop(&out), "tool_calls");
        // Cut off.
        let mut t = Translator::new("m");
        t.feed(&delta(json!({"content": "Hel"}))).unwrap();
        assert!(t.finish().is_err());
    }
}
