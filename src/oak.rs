//! The Oak format: how Oak itself keeps a conversation and asks for a reply,
//! whatever provider serves it. Conversations are stored in it (format 2),
//! the agent builds its requests in it (`Request`), and every provider turns
//! a `Request` into its own wire format and its stream back into `Event`s.
//! Nothing outside a provider's module knows a wire format.
//!
//! A message is `{"role": "user" | "assistant", "content": [block, ...]}`.
//! Blocks, by `type`:
//!
//! - `text`: `{text}`
//! - `image`: `{media_type, data}`, the data in base64; in a stored
//!   conversation, `{media_type, blob, _oak_size}` instead: the file in
//!   Oak's `images` folder that holds it (`crate::blobs`), put back as
//!   data when a request goes out (`api::stream`)
//! - `document`: `{media_type, data, title}`: a PDF in base64, or plain text
//!   (`text/plain`) as it is
//! - `reasoning`: `{text, sig?, by?, redacted?}`: what the model thought. `by`
//!   is the provider that wrote it (`Kind::id`); `sig` is that provider's own
//!   proof or state for it (Claude's signature, Codex's encrypted reasoning
//!   item), which only `by` can read. A redacted one has only its `sig`.
//! - `tool_call`: `{id, name, input}`
//! - `tool_result`: `{call_id, content, is_error?}`, the content a string or
//!   `text` and `image` blocks
//!
//! Oak's own keys on messages and blocks start with `_oak_` (when a block
//! came in, how long a thought took…); they never leave Oak (`clean`).

use serde_json::{Value, json};

/// The stored format this module describes (`Conversation::format`). 0 is
/// the Anthropic Messages format conversations were kept in before; 1 is
/// this one, without the conversation's provider.
pub const FORMAT: u32 = 2;

// ---------------------------------------------------------------- blocks

pub fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

pub fn image(media_type: &str, data: &str) -> Value {
    json!({"type": "image", "media_type": media_type, "data": data})
}

pub fn tool_result(call_id: &Value, content: Value, is_error: bool) -> Value {
    json!({"type": "tool_result", "call_id": call_id, "content": content, "is_error": is_error})
}

pub fn is(b: &Value, kind: &str) -> bool {
    b["type"] == kind
}

pub fn is_reasoning(b: &Value) -> bool {
    is(b, "reasoning")
}

pub fn is_call(b: &Value) -> bool {
    is(b, "tool_call")
}

pub fn is_result(b: &Value) -> bool {
    is(b, "tool_result")
}

/// `blocks` without Oak's own keys, on the blocks and inside tool results.
pub fn clean_blocks(blocks: &mut [Value]) {
    for b in blocks.iter_mut() {
        if let Some(o) = b.as_object_mut() {
            o.retain(|k, _| !k.starts_with("_oak_"));
            if let Some(Value::Array(inner)) = o.get_mut("content") {
                clean_blocks(inner);
            }
        }
    }
}

/// Whether `id` is a tool call id the strictest APIs take: 1 to `max`
/// characters of `A-Z a-z 0-9 _ -` (Claude's pattern).
fn fit_id(id: &str, max: usize) -> bool {
    !id.is_empty() && id.len() <= max && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `id` made to fit (`fit_id`): other characters as `_`, clipped.
fn fitted_id(id: &str, max: usize) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(max).collect();
    if s.is_empty() { "call".into() } else { s }
}

