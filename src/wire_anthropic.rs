//! The Anthropic Messages wire format, for the providers that speak it (the
//! Claude API and llama.cpp's server): an Oak request as a Messages body,
//! and a Messages stream as Oak events.

use crate::oak::{self, Event, Request, Usage};
use serde_json::{Value, json};

/// Whether text is what the Messages API refuses in a text block: none,
/// or nothing but whitespace (other providers' models write such blocks
/// between their calls, and one sent back gets the request refused).
fn blank(t: &Value) -> bool {
    t.as_str().is_none_or(|s| s.trim().is_empty())
}

/// The picture types the Messages API takes.
const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// What stands in for a picture or document the API can't take (another
/// provider's type, or no data).
fn unsendable(kind: &str, b: &Value) -> Value {
    let t = b["media_type"].as_str().filter(|t| !t.is_empty()).unwrap_or("unknown");
    json!({"type": "text", "text": format!("[A {kind} of type {t} that can't be sent to this model]")})
}

/// A tool name as Claude's pattern takes it (letters, digits, `_` and `-`,
/// at most 64): other providers' tools (a Quick device's, named after the
/// device) may have other characters. Names that fit stay as they are.
fn fitted_name(name: &Value) -> Value {
    let s = name.as_str().unwrap_or("");
    let fits = !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
    if fits {
        return name.clone();
    }
    let f: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect();
    json!(if f.is_empty() { "tool".to_string() } else { f })
}

/// A call's input as an object: one another provider kept as JSON text is
/// read, anything else is empty.
fn call_input(input: &Value) -> Value {
    match input {
        Value::Object(_) => input.clone(),
        Value::String(s) => serde_json::from_str::<Value>(s).ok().filter(Value::is_object).unwrap_or_else(|| json!({})),
        _ => json!({}),
    }
}

/// An Oak block as a Messages block, or None when the provider `by` can't
/// take it (reasoning written by another provider: its `sig` means nothing
/// here; Claude also refuses a thought it didn't sign; text that is blank).
pub fn block(b: &Value, by: &str, signed: bool) -> Option<Value> {
    Some(match b["type"].as_str().unwrap_or("") {
        "text" if blank(&b["text"]) => return None,
        "text" => json!({"type": "text", "text": b["text"]}),
        "image" if !IMAGE_TYPES.contains(&b["media_type"].as_str().unwrap_or("")) || blank(&b["data"]) => unsendable("picture", b),
        "image" => json!({"type": "image", "source": {"type": "base64", "media_type": b["media_type"], "data": b["data"]}}),
        "document" if !matches!(b["media_type"].as_str(), Some("application/pdf" | "text/plain")) || !b["data"].is_string() => unsendable("document", b),
        "document" => {
            let source = if b["media_type"] == "text/plain" {
                json!({"type": "text", "media_type": "text/plain", "data": b["data"]})
            } else {
                json!({"type": "base64", "media_type": b["media_type"], "data": b["data"]})
            };
            let mut d = json!({"type": "document", "source": source});
            if b["title"].is_string() {
                d["title"] = b["title"].clone();
            }
            d
        }
        "reasoning" => {
            if b["by"].as_str().unwrap_or(by) != by {
                return None;
            }
            let sig = b["sig"].as_str().unwrap_or("");
            if b["redacted"] == true {
                return (!sig.is_empty()).then(|| json!({"type": "redacted_thinking", "data": sig}));
            }
            if signed && sig.is_empty() {
                return None;
            }
            json!({"type": "thinking", "thinking": b["text"].as_str().unwrap_or(""), "signature": sig})
        }
        "tool_call" => json!({"type": "tool_use", "id": b["id"], "name": fitted_name(&b["name"]), "input": call_input(&b["input"])}),
        "tool_result" => {
            let mut content = match &b["content"] {
                Value::Array(a) => Value::Array(a.iter().filter_map(|x| block(x, by, signed)).collect()),
                Value::String(s) => json!(s),
                _ => json!(""),
            };
            let error = b["is_error"].as_bool();
            // The API refuses an error with nothing in it (another
            // provider's call may have failed without a word).
            let empty = match &content {
                Value::Array(a) => a.is_empty(),
                s => blank(s),
            };
            if error == Some(true) && empty {
                content = json!(EMPTY_ERROR);
            }
            let mut r = json!({"type": "tool_result", "tool_use_id": b["call_id"], "content": content});
            if let Some(e) = error {
                r["is_error"] = json!(e);
            }
            r
        }
        _ => return None,
    })
}

