//! Refuseless: hosted abliterated open-weight models behind an
//! OpenAI-compatible API (chat completions), paid per token from the
//! account's credits.
//!
//! Requests and streams are NodeClub's chat completions ones
//! (`crate::nodeclub`: system message, tool calls and tool messages, images
//! as `image_url` parts, reasoning from `reasoning_content` or think tags),
//! with Refuseless's own `reasoning_effort` (off, low, high, max). What a
//! reply cost is worked out from its usage and the model's rates, and the
//! credits left come from `/v1/credits`.
//!
//! The keys are kept in refuseless.json in OAK's folder (Home adds and
//! removes them), or come from REFUSELESS_API_KEY; connected to a command
//! center they are kept there, for every OAK (`provider::Keys`).
//!
//! Refuseless sits behind Cloudflare, which turns away requests without a
//! user agent (reqwest sends none): every request names OAK.

use crate::api::{self, Sse};
use crate::nodeclub::{self, Translator};
use crate::oak::{self, Event, Request};
use crate::provider;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::mpsc::Sender;
use std::time::Duration;

const BASE: &str = "https://api.refuseless.com/v1";

/// What every request says it is (see the module's note on Cloudflare).
const USER_AGENT: &str = concat!("OAK/", env!("CARGO_PKG_VERSION"));

/// The API's address: REFUSELESS_BASE_URL when set (a proxy, or a test
/// server), else Refuseless's.
fn base() -> String {
    std::env::var("REFUSELESS_BASE_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()).unwrap_or_else(|| BASE.into())
}

/// A model OAK offers through Refuseless.
pub struct Model {
    /// The id Refuseless takes, and OAK's id for it.
    pub id: &'static str,
    pub name: &'static str,
    /// Its context window (tokens).
    pub context: u64,
    /// The most a reply may write (less when the prompt leaves less room).
    /// Refuseless holds a request's worst-case cost before it starts, so
    /// this stays well under the window.
    pub max_output: u64,
    /// The effort levels it tells apart (`agent::efforts`).
    pub efforts: &'static [&'static str],
    /// It takes `reasoning_effort: off` (requests that shouldn't think
    /// send that; the others send its lowest level).
    pub off: bool,
    /// It reads images.
    pub vision: bool,
    /// Dollars per million tokens: new input, input read from the cache,
    /// output (reasoning included), as `/v1/models` listed them in November
    /// 2026.
    pub price: (f64, f64, f64),
}

pub const MODELS: &[Model] = &[
    Model {
        id: "glm-5.3-flash-abliterated",
        name: "GLM 5.3 Flash Abliterated",
        context: 1_048_576,
        max_output: 131_072,
        efforts: &["low", "high", "max"],
        off: false,
        vision: true,
        price: (0.12, 0.025, 0.50),
    },
    Model {
        // The 1017 checkpoint of DeepSeek V4.1 Flash (Refuseless's id
        // names no date).
        id: "deepseek-v4.1-flash-abliterated",
        name: "DeepSeek V4.1 Flash 1017 Abliterated",
        context: 1_048_576,
        max_output: 131_072,
        efforts: &["low", "high", "max"],
        off: true,
        vision: true,
        price: (0.12, 0.025, 0.45),
    },
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// A model in the picker served through Refuseless.
pub fn serves(model_id: &str) -> bool {
    model(model_id).is_some()
}

/// What a reply with `usage` (prompt tokens, of them read from the cache,
/// written) cost on `m` (dollars).
fn cost(m: &Model, (input, cached, output): (u64, u64, u64)) -> f64 {
    let fresh = input.saturating_sub(cached) as f64;
    (fresh * m.price.0 + cached as f64 * m.price.1 + output as f64 * m.price.2) / 1e6
}

// ---------------------------------------------------------------- the keys

/// What OAK keeps beside a key, for the account box.
#[derive(Clone, Default)]
pub(crate) struct Stats {
    /// Credits a new request can use (dollars), from /credits.
    available: Option<f64>,
    /// Credits on the account, all told (dollars).
    balance: Option<f64>,
    /// `spent` when the credits were fetched: what was spent since comes
    /// off what is available, until they are fetched again.
    spent_then: f64,
    error: Option<String>,
}

pub(crate) static KEYS: LazyLock<provider::Keys<Stats>> = LazyLock::new(|| {
    provider::Keys::new(provider::KeyCfg { name: "Refuseless", kind: api::Provider::Refuseless, file: "refuseless.json", env: "REFUSELESS_API_KEY", vault: "refuseless" })
});

/// A Refuseless key is set.
pub fn on() -> bool {
    KEYS.on()
}

/// Checks `key` with Refuseless (its credits), then saves it (on the
/// vault when one is connected) beside the others.
pub fn set_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Paste a Refuseless key.".into());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("A key has no spaces in it.".into());
    }
    let credits = fetch_credits(key).map_err(|e| format!("Refuseless refused the key: {e}"))?;
    KEYS.take_remote(key)?;
    KEYS.edit(&provider::key_id(key), |k| {
        let spent = k.spent;
        read_credits(&mut k.extras, &credits, spent);
    });
    Ok(())
}

