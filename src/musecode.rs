//! Muse Code: Meta's Muse models (Wonder, Spark, Ember), the ones Meta's
//! Muse Code agent runs, through the Meta Model API with the user's API
//! key, paid per token. (A Muse Code subscription works only in Meta's own
//! `muse` CLI, with its own login: Oak doesn't use it.)
//!
//! Requests go to the API's Anthropic-compatible Messages endpoint
//! (`/v1/messages`, bearer auth), which is stateless: the body is the Claude
//! one (`crate::wire_anthropic`), less what Meta refuses (`body_for`). The
//! models always think: `thinking` is always adaptive, with effort as
//! `output_config.effort` (low to xhigh on Messages). Earlier thinking goes
//! back as the API sent it: a `redacted_thinking` block carries the
//! encrypted chain of thought, which the model reads again; the summaries
//! beside it are accepted and ignored. What a reply cost is worked out from
//! its usage and the model's rates.
//!
//! The keys are kept in musecode.json in OAK's folder (Home adds and
//! removes them), or come from META_API_KEY; connected to a command
//! center they are kept there, for every OAK (`provider::Keys`). A key is
//! checked against `/v1/models` when it is added. MUSECODE_BASE_URL points
//! the requests elsewhere (a proxy, or a test server).

use crate::api::{self, Sse};
use crate::oak::{self, Event, Request};
use crate::provider;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::mpsc::Sender;
use std::time::Duration;

const BASE: &str = "https://api.meta.ai/v1";

/// What every request says it is.
const USER_AGENT: &str = concat!("OAK/", env!("CARGO_PKG_VERSION"));

/// The API's address: MUSECODE_BASE_URL when set, else Meta's.
fn base() -> String {
    std::env::var("MUSECODE_BASE_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()).unwrap_or_else(|| BASE.into())
}

/// A model OAK offers through the Meta Model API.
pub struct Model {
    /// The id the API takes, and OAK's id for it.
    pub id: &'static str,
    pub name: &'static str,
    /// Its context window (tokens), the prompt and the reply together.
    pub context: u64,
    /// The most a reply may write (less when the prompt leaves less room).
    pub max_output: u64,
    /// The effort levels it tells apart (`agent::efforts`): Messages takes
    /// low to xhigh.
    pub efforts: &'static [&'static str],
    /// Dollars per million tokens: new input, input read from the cache,
    /// output (thinking included), as Meta's pricing page listed them.
    pub price: (f64, f64, f64),
    /// Rates for a request with more than 200K input tokens (cached ones
    /// included), on the whole request, when the model has them.
    pub long: Option<(f64, f64, f64)>,
}

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Spark 1.3 first (the one Meta recommends), so it is the one picked.
pub const MODELS: &[Model] = &[
    Model { id: "muse-spark-1.3", name: "Muse Spark 1.3", context: 1_048_576, max_output: 131_072, efforts: EFFORTS, price: (1.25, 0.15, 4.25), long: None },
    Model { id: "muse-wonder-1.0", name: "Muse Wonder 1.0", context: 1_048_576, max_output: 131_072, efforts: EFFORTS, price: (4.00, 0.20, 20.00), long: None },
    Model { id: "muse-ember-1.0", name: "Muse Ember 1.0", context: 1_048_576, max_output: 131_072, efforts: EFFORTS, price: (0.10, 0.004, 0.50), long: Some((0.30, 0.012, 1.50)) },
    Model {
        // Contributor mode: Meta may train on what it is sent.
        id: "muse-spark-1.3-contributor",
        name: "Muse Spark 1.3 Contributor",
        context: 1_048_576,
        max_output: 131_072,
        efforts: EFFORTS,
        price: (0.10, 0.002, 0.20),
        long: None,
    },
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// A model in the picker served through the Meta Model API.
pub fn serves(model_id: &str) -> bool {
    model(model_id).is_some()
}

/// What a reply cost on `m` (dollars): `input` new prompt tokens, `cached`
/// read from the cache (Messages counts them apart, as Claude does), and
/// `output` written.
fn cost(m: &Model, (input, cached, output): (u64, u64, u64)) -> f64 {
    let p = match m.long {
        Some(long) if input + cached > 200_000 => long,
        _ => m.price,
    };
    (input as f64 * p.0 + cached as f64 * p.1 + output as f64 * p.2) / 1e6
}

// ---------------------------------------------------------------- the keys

/// What OAK keeps beside a key, for the account box.
#[derive(Clone, Default)]
pub(crate) struct Stats {
    /// The last check of the key failed (not a refusal: the network).
    error: Option<String>,
}

pub(crate) static KEYS: LazyLock<provider::Keys<Stats>> = LazyLock::new(|| {
    provider::Keys::new(provider::KeyCfg { name: "Muse Code", kind: api::Provider::MuseCode, file: "musecode.json", env: "META_API_KEY", vault: "musecode" })
});

/// A Meta Model API key is set.
pub fn on() -> bool {
    KEYS.on()
}

/// Checks `key` with the API (its model list), then saves it (on the vault
/// when one is connected) beside the others.
pub fn set_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Paste a Meta Model API key.".into());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("A key has no spaces in it.".into());
    }
    check(key).map_err(|e| format!("The Meta Model API refused the key: {e}"))?;
    KEYS.take_remote(key)?;
    KEYS.edit(&provider::key_id(key), |k| k.extras.error = None);
    Ok(())
}