/// `messages` with tool call ids an API that checks them takes, whichever
/// provider's model made the calls (a conversation moves between them): each
/// fits Claude's pattern (`fit_id`, at most `max` characters) and no two calls
/// share one (some servers number every reply's calls from 0, and a plugin's
/// may give none). A result goes with the newest call before it of its id.
/// Ids that are fine stay as they are, and what one becomes depends only on
/// the messages before it, so requests still only grow (`crate::prefix`).
/// Borrowed when nothing changes.
pub fn fit_call_ids(messages: &[Value], max: usize) -> std::borrow::Cow<'_, [Value]> {
    use std::collections::{HashMap, HashSet};
    let blocks = || messages.iter().flat_map(|m| m["content"].as_array().into_iter().flatten());
    let mut seen = HashSet::new();
    let fine = blocks().all(|b| match b["type"].as_str() {
        Some("tool_call") => b["id"].as_str().is_some_and(|id| fit_id(id, max) && seen.insert(id)),
        Some("tool_result") => b["call_id"].as_str().is_some_and(|id| fit_id(id, max)),
        _ => true,
    });
    if fine {
        return std::borrow::Cow::Borrowed(messages);
    }
    let mut used: HashSet<String> = HashSet::new();
    let mut now: HashMap<String, String> = HashMap::new();
    let mut out = messages.to_vec();
    for b in out.iter_mut().flat_map(|m| m["content"].as_array_mut().into_iter().flatten()) {
        match b["type"].as_str() {
            Some("tool_call") => {
                let was = match &b["id"] {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                let base = fitted_id(&was, max);
                let mut id = base.clone();
                let mut k = 2;
                while used.contains(&id) {
                    let tail = format!("_{k}");
                    let head: String = base.chars().take(max.saturating_sub(tail.len())).collect();
                    id = format!("{head}{tail}");
                    k += 1;
                }
                used.insert(id.clone());
                now.insert(was, id.clone());
                b["id"] = json!(id);
            }
            Some("tool_result") => {
                let was = match &b["call_id"] {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                let id = now.get(&was).cloned().unwrap_or_else(|| fitted_id(&was, max));
                b["call_id"] = json!(id);
            }
            _ => {}
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Messages without Oak's own keys.
pub fn clean(mut messages: Vec<Value>) -> Vec<Value> {
    for m in messages.iter_mut() {
        if let Some(o) = m.as_object_mut() {
            o.retain(|k, _| !k.starts_with("_oak_"));
        }
        if let Some(c) = m["content"].as_array_mut() {
            clean_blocks(c);
        }
    }
    messages
}

// ---------------------------------------------------------------- requests

/// What a request is for, for the usage log and the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Purpose {
    #[default]
    Main,
    Summary,
    Title,
    /// A subagent's prompt, rewritten.
    Rewrite,
    /// A plugin's request (`model_stream`), by the name it gives it.
    Plugin(&'static str),
}

impl Purpose {
    /// Its name in the usage log.
    pub fn id(self) -> &'static str {
        match self {
            Purpose::Main => "main",
            Purpose::Summary => "summary",
            Purpose::Title => "title",
            Purpose::Rewrite => "agent prompt rewrite",
            Purpose::Plugin(name) => name,
        }
    }
}

/// How long the cache keeps what a request writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ttl {
    /// No cache: a one-off request (a title) would only pay for the write.
    Off,
    #[default]
    FiveMinutes,
    OneHour,
}

impl Ttl {
    /// How long the cache lives after a request (ms).
    pub fn ms(self) -> i64 {
        match self {
            Ttl::Off => 0,
            Ttl::FiveMinutes => 5 * 60_000,
            Ttl::OneHour => 60 * 60_000,
        }
    }
}

/// A request for a reply, as Oak asks for one.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub provider: crate::providers::Kind,
    pub model: String,
    pub max_tokens: u64,
    pub system: String,
    /// Oak messages, Oak's own keys taken out.
    pub messages: Vec<Value>,
    /// `{name, description, input_schema}`.
    pub tools: Vec<Value>,
    /// The model may not call tools (a summary asked again).
    pub tools_off: bool,
    /// Thinking before it answers (adaptive, where the model decides).
    pub thinking: bool,
    /// "low" … "max", where the model has effort levels.
    pub effort: Option<String>,
    pub purpose: Purpose,
    pub ttl: Ttl,
    /// With a five-minute `ttl`: the request message whose end the last
    /// one-hour entry is, given a one-hour breakpoint there so it stays warm
    /// (`cache_ttl`).
    pub anchor: Option<usize>,
    /// Joins the last user turn after its cache breakpoint, so the request
    /// reads exactly what the one it copies wrote (a summary instruction).
    pub instruction: Option<String>,
    /// Models to fall back on, in order (OpenRouter's title models).
    pub fallbacks: Vec<String>,
    /// The upstream that served the conversation last (OpenRouter), kept
    /// while it is a good pick for its cache.
    pub route: Option<String>,
    /// The working folder (Freebuff's sidecar signs the project).
    pub folder: Option<String>,
    /// The conversation it is for: its account stays the same (`rotation`).
    pub conv: Option<String>,
    /// For a provider that keeps the conversation itself (Quick): the part
    /// of `messages` it hasn't seen (other models wrote it), which it is
    /// sent besides the newest user turn (`agent::unseen_by`).
    pub unseen: Vec<Value>,
    pub quick_session: Option<crate::quick::recovery::Session>,
    /// The session's scratchpad (Quick): where the whole of `unseen` is
    /// written when the message carries it cut short (`quick::so_far`).
    pub scratch: Option<String>,
}

impl Request {
    /// The conversation whose account it goes on (`rotation`): none for a
    /// title, which takes whichever account has room.
    pub fn seat_conv(&self) -> String {
        if self.purpose == Purpose::Title { String::new() } else { self.conv.clone().unwrap_or_default() }
    }

    /// The messages with `instruction` joined to the end, for providers
    /// without cache breakpoints.
    pub fn messages_with_instruction(&self) -> Vec<Value> {
        let mut m = self.messages.clone();
        if let Some(i) = &self.instruction {
            match m.last_mut() {
                Some(last) if last["role"] == "user" && last["content"].is_array() => last["content"].as_array_mut().unwrap().push(text(i)),
                _ => m.push(json!({"role": "user", "content": [text(i)]})),
            }
        }
        m
    }
}

// ---------------------------------------------------------------- the stream

/// Token counts for a reply, as its provider gave them: running totals
/// (the larger of two reports wins).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Prompt tokens neither read from nor written to a cache.
    pub input: u64,
    pub cache_read: u64,
    /// Cache writes, all; and by lifetime where the provider says.
    pub write: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub output: u64,
    /// How many of the output tokens were thinking.
    pub reasoning: Option<u64>,
}