fn fetch_credits(key: &str) -> Result<Value, String> {
    let resp = api::http()
        .get(format!("{}/credits", base()))
        .bearer_auth(key)
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(api::network_error)?;
    api::json_or(resp, nodeclub::error_text)
}

fn read_credits(s: &mut Stats, v: &Value, spent: f64) {
    s.available = v["available"].as_f64();
    s.balance = v["balance"].as_f64();
    s.spent_then = spent;
    s.error = None;
}

/// Fetches each key's credits, for the account boxes (in the background).
pub fn refresh() {
    for k in KEYS.list() {
        let r = fetch_credits(&k.key);
        if r.is_ok() {
            KEYS.accepted(&k.id);
        }
        KEYS.edit(&k.id, |k| match r {
            Ok(v) => {
                let spent = k.spent;
                read_credits(&mut k.extras, &v, spent);
            }
            Err(e) => k.extras.error = Some(e),
        });
    }
}

/// A Refuseless key (or all of them) as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    /// The key, shortened ("abl_…3f9a"), or how many keys.
    pub label: String,
    /// Dollars a request can still use: the credits available when last
    /// fetched, less what was spent since.
    pub left: Option<f64>,
    /// Credits on the account.
    pub balance: Option<f64>,
    /// What OAK spent through it, all told.
    pub spent: f64,
    pub error: Option<String>,
    /// The vault's.
    pub remote: bool,
    /// Refuseless refused the key itself: requests skip it.
    pub rejected: bool,
}

fn account_of(k: &provider::Key<Stats>) -> Account {
    let s = &k.extras;
    let since = (k.spent - s.spent_then).max(0.0);
    let error = match &k.rejected {
        Some(why) => Some(format!("Refuseless rejected this key ({why}): remove it")),
        None => s.error.clone(),
    };
    Account {
        id: k.id.clone(),
        label: provider::short_key(&k.key),
        left: s.available.map(|a| a - since),
        balance: s.balance,
        spent: k.spent,
        error,
        remote: k.remote,
        rejected: k.rejected.is_some(),
    }
}

/// Each key, in order.
pub fn accounts() -> Vec<Account> {
    KEYS.list().iter().map(account_of).collect()
}

/// The key conversation `conv` is on (`crate::rotation`), else all of
/// them together (`account`).
pub fn account_of_conv(conv: &str) -> Option<Account> {
    let id = crate::rotation::seat(api::Provider::Refuseless, conv);
    id.and_then(|id| accounts().into_iter().find(|a| a.id == id)).or_else(account)
}

/// All the keys together, when one is set: what is spent summed, and the
/// credits of each account counted once (keys of one account share them);
/// the first error, unless another key works. With several, the label
/// says how many.
pub fn account() -> Option<Account> {
    let all = accounts();
    let mut first = all.first()?.clone();
    if all.len() > 1 {
        first.label = format!("{} keys", all.len());
    }
    let mut seen: Vec<f64> = vec![];
    let (mut left, mut balance) = (None::<f64>, None::<f64>);
    for a in &all {
        if let Some(b) = a.balance {
            if seen.contains(&b) {
                continue;
            }
            seen.push(b);
            balance = Some(balance.unwrap_or(0.0) + b);
        }
        if let Some(l) = a.left {
            left = Some(left.unwrap_or(0.0) + l);
        }
    }
    let spent = all.iter().map(|a| a.spent).sum();
    let error = if all.iter().any(|a| a.error.is_none()) { None } else { all.iter().find_map(|a| a.error.clone()) };
    Some(Account { left, balance, spent, error, remote: all.iter().any(|a| a.remote), rejected: all.iter().all(|a| a.rejected), ..first })
}

/// What a key has left, for rotation: its credits (dollars), or plenty
/// while they aren't known.
fn left_of(k: &provider::Key<Stats>) -> f64 {
    account_of(k).left.unwrap_or(1e6)
}

// ---------------------------------------------------------------- requests

