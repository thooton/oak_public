//! A model on the user's own llama.cpp server (`llama-server`), through its
//! Anthropic Messages endpoint. The user starts the server themselves (a
//! .bat of theirs); OAK only connects, to the URL kept in local.json with
//! the tokens used so far, which the account box shows.
//!
//! The server is often started after Oak, or restarted with another model
//! or `-c`, so its model and context are read again (`/v1/models`,
//! `/props`) at start, after adding it, and in the background after each
//! request that reached it (at most every `PROBE_EVERY`); a "too long"
//! refusal's `n_ctx` is kept too. There is one server, so every `local/`
//! id is its model whatever alias it was saved under: a conversation
//! started before the alias was known keeps working, under the new name.
//! The user can name the model and cap its context (Home, right-click the
//! server, Edit server); the cap never goes past what the server has.
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
use std::time::{Duration, Instant};

/// What OAK names the server's model: "local/" and the server's alias.
pub const PREFIX: &str = "local/";

/// The URL a llama-server listens on unless told otherwise.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// The alias kept until the server has said its own.
const UNKNOWN_ALIAS: &str = "local";

/// The context window when the server can't say (`--ctx-size` of the
/// launcher), and the most a reply may write.
const DEFAULT_CONTEXT: u64 = 32_768;
const MAX_OUTPUT: u64 = 8_192;

/// The least context Oak works with, whatever is set.
const MIN_CONTEXT: u64 = 4_096;

/// How often a request that reached the server reads its model and context
/// again.
const PROBE_EVERY: Duration = Duration::from_secs(60);

/// local.json: the server, its model as it was last seen, what the user
/// set for it, and the tokens used through it, all time.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Server {
    pub url: String,
    /// The server's model alias (`--alias`), e.g. "qwen3.8-27b"; "local"
    /// until the server has said it.
    pub alias: String,
    /// Its context window (the server's `n_ctx`); 0 until the server has
    /// said it.
    pub context: u64,
    /// The user's name for the model; empty: one made from the alias.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The most context Oak uses, as the user set it; 0: the server's.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_limit: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub requests: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Server {
    /// Every token through it: prompts (new and from its cache) and replies.
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.output
    }

    /// The model's name: the user's, else one made from the alias
    /// ("Inkling 2 Mini" from "inkling-2-mini").
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        if self.alias.is_empty() || self.alias == UNKNOWN_ALIAS {
            return "llama.cpp model".into();
        }
        pretty_name(&self.alias)
    }

    /// The context Oak works with: the user's cap within the server's
    /// window, else the server's, else the default until it has said.
    pub fn window(&self) -> u64 {
        let w = match (self.context_limit, self.context) {
            (0, 0) => DEFAULT_CONTEXT,
            (0, server) => server,
            (cap, 0) => cap,
            (cap, server) => cap.min(server),
        };
        w.max(MIN_CONTEXT)
    }
}

/// A readable name from a server's alias or model file: the file's name
/// without ".gguf", its words split at "-", "_" and spaces, lowercase
/// words capitalized and sizes upper-cased ("qwen3.8-27b" to "Qwen3.8 27B").
/// Words with capitals of their own stay as they are.
pub fn pretty_name(alias: &str) -> String {
    let file = alias.rsplit(['/', '\\']).next().unwrap_or(alias);
    let stem = file.strip_suffix(".gguf").or_else(|| file.strip_suffix(".GGUF")).unwrap_or(file);
    let words: Vec<String> = stem
        .split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            if w.chars().any(|c| c.is_ascii_uppercase()) {
                return w.to_string();
            }
            // "27b", "3.4b", "8k": a number with a unit.
            let (num, unit) = w.split_at(w.len() - w.chars().last().map_or(0, char::len_utf8));
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.') && matches!(unit, "b" | "m" | "k" | "t") {
                return format!("{num}{}", unit.to_ascii_uppercase());
            }
            let mut c = w.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
        })
        .collect();
    if words.is_empty() { alias.to_string() } else { words.join(" ") }
}

/// "64k", "32.5k", "812": a context size as `-c` usually gives it.
pub fn context_text(n: u64) -> String {
    if n >= 1024 && n.is_multiple_of(1024) { format!("{}k", n / 1024) } else { tokens_text(n) }
}

/// A context size the user typed ("64k", "65536", "128K"); 0 for nothing.
pub fn parse_context(s: &str) -> Result<u64, String> {
    let s = s.trim().to_ascii_lowercase().replace([',', '_', ' '], "");
    if s.is_empty() {
        return Ok(0);
    }
    let (num, mult) = match s.strip_suffix('k') {
        Some(n) => (n, 1024.0),
        None => (s.as_str(), 1.0),
    };
    match num.parse::<f64>() {
        Ok(n) if n > 0.0 && (n * mult) as u64 >= MIN_CONTEXT => Ok((n * mult).round() as u64),
        Ok(_) => Err(format!("At least {}.", context_text(MIN_CONTEXT))),
        Err(_) => Err("A context size, like 64k or 65536.".into()),
    }
}

static STATE: Mutex<Option<Server>> = Mutex::new(None);

/// When the server's model and context were last read (tried), so requests
/// don't read them every time.
static PROBED: Mutex<Option<Instant>> = Mutex::new(None);

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