impl Usage {
    pub fn merge(&mut self, u: &Usage) {
        self.input = self.input.max(u.input);
        self.cache_read = self.cache_read.max(u.cache_read);
        self.write = self.write.max(u.write).max(u.write_5m + u.write_1h);
        self.write_5m = self.write_5m.max(u.write_5m);
        self.write_1h = self.write_1h.max(u.write_1h);
        self.output = self.output.max(u.output);
        if u.reasoning.is_some() {
            self.reasoning = u.reasoning;
        }
    }

    /// The whole prompt: fresh, read and written.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.write
    }
}

/// Why a reply ended.
pub mod stop {
    pub const END: &str = "end";
    pub const TOOL_CALLS: &str = "tool_calls";
    pub const MAX_TOKENS: &str = "max_tokens";
    pub const REFUSAL: &str = "refusal";
    /// The model ran out of context while writing.
    pub const CONTEXT: &str = "context";
    pub const PAUSE: &str = "pause";
    /// The turn is over and its work already done: its tool calls are in the
    /// transcript as what happened, not as work still to do. (Quick runs its
    /// own loop, and Oak runs the tools as they are asked for, so nothing is
    /// left for Oak's loop to run.)
    pub const COMPLETE: &str = "complete";
    /// Quick took in a message the user typed while its turn ran: the reply
    /// so far is done (its calls run and answered), the message goes in after
    /// it, and the next request reads on in Quick's same turn.
    pub const STEERED: &str = "steered";
}

/// What a provider's stream says, in Oak's terms.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The reply began: its id (a server thread continues from it) and the
    /// model that writes it.
    Start { id: String, model: String },
    Usage(Usage),
    /// A block began: its first form (an empty `text`, `reasoning` or a
    /// `tool_call` with an empty input).
    Block { index: usize, block: Value },
    Text { index: usize, text: String },
    Reasoning { index: usize, text: String },
    /// A reasoning block's `sig`, whole.
    Sig { index: usize, sig: String },
    /// More of a tool call's input, as JSON text.
    Input { index: usize, json: String },
    BlockEnd { index: usize },
    /// Why it ended (`stop`), and what a refusal said.
    Stop { reason: String, detail: Option<String> },
    /// What the reply cost, in dollars (credit-based providers).
    Cost(f64),
    /// The upstream that served it (OpenRouter).
    ServedBy(String),
    /// The request failed and goes again in `wait` seconds.
    Retry { attempt: u32, wait: u64, reason: String },
    /// The server refused to continue its thread; the request went whole.
    ThreadRestart(String),
    /// The response's HTTP status (for the benchmark log).
    Headers(u16),
}

/// A reply's text, stop reason and usage, from all of its events.
#[derive(Clone, Debug, Default)]
pub struct Collected {
    pub text: String,
    pub stop: String,
    pub detail: Option<String>,
    pub usage: Usage,
    pub usd: f64,
}