/// Whether the API takes `key`: Err says why not.
fn check(key: &str) -> Result<(), String> {
    let resp = api::http()
        .get(format!("{}/models", base()))
        .bearer_auth(key)
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(api::network_error)?;
    api::json_or(resp, error_text).map(drop)
}

/// Checks each key again, for the account boxes (in the background).
pub fn refresh() {
    for k in KEYS.list() {
        let r = check(&k.key);
        if r.is_ok() {
            KEYS.accepted(&k.id);
        }
        KEYS.edit(&k.id, |k| k.extras.error = r.err());
    }
}

/// A key (or all of them) as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    /// The key, shortened ("LLM_…3f9a"), or how many keys.
    pub label: String,
    /// What OAK spent through it, all told (dollars).
    pub spent: f64,
    pub error: Option<String>,
    /// The vault's.
    pub remote: bool,
    /// The API refused the key itself: requests skip it.
    pub rejected: bool,
}

fn account_of(k: &provider::Key<Stats>) -> Account {
    let error = match &k.rejected {
        Some(why) => Some(format!("The Meta Model API rejected this key ({why}): remove it")),
        None => k.extras.error.clone(),
    };
    Account { id: k.id.clone(), label: provider::short_key(&k.key), spent: k.spent, error, remote: k.remote, rejected: k.rejected.is_some() }
}

/// Each key, in order.
pub fn accounts() -> Vec<Account> {
    KEYS.list().iter().map(account_of).collect()
}

/// The key conversation `conv` is on (`crate::rotation`), else all of
/// them together (`account`).
pub fn account_of_conv(conv: &str) -> Option<Account> {
    let id = crate::rotation::seat(api::Provider::MuseCode, conv);
    id.and_then(|id| accounts().into_iter().find(|a| a.id == id)).or_else(account)
}

/// All the keys together, when one is set: what is spent summed; the first
/// error, unless another key works. With several, the label says how many.
pub fn account() -> Option<Account> {
    let all = accounts();
    let mut first = all.first()?.clone();
    if all.len() > 1 {
        first.label = format!("{} keys", all.len());
    }
    let spent = all.iter().map(|a| a.spent).sum();
    let error = if all.iter().any(|a| a.error.is_none()) { None } else { all.iter().find_map(|a| a.error.clone()) };
    Some(Account { spent, error, remote: all.iter().any(|a| a.remote), rejected: all.iter().all(|a| a.rejected), ..first })
}