/// Oak messages as Messages ones, for provider `by` (`signed`: it refuses
/// unsigned thinking). A reply left with nothing says so, so the turn still
/// has content.
pub fn messages(messages: &[Value], by: &str, signed: bool) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            if m["role"] == "system" {
                return system_message(m);
            }
            let mut content: Vec<Value> = m["content"].as_array().into_iter().flatten().filter_map(|b| block(b, by, signed)).collect();
            if content.is_empty() {
                content.push(json!({"type": "text", "text": if m["role"] == "assistant" { "(No reply text.)" } else { "(empty)" }}));
            }
            json!({"role": m["role"], "content": content})
        })
        .collect()
}

/// Whether Oak reply `m` was written by another provider's model, or its
/// thinking left out as unsigned, as far as a request's messages tell (Oak's
/// own mark, `agent::BY_KEY`, is taken out of them): its reasoning's `by`,
/// else its calls' ids, which Claude's API always starts with `toolu_`. A
/// reply that tells neither (no reasoning, no calls) counts as Claude's.
fn written_elsewhere(m: &Value) -> bool {
    let blocks = m["content"].as_array().map_or(&[][..], |c| c);
    if blocks.iter().any(|b| b.get(crate::agent::BY_KEY).is_some_and(|by| by != "claude")) {
        return true;
    }
    let reasoning: Vec<&Value> = blocks.iter().filter(|b| oak::is_reasoning(b)).collect();
    if !reasoning.is_empty() {
        let dropped = |b: &&Value| b["by"].as_str().is_some_and(|by| by != "claude") || b["sig"].as_str().is_none_or(str::is_empty);
        return reasoning.iter().any(dropped);
    }
    blocks.iter().filter(|b| oak::is_call(b)).any(|b| !b["id"].as_str().is_some_and(|id| id.starts_with("toolu_")))
}

/// Whether a Claude request can go with thinking on, its Messages messages
/// `wire` made from Oak messages `oak` (one for one): the API asks that the
/// reply a turn of tool results continues (the last one, with only
/// result-carrying user turns after it) begin with a thinking block when
/// thinking is on. Another provider's reply (a switch mid-turn: its
/// reasoning is left out) doesn't, nor one whose thinking was unsigned:
/// then not. Claude's own replies that didn't think (adaptive thinking
/// often skips it on a call) are fine, so a turn on Claude alone never
/// toggles it. A user turn without results starts a new turn, where none of
/// this applies.
pub fn thinking_fits(oak: &[Value], wire: &[Value]) -> bool {
    let Some(a) = wire.iter().rposition(|m| m["role"] == "assistant") else { return true };
    let has_result = |m: &Value| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    if wire[a + 1..].iter().any(|m| m["role"] == "user" && !has_result(m)) {
        return true;
    }
    matches!(wire[a]["content"][0]["type"].as_str(), Some("thinking" | "redacted_thinking")) || !oak.get(a).is_some_and(written_elsewhere)
}

/// What an error result with no message says.
pub(crate) const EMPTY_ERROR: &str = "The call failed, with no message.";

/// What a call nothing answered gets as its result.
pub(crate) const UNANSWERED: &str = "No result: this call never finished, so its effects are unknown. Check them before relying on them.";