pub fn collect(events: &[Event]) -> Collected {
    let mut texts: Vec<Option<String>> = vec![];
    let mut c = Collected::default();
    for ev in events {
        match ev {
            Event::Block { index, block } => {
                if texts.len() <= *index {
                    texts.resize(index + 1, None);
                }
                texts[*index] = is(block, "text").then(|| block["text"].as_str().unwrap_or("").to_string());
            }
            Event::Text { index, text } => {
                if let Some(t) = texts.get_mut(*index).and_then(Option::as_mut) {
                    t.push_str(text);
                }
            }
            Event::Stop { reason, detail } => {
                c.stop = reason.clone();
                if detail.is_some() {
                    c.detail = detail.clone();
                }
            }
            Event::Usage(u) => c.usage.merge(u),
            Event::Cost(usd) => c.usd += usd,
            Event::Retry { .. } | Event::ThreadRestart(_) => {
                texts.clear();
                c.usage = Usage::default();
            }
            _ => {}
        }
    }
    c.text = texts.into_iter().flatten().collect::<Vec<_>>().join("").trim().to_string();
    c
}

// ---------------------------------------------------------------- from the old format

/// A block of the Anthropic Messages format, as conversations were kept
/// before format 1, in the Oak format. `by` is the provider whose model wrote
/// the conversation when a thinking block doesn't say (`_oak_model`).
pub fn from_anthropic_block(b: &Value, by: &str) -> Value {
    let mut out = match b["type"].as_str().unwrap_or("") {
        "text" => text(b["text"].as_str().unwrap_or("")),
        "image" => image(b["source"]["media_type"].as_str().unwrap_or("image/png"), b["source"]["data"].as_str().unwrap_or("")),
        "document" => json!({
            "type": "document",
            "media_type": b["source"]["media_type"].as_str().unwrap_or("application/pdf"),
            "data": b["source"]["data"],
            "title": b["title"],
        }),
        "thinking" => {
            let mut r = json!({"type": "reasoning", "text": b["thinking"].as_str().unwrap_or("")});
            let sig = b["signature"].as_str().unwrap_or("");
            if !sig.is_empty() {
                r["sig"] = json!(sig);
            }
            r["by"] = json!(thinking_by(b, by));
            r
        }
        "redacted_thinking" => json!({"type": "reasoning", "text": "", "sig": b["data"], "by": "claude", "redacted": true}),
        "tool_use" => json!({"type": "tool_call", "id": b["id"], "name": b["name"], "input": b["input"]}),
        "tool_result" => {
            let content = match &b["content"] {
                Value::Array(a) => Value::Array(a.iter().map(|x| from_anthropic_block(x, by)).collect()),
                Value::Null => json!(""),
                other => other.clone(),
            };
            let mut r = json!({"type": "tool_result", "call_id": b["tool_use_id"], "content": content});
            if b["is_error"].as_bool() == Some(true) {
                r["is_error"] = json!(true);
            } else if b.get("is_error").is_some() {
                r["is_error"] = json!(false);
            }
            r
        }
        _ => b.clone(),
    };
    // Oak's own keys stay (when it came in, how long it took…).
    if let (Some(o), Some(src)) = (out.as_object_mut(), b.as_object()) {
        for (k, v) in src.iter().filter(|(k, _)| k.starts_with("_oak_")) {
            o.insert(k.clone(), v.clone());
        }
    }
    out
}

/// The provider that wrote a thinking block of the old format: the one of
/// the model it was marked with, else Codex's for an encrypted reasoning
/// item in the signature, else `by`.
fn thinking_by(b: &Value, by: &str) -> String {
    if let Some(m) = b["_oak_model"].as_str() {
        return crate::providers::Kind::of(m).id().to_string();
    }
    let sig = b["signature"].as_str().unwrap_or("");
    if sig.starts_with('{') && serde_json::from_str::<Value>(sig).is_ok_and(|v| v["type"] == "reasoning") {
        return "codex".into();
    }
    by.to_string()
}