// ---------------------------------------------------------------- requests

/// The effort for request `r` to `m`: the lowest for a title, and for a
/// request that turns thinking off (the models can't stop thinking); else
/// the request's, fitted to the model's levels. A summary keeps the
/// conversation's, so it reads what the conversation's requests cached.
fn effort(r: &Request, m: &Model) -> &'static str {
    if r.purpose == oak::Purpose::Title || !r.thinking {
        return m.efforts[0];
    }
    match r.effort.as_deref() {
        Some("minimal" | "low") => "low",
        Some("medium") => "medium",
        Some("xhigh" | "max" | "ultra") => "xhigh",
        _ => "high",
    }
}

/// The least a request lets the model write, its thinking included.
const MIN_REPLY: u64 = 4_096;

/// Fields of a tool definition the API takes; Oak's may carry others (for
/// Claude) that it would refuse.
const TOOL_FIELDS: [&str; 3] = ["name", "description", "input_schema"];

/// The Messages body for an Oak request to model `m`: Claude's, with
/// thinking always on (`disabled` is refused), the effort fitted, no cache
/// breakpoints (Meta caches by itself; a lifetime it may refuse), tools
/// with only the fields the API takes, and a reply limit that fits beside
/// the prompt. Everything comes from the request alone, so a conversation's
/// requests grow by appending, as the cache needs.
fn body_for(r: &Request, m: &Model) -> Value {
    let mut body = crate::wire_anthropic::body(r, "musecode", false);
    body["model"] = json!(m.id);
    body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
    body["output_config"] = json!({"effort": effort(r, m)});
    strip_cache_marks(&mut body["messages"]);
    if let Some(tools) = body["tools"].as_array_mut() {
        for t in tools {
            if let Some(o) = t.as_object_mut() {
                o.retain(|k, _| TOOL_FIELDS.contains(&k.as_str()));
            }
        }
    }
    // Mid-conversation system messages only come in Claude's own mode
    // (`crate::prefix::mode` sends this provider's as text); were one to
    // come anyway, it goes as a user turn's text rather than be refused.
    if let Some(msgs) = body["messages"].as_array_mut() {
        for msg in msgs.iter_mut().filter(|x| x["role"] == "system") {
            let text: Vec<&str> = msg["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
            *msg = json!({"role": "user", "content": [{"type": "text", "text": text.join("\n\n")}]});
        }
    }
    // No tools (a side request): no tools field, rather than a null.
    if body["tools"].as_array().is_none_or(Vec::is_empty) {
        let o = body.as_object_mut().expect("a body is an object");
        o.remove("tools");
        o.remove("tool_choice");
    }
    // The model thinks before it answers, even for a title's few words:
    // a limit that small leaves room for the thinking too (the reply still
    // stops when it is done).
    let asked = if r.max_tokens == 0 { m.max_output } else { r.max_tokens.max(MIN_REPLY).min(m.max_output) };
    let room = m.context.saturating_sub(estimate(&body) * 11 / 10 + 512);
    body["max_tokens"] = json!(asked.min(room).max(1_024.min(asked)));
    body
}

/// Removes `cache_control` from every block in `v`.
fn strip_cache_marks(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(strip_cache_marks),
        Value::Object(o) => {
            o.remove("cache_control");
            o.values_mut().for_each(strip_cache_marks);
        }
        _ => {}
    }
}

/// A rough count of a body's tokens, high rather than low: three characters
/// a token, and an image or document's data as a page's worth.
fn estimate(body: &Value) -> u64 {
    fn walk(v: &Value, n: &mut u64) {
        match v {
            Value::String(s) => *n += s.len() as u64 / 3,
            Value::Array(a) => a.iter().for_each(|x| walk(x, n)),
            Value::Object(o) => {
                for (k, x) in o {
                    if k == "data" && o.get("type").is_some_and(|t| t == "base64") {
                        *n += 2_000;
                    } else {
                        walk(x, n);
                    }
                }
            }
            _ => {}
        }
    }
    let mut n = 0;
    walk(body, &mut n);
    n
}