/// Adds the server at `url`, or changes the one added: the user's name
/// for its model and context cap (empty and 0: the server's own). What the
/// server says of its model and context is read in the background (the UI
/// doesn't wait on a server that isn't up); the token count stays.
pub fn add(url: &str, name: &str, context_limit: u64) {
    let url = base(if url.trim().is_empty() { DEFAULT_URL } else { url });
    let old = server().unwrap_or_default();
    let same = old.url == url;
    let mut s = Server { url, name: name.trim().to_string(), context_limit, ..old };
    if !same {
        // Another server: what was seen of the old one's model doesn't hold.
        s.alias.clear();
        s.context = 0;
    }
    if s.alias.is_empty() {
        s.alias = UNKNOWN_ALIAS.into();
    }
    save(&s);
    *STATE.lock().unwrap() = Some(s);
    *PROBED.lock().unwrap() = None;
    std::thread::spawn(refresh);
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
    // A slot's window; /v1/models says it too on newer servers.
    let context = props["default_generation_settings"]["n_ctx"]
        .as_u64()
        .or(models["data"][0]["meta"]["n_ctx"].as_u64())
        .unwrap_or(0);
    Ok((alias, context))
}

/// Reads the server's model and context again, when it is running (at
/// start, after adding it, and after requests, in the background), keeping
/// what is known when it isn't.
pub fn refresh() {
    let Some(s) = server() else { return };
    *PROBED.lock().unwrap() = Some(Instant::now());
    if let Ok((alias, context)) = probe(&s.url) {
        seen(&s.url, &alias, context);
    }
}

/// `refresh`, when it last ran more than `PROBE_EVERY` ago.
fn refresh_if_due() {
    let due = PROBED.lock().unwrap().is_none_or(|t| t.elapsed() >= PROBE_EVERY);
    if due {
        refresh();
    }
}

/// Keeps what the server at `url` said of its model and context (empty
/// and 0: it didn't say), saving local.json when that changed.
fn seen(url: &str, alias: &str, context: u64) {
    let mut st = STATE.lock().unwrap();
    if let Some(cur) = st.as_mut().filter(|c| c.url == url) {
        let changed = (!alias.is_empty() && alias != cur.alias) || (context > 0 && context != cur.context);
        if !alias.is_empty() {
            cur.alias = alias.to_string();
        }
        if context > 0 {
            cur.context = context;
        }
        if changed {
            save(cur);
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

/// Whether `model` gets the basic tools only, and a short window's ways
/// (summaries kept short, no MCP servers): a local model's or a plugin's
/// (`crate::provided`), which both read tools from their prompt or take
/// them less well than the big APIs' models.
pub fn basic(model: &str) -> bool {
    serves(model) || crate::provided::serves(model)
}

/// "Inkling 2 Mini (local)": the server's model, whichever alias `model`
/// was saved under (there is one server); with none added, from the id.
pub fn model_name(model: &str) -> String {
    let name = server().map_or_else(|| pretty_name(model.strip_prefix(PREFIX).unwrap_or(model)), |s| s.display_name());
    format!("{name} (local)")
}

/// Its context window: the user's cap within the server's, as last seen.
pub fn context(_model: &str) -> u64 {
    server().map_or(DEFAULT_CONTEXT, |s| s.window())
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
    // The server's model as it last said, whatever alias the conversation
    // saved (a router-mode server picks the model by it).
    let alias = if s.alias.is_empty() || s.alias == UNKNOWN_ALIAS {
        model_id.strip_prefix(PREFIX).unwrap_or(UNKNOWN_ALIAS).to_string()
    } else {
        s.alias.clone()
    };
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
        // It is up: what it serves now (another model, another `-c`) is
        // read beside the reply, for the next request.
        if result.is_ok() {
            std::thread::spawn(refresh_if_due);
        }
        match result {
            Ok(r) if r.status().is_success() => break r,
            Ok(r) => {
                let status = r.status().as_u16();
                let text = r.text().unwrap_or_default();
                if let Some(n_ctx) = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["n_ctx"].as_u64()) {
                    seen(&s.url, "", n_ctx);
                }
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
    fn names_read_well() {
        assert_eq!(pretty_name("inkling-2-mini"), "Inkling 2 Mini");
        assert_eq!(pretty_name("qwen3.8-27b"), "Qwen3.8 27B");
        assert_eq!(pretty_name(r"C:\models\Inkling-2-Mini-UD-IQ3_XXS.gguf"), "Inkling 2 Mini UD IQ3 XXS");
        assert_eq!(pretty_name("Inkling 2 Mini"), "Inkling 2 Mini");
        let s = Server { alias: "inkling-2-mini".into(), ..Default::default() };
        assert_eq!(s.display_name(), "Inkling 2 Mini");
        assert_eq!(Server { name: " Mine ".into(), ..s }.display_name(), "Mine");
        assert_eq!(Server { alias: UNKNOWN_ALIAS.into(), ..Default::default() }.display_name(), "llama.cpp model");
    }

    #[test]
    fn the_window_is_the_servers_within_the_users_cap() {
        let s = |context, context_limit| Server { context, context_limit, ..Default::default() }.window();
        assert_eq!(s(0, 0), DEFAULT_CONTEXT);
        assert_eq!(s(65_536, 0), 65_536);
        assert_eq!(s(65_536, 49_152), 49_152);
        assert_eq!(s(32_768, 131_072), 32_768);
        assert_eq!(s(0, 131_072), 131_072);
        assert_eq!(s(1_000, 0), MIN_CONTEXT);
        assert_eq!(parse_context("64k"), Ok(65_536));
        assert_eq!(parse_context(" 65,536 "), Ok(65_536));
        assert_eq!(parse_context(""), Ok(0));
        assert!(parse_context("lots").is_err() && parse_context("1k").is_err());
        assert_eq!(context_text(65_536), "64k");
    }

    #[test]
    fn too_long_reads_like_the_claude_api() {
        let e = r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":40123,"n_ctx":32768}}"#;
        assert_eq!(too_long(e).as_deref(), Some("prompt is too long: 40123 tokens > 32768 maximum"));
        assert_eq!(too_long(r#"{"error":{"message":"other"}}"#), None);
    }
}