/// The `reasoning_effort` for request `r` to `m`: none for summaries and
/// titles, or when the request turns thinking off (`off`, or the model's
/// lowest level when it can't turn reasoning off); else the request's
/// effort, fitted to Refuseless's levels.
fn effort(r: &Request, m: &Model) -> &'static str {
    let thinks = r.thinking && matches!(r.purpose, oak::Purpose::Main | oak::Purpose::Rewrite);
    if !thinks {
        return if m.off { "off" } else { "low" };
    }
    match r.effort.as_deref() {
        Some("low") => "low",
        Some("xhigh" | "max" | "ultra") => "max",
        _ => "high",
    }
}

/// The chat completions request for an Oak request to model `m`.
fn chat_body(r: &Request, m: &Model) -> Value {
    let mut req = json!({
        "model": m.id,
        "messages": nodeclub::chat_messages(r, m.vision),
        "stream": true,
        "stream_options": {"include_usage": true},
        "reasoning_effort": effort(r, m),
    });
    let tools = nodeclub::chat_tools(&r.tools);
    if !tools.is_empty() {
        req["tools"] = Value::Array(tools);
        req["tool_choice"] = json!(if r.tools_off { "none" } else { "auto" });
    }
    // The reply's limit: what was asked for (the model's most), within what
    // the prompt leaves of the window.
    let asked = if r.max_tokens == 0 { m.max_output } else { r.max_tokens.min(m.max_output) };
    let room = m.context.saturating_sub(nodeclub::estimate(&req) * 11 / 10 + 512);
    req["max_tokens"] = json!(asked.min(room).max(1_024.min(asked)));
    req
}

/// Streams one Oak request through Refuseless, sending its events to `tx`:
/// failed requests are retried (with `Retry` events), a key out of credits
/// or refused passes the conversation to another, and the cost goes on the
/// key's total.
pub fn stream(body: Request, tx: Sender<Sse>) {
    let id = body.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err(format!("Refuseless doesn't serve {id}."))));
    };
    let conv = body.seat_conv();
    let mut req = chat_body(&body, m);
    let (resp, key_id) = match open(&mut req, &conv, &tx) {
        Ok(r) => r,
        Err(Some(s)) => return drop(tx.send(s)),
        Err(None) => return,
    };
    let mut t = Translator::of(&id, "refuseless", "Refuseless");
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
            let usd = cost(m, t.usage);
            for e in events.into_iter().chain((usd > 0.0).then_some(Event::Cost(usd))) {
                if tx.send(Sse::Event(e)).is_err() {
                    return;
                }
            }
            if usd > 0.0 {
                KEYS.spend(&key_id, usd);
            }
            let _ = tx.send(Sse::Done);
        }
        Err(e) => drop(tx.send(Sse::Err(e))),
    }
}