/// An error reply's text: what its JSON says, with a prompt past the window
/// as the agent expects it ("prompt is too long: …"), which it answers by
/// summarizing.
fn error_text(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = provider::error_msg(&v, body, v.get("message").and_then(Value::as_str));
    if let Some((input, _, window)) = context_numbers(&msg)
        && input >= window
    {
        return format!("prompt is too long: {input} tokens > {window} maximum");
    }
    format!("HTTP {status}: {msg}")
}

/// The prompt's tokens, the reply limit asked for and the window, from the
/// API's "You passed N input tokens and requested M output tokens. However,
/// the model's context length is only C tokens".
fn context_numbers(msg: &str) -> Option<(u64, u64, u64)> {
    let after = |pat: &str| -> Option<u64> {
        let rest = &msg[msg.find(pat)? + pat.len()..];
        rest.trim_start().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
    };
    Some((after("You passed")?, after("and requested")?, after("context length is only")?))
}

/// Streams one Oak request through the Meta Model API, sending its events
/// to `tx`: failed requests are retried (with `Retry` events), a key
/// refused or without billing passes the conversation to another, and the
/// cost goes on the key's total.
pub fn stream(req: Request, tx: Sender<Sse>) {
    stream_on(&KEYS, req, tx)
}

/// `stream` through `keys`.
fn stream_on(keys: &provider::Keys<Stats>, req: Request, tx: Sender<Sse>) {
    let id = req.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err(format!("The Meta Model API doesn't serve {id} through Oak."))));
    };
    let conv = req.seat_conv();
    let mut body = body_for(&req, m);
    let (resp, key_id) = match open(keys, &mut body, &conv, &tx) {
        Ok(r) => r,
        Err(Some(s)) => return drop(tx.send(s)),
        Err(None) => return,
    };
    // The reply's usage as it comes (running totals), and at its end what
    // it cost, sent before the relay's `Done`.
    let usage = std::cell::Cell::new((0u64, 0u64, 0u64));
    api::relay(
        resp,
        &tx,
        "musecode",
        |ev| {
            if ev["type"] != "message_stop" {
                return vec![];
            }
            let usd = cost(m, usage.get());
            if usd <= 0.0 {
                return vec![];
            }
            keys.spend(&key_id, usd);
            vec![Event::Cost(usd)]
        },
        |ev| {
            if let Event::Usage(u) = ev {
                let (i, c, o) = usage.get();
                usage.set((i.max(u.input), c.max(u.cache_read), o.max(u.output)));
            }
        },
    );
}