/// Old-format messages in the Oak format (see `from_anthropic_block`).
pub fn from_anthropic(messages: &[Value], by: &str) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let content = match &m["content"] {
                Value::String(s) => vec![text(s)],
                Value::Array(a) => a.iter().map(|b| from_anthropic_block(b, by)).collect(),
                _ => vec![],
            };
            let mut out = json!({"role": m["role"], "content": content});
            if let (Some(o), Some(src)) = (out.as_object_mut(), m.as_object()) {
                for (k, v) in src.iter().filter(|(k, _)| k.starts_with("_oak_")) {
                    o.insert(k.clone(), v.clone());
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_blocks_become_oak_blocks() {
        let old = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi", "_oak_at": 5}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hm", "signature": "EqQB", "_oak_ms": 3},
                {"type": "thinking", "thinking": "plan", "signature": "{\"type\":\"reasoning\",\"encrypted_content\":\"e\"}"},
                {"type": "redacted_thinking", "data": "R"},
                {"type": "tool_use", "id": "t1", "name": "READ", "input": {"path": "a"}, "caller": {"type": "direct"}},
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "x"}], "is_error": false}]}),
        ];
        let new = from_anthropic(&old, "claude");
        assert_eq!(new[0]["content"][0], json!({"type": "text", "text": "hi", "_oak_at": 5}));
        assert_eq!(new[0]["content"][1], json!({"type": "image", "media_type": "image/png", "data": "AA"}));
        assert_eq!(new[1]["content"][0], json!({"type": "reasoning", "text": "hm", "sig": "EqQB", "by": "claude", "_oak_ms": 3}));
        assert_eq!(new[1]["content"][1]["by"], "codex");
        assert_eq!(new[1]["content"][2], json!({"type": "reasoning", "text": "", "sig": "R", "by": "claude", "redacted": true}));
        assert_eq!(new[1]["content"][3], json!({"type": "tool_call", "id": "t1", "name": "READ", "input": {"path": "a"}}));
        assert_eq!(new[2]["content"][0], json!({"type": "tool_result", "call_id": "t1", "content": [{"type": "text", "text": "x"}], "is_error": false}));
    }

    #[test]
    fn collect_joins_text_and_keeps_the_last_attempt() {
        let ev = vec![
            Event::Block { index: 0, block: text("") },
            Event::Text { index: 0, text: "lost".into() },
            Event::Retry { attempt: 1, wait: 1, reason: "x".into() },
            Event::Block { index: 0, block: json!({"type": "reasoning", "text": ""}) },
            Event::Reasoning { index: 0, text: "hm".into() },
            Event::Block { index: 1, block: text("") },
            Event::Text { index: 1, text: " Hello ".into() },
            Event::Usage(Usage { input: 3, output: 4, ..Default::default() }),
            Event::Stop { reason: stop::END.into(), detail: None },
        ];
        let c = collect(&ev);
        assert_eq!(c.text, "Hello");
        assert_eq!(c.stop, "end");
        assert_eq!(c.usage.output, 4);
    }

    /// Call ids from every provider's models are made to fit the strictest
    /// API's pattern and to be unique, with each result keeping its call; ids
    /// that fit stay as they are.
    #[test]
    fn call_ids_fit_the_strictest_api() {
        let call = |id: Value| json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": "READ", "input": {}}]});
        let result = |id: Value| json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": "x"}]});
        let ok = vec![call(json!("call_1")), result(json!("call_1"))];
        assert!(matches!(fit_call_ids(&ok, 64), std::borrow::Cow::Borrowed(_)));
        let plain = vec![
            call(json!("call:1/bad")),
            result(json!("call:1/bad")),
            // Two calls of one id (a plugin's: one per reply).
            call(json!("call_0")),
            result(json!("call_0")),
            call(json!("call_0")),
            result(json!("call_0")),
            // A call with no id at all.
            call(Value::Null),
        ];
        let out = fit_call_ids(&plain, 64).into_owned();
        let ids: Vec<String> = out.iter().filter_map(|m| m["content"][0]["id"].as_str().map(String::from)).collect();
        assert_eq!(ids, ["call_1_bad", "call_0", "call_0_2", "call"], "{ids:?}");
        let results: Vec<String> = out.iter().filter_map(|m| m["content"][0]["call_id"].as_str().map(String::from)).collect();
        assert_eq!(results, ["call_1_bad", "call_0", "call_0_2"], "each result its newest call's id");
        // Long ones are clipped.
        let long = vec![call(json!("x".repeat(100)))];
        let out = fit_call_ids(&long, 64).into_owned();
        assert_eq!(out[0]["content"][0]["id"].as_str().unwrap().len(), 64);
    }
}