/// Sends `req` through conversation `conv`'s key until a response comes
/// that can be read as a stream: network failures, rate limits (a model
/// warming up) and server errors retried; a key out of credits (402) or
/// refused (401, 403) passes the request to another; a reply limit past
/// the window brought down. Ok has the key's id; Err carries what the
/// receiver gets, or None when it is gone.
fn open(req: &mut Value, conv: &str, tx: &Sender<Sse>) -> Result<(reqwest::blocking::Response, String), Option<Sse>> {
    let mut k = KEYS.pick(conv, left_of).map_err(|e| Some(Sse::Err(e)))?;
    let mut tries: u32 = 0;
    let mut fixes: u32 = 0;
    loop {
        let key = k.key.clone();
        let body = req.clone();
        let build = || {
            api::http()
                .post(format!("{}/chat/completions", base()))
                .bearer_auth(&key)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "text/event-stream")
                .json(&body)
                .timeout(Duration::from_secs(900))
        };
        let why = |r: reqwest::blocking::Response| format!("Refuseless {}", nodeclub::error_text(r.status().as_u16(), &r.text().unwrap_or_default()));
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
            // Out of credits (402): another key, of another account, may
            // have some. Refused (401, 403: revoked, or a model not on the
            // plan): another key takes the conversation, and this one
            // waits on Home.
            let credits = status == 402;
            let label = provider::short_key(&k.key);
            let msg = nodeclub::error_text(status, &text);
            let set_aside = if credits { KEYS.limited(&k.id, api::now_ms() + 3_600_000) } else { KEYS.reject(&k.id, &msg) };
            if set_aside
                && let Ok(next) = KEYS.pick(conv, left_of)
                && next.id != k.id
            {
                k = next;
                if tx.send(Sse::Rotated(provider::short_key(&k.key))).is_err() {
                    return Err(None);
                }
                tries = 0;
                continue;
            }
            return Err(Some(Sse::Err(if credits {
                format!("Refuseless is out of credits: {msg}")
            } else {
                format!("Refuseless rejected key {label} ({msg}): remove it on Home, or add one that works.")
            })));
        }
        if (400..500).contains(&status) && fixes < 3 {
            fixes += 1;
            let lower = text.to_lowercase();
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let msg = v["error"]["message"].as_str().or(v["detail"].as_str()).unwrap_or(&text).to_string();
            if (lower.contains("max_tokens") || lower.contains("max_completion_tokens") || lower.contains("maximum context length"))
                && let Some(room) = nodeclub::room_for_reply(&msg).filter(|r| Some(*r) < req["max_tokens"].as_u64())
            {
                req["max_tokens"] = json!(room);
                continue;
            }
        }
        return Err(Some(Sse::Err(format!("Refuseless {}", nodeclub::error_text(status, &text)))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(model: &str) -> Request {
        Request {
            provider: crate::providers::Kind::Refuseless,
            model: model.into(),
            max_tokens: 32_000,
            system: "You are Oak.".into(),
            tools: vec![json!({"name": "READ", "description": "Read a file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}})],
            thinking: true,
            effort: Some("high".into()),
            messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "media_type": "image/png", "data": "AAA"}]})],
            ..Default::default()
        }
    }

    #[test]
    fn request_shape() {
        let ds = model("deepseek-v4.1-flash-abliterated").unwrap();
        let glm = model("glm-5.3-flash-abliterated").unwrap();
        let req = chat_body(&body(ds.id), ds);
        assert_eq!(req["model"], "deepseek-v4.1-flash-abliterated");
        assert_eq!(req["messages"][0], json!({"role": "system", "content": "You are Oak."}));
        assert_eq!(req["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAA");
        assert_eq!(req["tools"][0]["function"]["name"], "READ");
        assert_eq!(req["reasoning_effort"], "high");
        assert_eq!(req["max_tokens"], 32_000);
        assert_eq!(req["stream_options"]["include_usage"], true);
        assert!(req.get("chat_template_kwargs").is_none());
        // Efforts fitted to off, low, high, max.
        let mut b = body(ds.id);
        b.effort = Some("xhigh".into());
        assert_eq!(chat_body(&b, ds)["reasoning_effort"], "max");
        b.effort = Some("low".into());
        assert_eq!(chat_body(&b, ds)["reasoning_effort"], "low");
        // Titles and summaries don't think: off where the model can, else
        // its lowest level.
        b.purpose = oak::Purpose::Title;
        assert_eq!(chat_body(&b, ds)["reasoning_effort"], "off");
        assert_eq!(chat_body(&b, glm)["reasoning_effort"], "low");
        // The two models, and nothing else.
        assert_eq!(MODELS.len(), 2);
        assert!(serves("glm-5.3-flash-abliterated") && !serves("glm-5.3-abliterated") && !serves("deepseek/deepseek-v4.1-flash-1017"));
        assert_eq!(crate::providers::Kind::of("glm-5.3-flash-abliterated"), crate::providers::Kind::Refuseless);
        assert_eq!(crate::providers::Kind::of("deepseek-v4.1-flash-abliterated"), crate::providers::Kind::Refuseless);
    }

    #[test]
    fn costs() {
        let ds = model("deepseek-v4.1-flash-abliterated").unwrap();
        // 1M prompt tokens, 800k of them cached, and 100k written.
        let usd = cost(ds, (1_000_000, 800_000, 100_000));
        assert!((usd - (0.2 * 0.12 + 0.8 * 0.025 + 0.1 * 0.45)).abs() < 1e-12, "{usd}");
        assert_eq!(cost(ds, (0, 0, 0)), 0.0);
    }

    #[test]
    fn stream_is_read_as_refuseless() {
        let mut t = Translator::of("glm-5.3-flash-abliterated", "refuseless", "Refuseless");
        let mut out = vec![];
        for c in [
            json!({"id": "x", "choices": [{"index": 0, "delta": {"reasoning_content": "hm"}, "finish_reason": null}]}),
            json!({"id": "x", "choices": [{"index": 0, "delta": {"content": "Hi"}, "finish_reason": "stop"}]}),
            json!({"id": "x", "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 4}}),
        ] {
            out.extend(t.feed(&c).unwrap());
        }
        t.done = true;
        out.extend(t.finish().unwrap());
        assert!(out.iter().any(|e| matches!(e, Event::Block { block, .. } if block["by"] == "refuseless")));
        assert_eq!(t.usage, (10, 0, 4));
        let e = Translator::of("m", "refuseless", "Refuseless").feed(&json!({"error": {"message": "boom"}})).err().unwrap();
        assert_eq!(e, "Refuseless error: boom");
    }
}