/// Sends `body` through conversation `conv`'s key until a response comes
/// that can be read as a stream: network failures, rate limits and server
/// errors retried; a key refused (401, 403) or without billing (402)
/// passes the request to another; a reply limit past the window brought
/// down. Ok has the key's id; Err carries what the receiver gets, or None
/// when it is gone.
fn open(keys: &provider::Keys<Stats>, body: &mut Value, conv: &str, tx: &Sender<Sse>) -> Result<(reqwest::blocking::Response, String), Option<Sse>> {
    let mut k = keys.pick(conv, |_| 1e6).map_err(|e| Some(Sse::Err(e)))?;
    let mut tries: u32 = 0;
    let mut fixes: u32 = 0;
    loop {
        let key = k.key.clone();
        let sent = body.clone();
        let build = || {
            api::http()
                .post(format!("{}/messages", base()))
                .bearer_auth(&key)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "text/event-stream")
                .json(&sent)
                .timeout(Duration::from_secs(900))
        };
        let why = |r: reqwest::blocking::Response| format!("Muse Code {}", error_text(r.status().as_u16(), &r.text().unwrap_or_default()));
        let r = match provider::send_retrying(tx, &mut tries, build, why).map_err(|()| None)? {
            Ok(r) if r.status().is_success() => {
                if !provider::say_headers(tx, r.status().as_u16()) {
                    return Err(None);
                }
                return Ok((r, k.id));
            }
            Ok(r) => r,
            Err(e) => return Err(Some(Sse::Err(e))),
        };
        let status = r.status().as_u16();
        let text = r.text().unwrap_or_default();
        if matches!(status, 401..=403) {
            // Billing (402): another key, of another team, may have it.
            // Refused (401, 403): another key takes the conversation, and
            // this one waits on Home.
            let billing = status == 402;
            let label = provider::short_key(&k.key);
            let msg = error_text(status, &text);
            let set_aside = if billing { keys.limited(&k.id, api::now_ms() + 3_600_000) } else { keys.reject(&k.id, &msg) };
            if set_aside
                && let Ok(next) = keys.pick(conv, |_| 1e6)
                && next.id != k.id
            {
                k = next;
                if tx.send(Sse::Rotated(provider::short_key(&k.key))).is_err() {
                    return Err(None);
                }
                tries = 0;
                continue;
            }
            return Err(Some(Sse::Err(if billing {
                format!("Muse Code: the Meta Model API needs billing set up ({msg}): see dev.meta.ai.")
            } else {
                format!("Muse Code: the Meta Model API rejected key {label} ({msg}): remove it on Home, or add one that works.")
            })));
        }
        if status == 400 && fixes < 3 {
            // The prompt fits, the reply limit doesn't: bring it down.
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let msg = provider::error_msg(&v, &text, None);
            if let Some((input, _, window)) = context_numbers(&msg)
                && let Some(room) = window.checked_sub(input + 64).filter(|r| *r >= 256 && Some(*r) < body["max_tokens"].as_u64())
            {
                fixes += 1;
                body["max_tokens"] = json!(room);
                continue;
            }
        }
        return Err(Some(Sse::Err(format!("Muse Code {}", error_text(status, &text)))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str) -> Request {
        Request {
            provider: crate::providers::Kind::MuseCode,
            model: model.into(),
            max_tokens: 32_000,
            system: "You are Oak.".into(),
            tools: vec![json!({"name": "READ", "description": "Read a file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}, "cache_control": {"type": "ephemeral"}, "eager_input_streaming": true})],
            thinking: true,
            effort: Some("high".into()),
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "media_type": "image/png", "data": "AAA"}]}),
                json!({"role": "assistant", "content": [
                    {"type": "reasoning", "text": "summary", "sig": "", "by": "musecode"},
                    {"type": "reasoning", "text": "", "sig": "ENC", "by": "musecode", "redacted": true},
                    {"type": "reasoning", "text": "theirs", "sig": "S", "by": "claude"},
                    {"type": "tool_call", "id": "t1", "name": "READ", "input": {"path": "a"}},
                ]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "x"}]}),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn request_shape() {
        let m = model("muse-spark-1.3").unwrap();
        let b = body_for(&req(m.id), m);
        assert_eq!(b["model"], "muse-spark-1.3");
        assert_eq!(b["system"], "You are Oak.");
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_tokens"], 32_000);
        assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(b["output_config"], json!({"effort": "high"}));
        // Only what the API takes, at the top and in tools.
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        for k in &keys {
            assert!(["model", "max_tokens", "stream", "system", "messages", "tools", "tool_choice", "thinking", "output_config"].contains(k), "{k}");
        }
        assert_eq!(b["tools"][0].as_object().unwrap().len(), 3);
        // Images as Messages blocks; its own reasoning back (the encrypted
        // chain of thought as redacted_thinking), Claude's left out.
        let c = &b["messages"][0]["content"];
        assert_eq!(c[1]["source"], json!({"type": "base64", "media_type": "image/png", "data": "AAA"}));
        let a = &b["messages"][1]["content"];
        assert_eq!(a[0], json!({"type": "thinking", "thinking": "summary", "signature": ""}));
        assert_eq!(a[1], json!({"type": "redacted_thinking", "data": "ENC"}));
        assert_eq!(a[2]["type"], "tool_use");
        assert_eq!(a.as_array().unwrap().len(), 3);
        assert_eq!(b["messages"][2]["content"][0], json!({"type": "tool_result", "tool_use_id": "t1", "content": "x"}));
    }

    #[test]
    fn thinking_and_effort() {
        let m = model("muse-wonder-1.0").unwrap();
        let mut r = req(m.id);
        for (asked, sent) in [("minimal", "low"), ("low", "low"), ("medium", "medium"), ("xhigh", "xhigh"), ("max", "xhigh"), ("ultra", "xhigh")] {
            r.effort = Some(asked.into());
            assert_eq!(body_for(&r, m)["output_config"]["effort"], sent, "{asked}");
        }
        // Thinking can't be turned off: the lowest effort instead.
        r.thinking = false;
        let b = body_for(&r, m);
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "low");
        // A title thinks least; a summary as the conversation does, with
        // no cache breakpoint before its instruction.
        r.thinking = true;
        r.effort = Some("xhigh".into());
        r.purpose = oak::Purpose::Title;
        r.max_tokens = 64;
        let b = body_for(&r, m);
        assert_eq!(b["output_config"]["effort"], "low");
        assert_eq!(b["max_tokens"], MIN_REPLY);
        r.max_tokens = 32_000;
        r.purpose = oak::Purpose::Summary;
        r.instruction = Some("Summarize".into());
        r.ttl = oak::Ttl::OneHour;
        let b = body_for(&r, m);
        assert_eq!(b["output_config"]["effort"], "xhigh");
        assert!(!b.to_string().contains("cache_control"));
        assert_eq!(b["messages"][2]["content"][1], json!({"type": "text", "text": "Summarize"}));
        // Tools off; none at all.
        r.tools_off = true;
        assert_eq!(body_for(&r, m)["tool_choice"], json!({"type": "none"}));
        r.tools.clear();
        let b = body_for(&r, m);
        assert!(b.get("tools").is_none() && b.get("tool_choice").is_none());
    }

    #[test]
    fn requests_grow_by_appending() {
        let m = model("muse-spark-1.3").unwrap();
        let mut r = req(m.id);
        let before = body_for(&r, m);
        r.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}));
        r.messages.push(json!({"role": "user", "content": [{"type": "text", "text": "next"}]}));
        let after = body_for(&r, m);
        for k in ["system", "tools", "thinking", "output_config"] {
            assert_eq!(before[k], after[k], "{k}");
        }
        let (b, a) = (before["messages"].as_array().unwrap(), after["messages"].as_array().unwrap());
        assert_eq!(&a[..b.len()], &b[..]);
    }

    #[test]
    fn reply_limit_fits_the_window() {
        let m = model("muse-ember-1.0").unwrap();
        let mut r = req(m.id);
        r.max_tokens = 0;
        assert_eq!(body_for(&r, m)["max_tokens"], m.max_output);
        r.messages[0]["content"][0]["text"] = json!("x".repeat(3_000_000));
        let b = body_for(&r, m);
        let n = b["max_tokens"].as_u64().unwrap();
        assert!(n < m.max_output && n >= 1_024, "{n}");
    }

    #[test]
    fn costs() {
        let spark = model("muse-spark-1.3").unwrap();
        // 200k new, 800k cached, 100k written.
        let usd = cost(spark, (200_000, 800_000, 100_000));
        assert!((usd - (0.2 * 1.25 + 0.8 * 0.15 + 0.1 * 4.25)).abs() < 1e-12, "{usd}");
        // Ember's long-context rates, on the whole request.
        let ember = model("muse-ember-1.0").unwrap();
        assert!((cost(ember, (100_000, 100_000, 0)) - (0.1 * 0.10 + 0.1 * 0.004)).abs() < 1e-12);
        assert!((cost(ember, (100_000, 100_001, 1_000_000)) - (0.1 * 0.30 + 0.100001 * 0.012 + 1.50)).abs() < 1e-9);
        assert_eq!(cost(spark, (0, 0, 0)), 0.0);
    }

    #[test]
    fn errors() {
        let long = r#"{"type":"error","error":{"type":"invalid_request_error","message":"You passed 1200064 input tokens and requested 1 output tokens. However, the model's context length is only 1048576 tokens, resulting in a maximum input length of 1048575 tokens. Please reduce the length of the input prompt"}}"#;
        assert_eq!(error_text(400, long), "prompt is too long: 1200064 tokens > 1048576 maximum");
        let fits = r#"{"error":{"message":"You passed 1000000 input tokens and requested 131072 output tokens. However, the model's context length is only 1048576 tokens"}}"#;
        assert!(error_text(400, fits).starts_with("HTTP 400: You passed"));
        assert_eq!(error_text(401, r#"{"type":"error","error":{"type":"authentication_error","message":"Unauthorized"}}"#), "HTTP 401: Unauthorized");
    }

    #[test]
    fn models() {
        assert!(serves("muse-spark-1.3") && serves("muse-wonder-1.0") && serves("muse-ember-1.0"));
        assert!(!serves("muse-spark") && !serves("claude-opus-4-5"));
        for m in MODELS {
            assert_eq!(crate::providers::Kind::of(m.id), crate::providers::Kind::MuseCode, "{}", m.id);
        }
    }

    /// A fake Meta Model API on a local port: it checks what each request
    /// carries and answers with a Messages stream (thinking, redacted
    /// thinking, text, a tool call), after refusing the first key.
    #[test]
    fn mocked_api_round_trip() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut s = conn.unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let (mut len, mut auth) = (0usize, String::new());
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = h["authorization:".len()..].trim().to_string();
                    }
                }
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).unwrap();
                let body: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
                log.lock().unwrap().push((path.clone(), auth.clone(), body.clone()));
                let reply = if auth != "Bearer LLM_good_key" {
                    let e = r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key"}}"#;
                    format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{e}", e.len())
                } else if body["system"] == "TOO BIG" && body["max_tokens"].as_u64() > Some(48_512) {
                    let e = json!({"type": "error", "error": {"type": "invalid_request_error", "message": format!("You passed 1000000 input tokens and requested {} output tokens. However, the model's context length is only 1048576 tokens", body["max_tokens"])}}).to_string();
                    format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{e}", e.len())
                } else if path == "/v1/models" {
                    let b = r#"{"object":"list","data":[{"id":"muse-spark-1.3","object":"model","created":0,"owned_by":"meta"}]}"#;
                    format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len())
                } else {
                    let evs = [
                        json!({"type": "message_start", "message": {"id": "msg_1", "model": "muse-spark-1.3", "usage": {"input_tokens": 1000, "cache_read_input_tokens": 9000, "output_tokens": 1}}}),
                        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
                        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Reading it."}}),
                        json!({"type": "content_block_stop", "index": 0}),
                        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "ENCRYPTED"}}),
                        json!({"type": "content_block_stop", "index": 1}),
                        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
                        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "Let me look."}}),
                        json!({"type": "content_block_stop", "index": 2}),
                        json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "call_1", "name": "READ", "input": {}}}),
                        json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"a\"}"}}),
                        json!({"type": "content_block_stop", "index": 3}),
                        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 2000, "output_tokens_details": {"thinking_tokens": 1500}}}),
                        json!({"type": "message_stop"}),
                    ];
                    let sse: String = evs.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
                    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}", sse.len())
                };
                let _ = s.write_all(reply.as_bytes());
            }
        });
        // SAFETY of the env: only this test reads MUSECODE_BASE_URL.
        unsafe { std::env::set_var("MUSECODE_BASE_URL", format!("http://127.0.0.1:{port}/v1")) };
        assert!(set_key("LLM_bad_key").unwrap_err().contains("Invalid API key"));
        assert!(check("LLM_good_key").is_ok());

        // Requests through keys of their own (a file in a scratch folder),
        // the refused one first: it is set aside, and the good one takes
        // the conversation.
        let dir = std::env::temp_dir().join(format!("oak-musecode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file: &'static str = Box::leak(dir.join("keys.json").to_string_lossy().into_owned().into_boxed_str());
        let keys = provider::Keys::<Stats>::new(provider::KeyCfg { name: "Muse Code", kind: api::Provider::MuseCode, file, env: "OAK_MUSECODE_TEST_NO_KEY", vault: "musecode" });
        keys.keep("LLM_bad_key", false).unwrap();
        let run = |r: Request| {
            let (tx, rx) = std::sync::mpsc::channel();
            stream_on(&keys, r, tx);
            rx.into_iter().collect::<Vec<Sse>>()
        };
        let mut r = req("muse-spark-1.3");
        r.conv = Some(format!("musecode-test-{}", std::process::id()));
        // Only the refused key: it is set aside, with the reason.
        let got = run(r.clone());
        assert!(matches!(got.last(), Some(Sse::Err(e)) if e.contains("rejected key …_key") && e.contains("Invalid API key") && !e.contains("LLM_bad")));
        assert!(keys.list()[0].rejected.is_some());
        // With a good one beside it, that one takes the conversation.
        keys.keep("LLM_good_key", false).unwrap();
        let got = run(r.clone());
        let events: Vec<&Event> = got.iter().filter_map(|s| if let Sse::Event(e) = s { Some(e) } else { None }).collect();
        assert!(events.iter().any(|e| matches!(e, Event::Block { block, .. } if block["type"] == "reasoning" && block["redacted"] == true && block["sig"] == "ENCRYPTED" && block["by"] == "musecode")));
        assert!(events.iter().any(|e| matches!(e, Event::Reasoning { text, .. } if text == "Reading it.")));
        assert!(events.iter().any(|e| matches!(e, Event::Text { text, .. } if text == "Let me look.")));
        assert!(events.iter().any(|e| matches!(e, Event::Block { block, .. } if block["type"] == "tool_call" && block["name"] == "READ")));
        assert!(events.iter().any(|e| matches!(e, Event::Stop { reason, .. } if reason == oak::stop::TOOL_CALLS)));
        // 1000 new, 9000 cached, 2000 written on Spark 1.3; then done.
        let usd = (1000.0 * 1.25 + 9000.0 * 0.15 + 2000.0 * 4.25) / 1e6;
        let n = got.len();
        assert!(matches!(&got[n - 2], Sse::Event(Event::Cost(c)) if (c - usd).abs() < 1e-12));
        assert!(matches!(got[n - 1], Sse::Done));
        let good = keys.list().into_iter().find(|k| k.key == "LLM_good_key").unwrap();
        assert!((good.spent - usd).abs() < 1e-12);
        {
            let sent = seen.lock().unwrap();
            let (path, auth, posted) = sent.last().unwrap();
            assert_eq!((path.as_str(), auth.as_str()), ("/v1/messages", "Bearer LLM_good_key"));
            assert_eq!(posted["thinking"]["type"], "adaptive");
            assert_eq!(posted["messages"][1]["content"][1], json!({"type": "redacted_thinking", "data": "ENC"}));
            assert!(posted.get("context_management").is_none());
        }
        // A reply limit that doesn't fit beside the prompt comes down.
        r.system = "TOO BIG".into();
        r.max_tokens = 131_072;
        let got = run(r);
        let errors: Vec<&String> = got.iter().filter_map(|s| if let Sse::Err(e) = s { Some(e) } else { None }).collect();
        assert!(matches!(got.last(), Some(Sse::Done)), "{errors:?}");
        let sent = seen.lock().unwrap();
        assert_eq!(sent.last().unwrap().2["max_tokens"], 48_512);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