/// The ids of the blocks of `kind` in Messages message `m` (`key` names them).
fn ids_in(m: &Value, kind: &str, key: &str) -> Vec<String> {
    m["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == kind)
        .filter_map(|b| b[key].as_str().map(String::from))
        .collect()
}

/// Messages as the API's rule for calls takes them: each `tool_use` is
/// answered in the next message (past mid-conversation system messages), a
/// user turn whose `tool_result` blocks come first, and each result answers
/// a call of the reply before it. A conversation other providers' models
/// worked on can break it (a Quick reply cut short with its calls
/// unanswered, two replies in a row after a switch mid-turn, a result whose
/// call another provider's request leaves out), and then every request of
/// the conversation would be refused. So a call nothing answers gets a
/// result saying so (`UNANSWERED`), or goes as text when another reply
/// follows it, a result that answers no call goes as text, and results go
/// first. The messages keep their number and places. Messages that keep the rule are left as they
/// are, and what one becomes depends only on it and the reply before it, so
/// requests still only grow (`crate::prefix`). A reply with calls that ends
/// the messages is left alone: its results haven't come yet.
pub fn pair_calls(messages: &mut Vec<Value>) {
    // The calls of the last reply, not yet answered.
    let mut asked: Vec<String> = vec![];
    let mut i = 0;
    while i < messages.len() {
        match messages[i]["role"].as_str().unwrap_or("") {
            "system" => {}
            "assistant" => {
                asked = ids_in(&messages[i], "tool_use", "id");
                let next = (i + 1..messages.len()).find(|&k| messages[k]["role"] != "system");
                // Another reply comes next (the API joins the two): the
                // calls go as text, so the messages keep their places (a
                // request's one-hour breakpoint goes by index, `cache_ttl`).
                if !asked.is_empty() && next.is_some_and(|k| messages[k]["role"] == "assistant") {
                    for b in messages[i]["content"].as_array_mut().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
                        let name = b["name"].as_str().unwrap_or("a tool").to_string();
                        *b = json!({"type": "text", "text": format!("(Called {name} with {}; the call never finished.)", b["input"])});
                    }
                    asked.clear();
                }
            }
            _ => {
                let Some(content) = messages[i]["content"].as_array() else {
                    asked.clear();
                    i += 1;
                    continue;
                };
                let mut answered: Vec<String> = vec![];
                let (mut results, mut rest) = (vec![], vec![]);
                for b in content {
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    if b["type"] != "tool_result" {
                        rest.push(b.clone());
                    } else if asked.iter().any(|a| a == id) && !answered.iter().any(|a| a == id) {
                        answered.push(id.to_string());
                        results.push(b.clone());
                    } else {
                        // A result of no call here: what it said, as text.
                        rest.push(json!({"type": "text", "text": format!("(The result of call {id}, which isn't in this conversation:)")}));
                        match &b["content"] {
                            Value::Array(a) => rest.extend(a.iter().cloned()),
                            s if !blank(s) => rest.push(json!({"type": "text", "text": s})),
                            _ => {}
                        }
                    }
                }
                for id in asked.iter().filter(|a| !answered.contains(a)) {
                    results.push(json!({"type": "tool_result", "tool_use_id": id, "content": UNANSWERED, "is_error": true}));
                }
                results.extend(rest);
                if results != *content {
                    messages[i]["content"] = Value::Array(results);
                }
                asked.clear();
            }
        }
        i += 1;
    }
}

/// A mid-conversation system message (`crate::prefix`): its tools as
/// `tool_addition` blocks (the inline-tools beta), its text, and its effort
/// as `output_config` (the per-message effort beta).
fn system_message(m: &Value) -> Value {
    let content: Vec<Value> = m["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| match b["type"].as_str() {
            Some("tool_definition") => json!({"type": "tool_addition", "tool": {"type": "tool_definition", "definition": b["definition"]}}),
            _ => json!({"type": "text", "text": b["text"]}),
        })
        .collect();
    let mut out = json!({"role": "system", "content": content});
    if let Some(e) = m["effort"].as_str() {
        out["output_config"] = json!({"effort": e});
    }
    out
}

/// The cache breakpoint for `ttl` (None: no cache).
pub fn cache_mark(ttl: oak::Ttl) -> Value {
    match ttl {
        oak::Ttl::Off => Value::Null,
        oak::Ttl::FiveMinutes => json!({"type": "ephemeral"}),
        oak::Ttl::OneHour => json!({"type": "ephemeral", "ttl": "1h"}),
    }
}

