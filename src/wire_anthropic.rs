//! The Anthropic Messages wire format, for the providers that speak it (the
//! Claude API and llama.cpp's server): an Oak request as a Messages body,
//! and a Messages stream as Oak events.

use crate::oak::{self, Event, Request, Usage};
use serde_json::{Value, json};

/// An Oak block as a Messages block, or None when the provider `by` can't
/// take it (reasoning written by another provider: its `sig` means nothing
/// here; Claude also refuses a thought it didn't sign).
pub fn block(b: &Value, by: &str, signed: bool) -> Option<Value> {
    Some(match b["type"].as_str().unwrap_or("") {
        "text" => json!({"type": "text", "text": b["text"]}),
        "image" => json!({"type": "image", "source": {"type": "base64", "media_type": b["media_type"], "data": b["data"]}}),
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
        "tool_call" => json!({"type": "tool_use", "id": b["id"], "name": b["name"], "input": if b["input"].is_object() { b["input"].clone() } else { json!({}) }}),
        "tool_result" => {
            let content = match &b["content"] {
                Value::Array(a) => Value::Array(a.iter().filter_map(|x| block(x, by, signed)).collect()),
                Value::String(s) => json!(s),
                _ => json!(""),
            };
            let mut r = json!({"type": "tool_result", "tool_use_id": b["call_id"], "content": content});
            if let Some(e) = b["is_error"].as_bool() {
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
    let mut messages = messages(&req.messages, by, signed);
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
    body["thinking"] = if req.thinking { json!({"type": "adaptive", "display": "summarized"}) } else { json!({"type": "disabled"}) };
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
            messages: vec![json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t", "content": "x"}]})],
            instruction: Some("Summarize".into()),
            ..Default::default()
        };
        let b = body(&req, "claude", true);
        let c = &b["messages"][0]["content"];
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