/// `req` as a Messages body for provider `by`: the system prompt as a
/// string, thinking and effort as the API takes them. A summary's
/// instruction joins the last user turn after a breakpoint on what was
/// there, so the request reads exactly what the one it copies wrote.
pub fn body(req: &Request, by: &str, signed: bool) -> Value {
    // Claude checks tool call ids (its pattern, and one per call): a
    // conversation's calls from other providers' models are made to fit.
    // (Through OpenRouter only for Claude's models: others' ids stay as
    // their own models wrote them.)
    let strict = matches!(by, "claude" | "musecode") || (by == "openrouter" && req.model.starts_with("anthropic/"));
    let sent = if strict { oak::fit_call_ids(&req.messages, 64) } else { std::borrow::Cow::Borrowed(req.messages.as_slice()) };
    let mut messages = messages(&sent, by, signed);
    if strict {
        pair_calls(&mut messages);
    }
    if let Some(instruction) = &req.instruction {
        // System messages that end the request it copies came after its
        // breakpoint: a user turn can't follow them, and they don't concern
        // the summary.
        while messages.last().is_some_and(|m| m["role"] == "system") {
            messages.pop();
        }
        let block = json!({"type": "text", "text": instruction});
        match messages.last_mut() {
            Some(last) if last["role"] == "user" => {
                let content = last["content"].as_array_mut().expect("content");
                let mark = cache_mark(req.ttl);
                if !mark.is_null()
                    && let Some(b) = content.last_mut().filter(|b| matches!(b["type"].as_str(), Some("text" | "tool_result")))
                {
                    b["cache_control"] = mark;
                }
                content.push(block);
            }
            _ => messages.push(json!({"role": "user", "content": [block]})),
        }
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "system": req.system,
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = json!(req.tools);
    }
    if req.tools_off {
        body["tool_choice"] = json!({"type": "none"});
    }
    // Claude, in a turn another provider's model began (a switch mid-turn):
    // thinking waits for the next turn, which every Claude model takes (a
    // reply without thinking can't continue with it on).
    let thinking = req.thinking && (by != "claude" || thinking_fits(&sent, body["messages"].as_array().map_or(&[][..], |m| m)));
    body["thinking"] = if thinking { json!({"type": "adaptive", "display": "summarized"}) } else { json!({"type": "disabled"}) };
    if let Some(e) = req.effort.as_deref().filter(|e| !e.is_empty()) {
        body["output_config"] = json!({"effort": e});
    }
    body
}

/// A Messages stop reason in Oak's terms.
pub fn stop_reason(s: &str) -> String {
    match s {
        "end_turn" | "stop_sequence" => oak::stop::END,
        "tool_use" => oak::stop::TOOL_CALLS,
        "max_tokens" => oak::stop::MAX_TOKENS,
        "refusal" => oak::stop::REFUSAL,
        "model_context_window_exceeded" => oak::stop::CONTEXT,
        "pause_turn" => oak::stop::PAUSE,
        other => other,
    }
    .to_string()
}

/// A Messages usage object in Oak's terms.
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let c = &u["cache_creation"];
    Usage {
        input: n("input_tokens"),
        cache_read: n("cache_read_input_tokens"),
        write: n("cache_creation_input_tokens"),
        write_5m: c["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0),
        write_1h: c["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0),
        output: n("output_tokens"),
        reasoning: u["output_tokens_details"]["thinking_tokens"].as_u64(),
    }
}

/// Why a safety classifier declined: the `stop_details` of a refusal
/// ("category cyber: This request was declined because …").
fn refusal_detail(d: &Value) -> Option<String> {
    let d = d.as_object()?;
    let cat = d.get("category").and_then(Value::as_str).map(|c| format!("category {c}"));
    let why = d.get("explanation").and_then(Value::as_str).map(String::from);
    let parts: Vec<String> = cat.into_iter().chain(why).collect();
    (!parts.is_empty()).then(|| parts.join(": "))
}

/// A Messages stream event as Oak events, for provider `by` (whose
/// reasoning it is).
pub fn events(ev: &Value, by: &str) -> Vec<Event> {
    let index = ev["index"].as_u64().unwrap_or(0) as usize;
    match ev["type"].as_str().unwrap_or("") {
        "message_start" => {
            let m = &ev["message"];
            vec![
                Event::Start { id: m["id"].as_str().unwrap_or("").into(), model: m["model"].as_str().unwrap_or("").into() },
                Event::Usage(usage(&m["usage"])),
            ]
        }
        "content_block_start" => {
            let b = &ev["content_block"];
            let block = match b["type"].as_str().unwrap_or("") {
                "thinking" => json!({"type": "reasoning", "text": b["thinking"].as_str().unwrap_or(""), "by": by}),
                "redacted_thinking" => json!({"type": "reasoning", "text": "", "sig": b["data"], "by": by, "redacted": true}),
                "tool_use" => json!({"type": "tool_call", "id": b["id"], "name": b["name"], "input": {}}),
                "text" => oak::text(b["text"].as_str().unwrap_or("")),
                _ => b.clone(),
            };
            vec![Event::Block { index, block }]
        }
        "content_block_delta" => {
            let d = &ev["delta"];
            let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
            match d["type"].as_str().unwrap_or("") {
                "text_delta" => vec![Event::Text { index, text: s("text") }],
                "thinking_delta" => vec![Event::Reasoning { index, text: s("thinking") }],
                "signature_delta" => vec![Event::Sig { index, sig: s("signature") }],
                "input_json_delta" => vec![Event::Input { index, json: s("partial_json") }],
                _ => vec![],
            }
        }
        "content_block_stop" => vec![Event::BlockEnd { index }],
        "message_delta" => {
            let mut out = vec![];
            if ev["usage"].is_object() {
                out.push(Event::Usage(usage(&ev["usage"])));
            }
            if let Some(s) = ev["delta"]["stop_reason"].as_str() {
                out.push(Event::Stop { reason: stop_reason(s), detail: refusal_detail(&ev["delta"]["stop_details"]) });
            }
            out
        }
        _ => vec![],
    }
}

#[cfg(test)]
#[path = "wire_anthropic_switch_test.rs"]
mod switch_test;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_and_unsigned_reasoning_stay_out() {
        let m = vec![json!({"role": "assistant", "content": [
            {"type": "reasoning", "text": "a", "sig": "S", "by": "claude"},
            {"type": "reasoning", "text": "b", "sig": "{}", "by": "codex"},
            {"type": "reasoning", "text": "c", "by": "claude"},
            {"type": "tool_call", "id": "t", "name": "READ", "input": {"path": "x"}},
        ]})];
        let out = messages(&m, "claude", true);
        assert_eq!(out[0]["content"], json!([
            {"type": "thinking", "thinking": "a", "signature": "S"},
            {"type": "tool_use", "id": "t", "name": "READ", "input": {"path": "x"}},
        ]));
        // llama.cpp takes its own unsigned thinking.
        let m = vec![json!({"role": "assistant", "content": [{"type": "reasoning", "text": "c", "by": "local"}]})];
        assert_eq!(messages(&m, "local", false)[0]["content"][0]["type"], "thinking");
    }

    #[test]
    fn instruction_follows_a_breakpoint() {
        let req = Request {
            model: "m".into(),
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "t", "name": "READ", "input": {}}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t", "content": "x"}]}),
            ],
            instruction: Some("Summarize".into()),
            ..Default::default()
        };
        let b = body(&req, "claude", true);
        let c = &b["messages"][2]["content"];
        assert_eq!(c[0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(c[1], json!({"type": "text", "text": "Summarize"}));
    }

    #[test]
    fn stream_events_become_oak_events() {
        let ev = json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"category": "cyber"}}, "usage": {"output_tokens": 9, "output_tokens_details": {"thinking_tokens": 4}}});
        let out = events(&ev, "claude");
        assert_eq!(out[0], Event::Usage(Usage { output: 9, reasoning: Some(4), ..Default::default() }));
        assert_eq!(out[1], Event::Stop { reason: "refusal".into(), detail: Some("category cyber".into()) });
        let ev = json!({"type": "content_block_start", "index": 2, "content_block": {"type": "thinking", "thinking": ""}});
        assert_eq!(events(&ev, "claude"), vec![Event::Block { index: 2, block: json!({"type": "reasoning", "text": "", "by": "claude"}) }]);
    }
}
