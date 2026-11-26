//! Conversations that moved between providers (AWS Quick, Codex, OpenRouter,
//! Antigravity) and back to Claude: what goes to the Messages API keeps
//! its rules however often the conversation switched, and requests still
//! only grow.

use super::*;
use std::collections::HashSet;

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn reply(blocks: Value) -> Value {
    json!({"role": "assistant", "content": blocks})
}

fn results(blocks: Value) -> Value {
    json!({"role": "user", "content": blocks})
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_call", "id": id, "name": name, "input": input})
}

fn result(id: &str, content: &str, is_error: bool) -> Value {
    oak::tool_result(&json!(id), json!(content), is_error)
}

fn req(messages: Vec<Value>) -> Request {
    Request { model: "claude-opus-4-6".into(), max_tokens: 32000, system: "You are Oak.".into(), messages, thinking: true, ..Default::default() }
}

fn fits(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// What the Messages API checks, as Oak has met it: no blank text, signed
/// thinking only, pictures of its types with data, call ids and names of
/// its pattern and each id once, every call answered in the next message
/// (past system messages) with the results first, and no result that
/// answers no call. A reply with calls may end the messages.
fn check(body: &Value) {
    let msgs = body["messages"].as_array().expect("messages");
    let mut ids = HashSet::new();
    let mut asked: Vec<String> = vec![];
    let blank = |b: &Value| b["type"] == "text" && b["text"].as_str().is_none_or(|t| t.trim().is_empty());
    for (i, m) in msgs.iter().enumerate() {
        let c = m["content"].as_array().unwrap_or_else(|| panic!("message {i} has no content: {m}"));
        assert!(!c.is_empty(), "message {i} is empty");
        for b in c {
            assert!(!blank(b), "blank text in message {i}: {m}");
            match b["type"].as_str().unwrap() {
                "thinking" => assert!(b["signature"].as_str().is_some_and(|s| !s.is_empty()), "unsigned thinking in {i}"),
                "image" => {
                    assert!(IMAGE_TYPES.contains(&b["source"]["media_type"].as_str().unwrap_or("")), "image type in {i}: {b}");
                    assert!(b["source"]["data"].as_str().is_some_and(|d| !d.is_empty()), "image without data in {i}");
                }
                "tool_use" => {
                    let id = b["id"].as_str().unwrap();
                    assert!(fits(id, 64), "call id {id:?} in {i}");
                    assert!(ids.insert(id.to_string()), "call id {id} twice");
                    assert!(fits(b["name"].as_str().unwrap(), 64), "tool name {} in {i}", b["name"]);
                    assert!(b["input"].is_object(), "input in {i}");
                }
                "tool_result" => {
                    if b["is_error"] == true {
                        assert!(b["content"] != json!("") && b["content"] != json!([]), "empty error in {i}");
                    }
                    for x in b["content"].as_array().into_iter().flatten() {
                        assert!(!blank(x), "blank text in a result in {i}");
                    }
                }
                _ => {}
            }
        }
        match m["role"].as_str().unwrap() {
            "system" => {}
            "assistant" => {
                assert!(asked.is_empty(), "calls {asked:?} before message {i} unanswered");
                asked = c.iter().filter(|b| b["type"] == "tool_use").map(|b| b["id"].as_str().unwrap().to_string()).collect();
            }
            _ => {
                let got: Vec<String> = c.iter().filter(|b| b["type"] == "tool_result").map(|b| b["tool_use_id"].as_str().unwrap().to_string()).collect();
                let lead = c.iter().take_while(|b| b["type"] == "tool_result").count();
                assert_eq!(lead, got.len(), "results not first in message {i}: {m}");
                let (mut a, mut g) = (asked.clone(), got);
                a.sort();
                g.sort();
                assert_eq!(a, g, "message {i} answers other calls than the reply before it asked");
                asked.clear();
            }
        }
    }
}

/// A Quick turn as the transcript keeps it, after `agent::for_provider`
/// left its snippet out: blank text around its calls, its own ids (which
/// it numbers from 0 every turn), a device tool's name, results.
fn quick_turn(n: usize, tag: &str) -> Vec<Value> {
    vec![
        reply(json!([
            {"type": "reasoning", "text": "plan", "by": "quick"},
            {"type": "text", "text": "\n\n"},
            call("call_0", "Oak-DESKTOP.VE88_file_read", json!(format!("{{\"path\": \"{tag}{n}\"}}"))),
            {"type": "text", "text": " "},
        ])),
        results(json!([result("call_0", "", true)])),
        reply(json!([{"type": "text", "text": format!("Quick {tag}{n} done.")}, {"type": "text", "text": ""}])),
    ]
}

/// A Claude turn: signed thinking, a call, its result, a reply.
fn claude_turn(n: usize, tag: &str) -> Vec<Value> {
    let id = format!("toolu_{tag}{n}");
    vec![
        reply(json!([
            {"type": "reasoning", "text": "think", "sig": "SIG", "by": "claude"},
            call(&id, "READ", json!({"path": "x"})),
        ])),
        results(json!([result(&id, "ok", false)])),
        reply(json!([{"type": "text", "text": "Claude done."}])),
    ]
}

#[test]
fn quick_turns_go_to_claude_as_it_takes_them() {
    let mut m = vec![user("hi")];
    m.extend(claude_turn(0, "a"));
    m.push(user("now on Quick"));
    m.extend(quick_turn(0, "q"));
    m.push(user("back on Claude"));
    let b = body(&req(m), "claude", true);
    check(&b);
    let msgs = b["messages"].as_array().unwrap();
    // Quick's reasoning stays out, its JSON-text input is read, its device
    // tool's name fits, its empty error says something.
    let quick = &msgs[5]["content"];
    assert_eq!(quick.as_array().unwrap().len(), 1, "only the call: {quick}");
    assert_eq!(quick[0]["name"], "Oak-DESKTOP_VE88_file_read");
    assert_eq!(quick[0]["input"], json!({"path": "q0"}));
    assert_eq!(msgs[6]["content"][0]["content"], EMPTY_ERROR);
    assert_eq!(msgs[7]["content"], json!([{"type": "text", "text": "Quick q0 done."}]));
}

#[test]
fn repeated_switches_stay_valid_and_only_grow() {
    // Claude, Quick, Claude, Quick, Claude, Quick, Claude: each Claude
    // request keeps the rules and starts with the one before it.
    let mut m = vec![user("start")];
    let mut last: Option<Vec<Value>> = None;
    for k in 0..7 {
        if k % 2 == 0 {
            m.extend(claude_turn(k, "c"));
        } else {
            m.extend(quick_turn(k, "q"));
        }
        m.push(user(&format!("turn {k}")));
        if k % 2 == 0 || k == 6 {
            let b = body(&req(m.clone()), "claude", true);
            check(&b);
            let now = b["messages"].as_array().unwrap().clone();
            if let Some(before) = &last {
                assert_eq!(&now[..before.len()], &before[..], "request {k} changed what an earlier one sent");
            }
            last = Some(now);
        }
    }
    // Every Quick turn's call_0 went with an id of its own.
    let b = body(&req(m), "claude", true);
    let ids: Vec<&str> = b["messages"].as_array().unwrap().iter().flat_map(|m| m["content"].as_array().unwrap().iter()).filter(|b| b["type"] == "tool_use").filter_map(|b| b["id"].as_str()).collect();
    assert_eq!(ids.iter().filter(|i| i.starts_with("call_0")).count(), 3, "{ids:?}");
}

#[test]
fn a_call_left_unanswered_gets_a_result() {
    // A Quick reply cut short, then Claude's reply right after it (a
    // fallback mid-turn), then one whose user turn came without results.
    let m = vec![
        user("go"),
        reply(json!([call("call_0", "READ", json!({}))])),
        reply(json!([{"type": "text", "text": "Taking over."}, call("toolu_1", "EXECUTE", json!({}))])),
        user("stop that"),
    ];
    let b = body(&req(m), "claude", true);
    check(&b);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4, "the messages keep their places");
    assert_eq!(msgs[1]["content"][0]["type"], "text");
    assert!(msgs[1]["content"][0]["text"].as_str().unwrap().starts_with("(Called READ with {}"));
    assert_eq!(msgs[3]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(msgs[3]["content"][0]["content"], UNANSWERED);
    assert_eq!(msgs[3]["content"][1], json!({"type": "text", "text": "stop that"}));
    // A reply with calls that ends the messages waits for its results.
    let pending = vec![user("go"), reply(json!([call("toolu_2", "READ", json!({}))]))];
    let b = body(&req(pending), "claude", true);
    assert_eq!(b["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn a_result_without_its_call_goes_as_text_and_results_go_first() {
    let m = vec![
        user("go"),
        reply(json!([call("toolu_1", "READ", json!({}))])),
        results(json!([
            {"type": "text", "text": "(a message the user sent mid-turn)"},
            result("toolu_1", "ok", false),
            result("snippet_9", "what the snippet printed", false),
        ])),
        results(json!([result("toolu_1", "again", false)])),
    ];
    let b = body(&req(m), "claude", true);
    check(&b);
    let c = &b["messages"][2]["content"];
    assert_eq!(c[0]["tool_use_id"], "toolu_1");
    assert_eq!(c[1]["text"], "(a message the user sent mid-turn)");
    assert!(c[2]["text"].as_str().unwrap().contains("snippet_9"));
    assert_eq!(c[3]["text"], "what the snippet printed");
    assert_eq!(b["messages"][3]["content"][1]["text"], "again");
}

#[test]
fn pictures_and_documents_claude_cant_take_go_as_text() {
    let m = vec![
        json!({"role": "user", "content": [
            {"type": "image", "media_type": "image/bmp", "data": "Qk0="},
            {"type": "image", "media_type": "image/png", "data": ""},
            {"type": "image", "media_type": "image/png", "data": "iVBORw0KGgo="},
            {"type": "document", "media_type": "application/msword", "data": "AAAA"},
            {"type": "text", "text": null},
        ]}),
    ];
    let b = body(&req(m), "claude", true);
    check(&b);
    let c = b["messages"][0]["content"].as_array().unwrap();
    assert_eq!(c.len(), 4);
    assert!(c[0]["text"].as_str().unwrap().contains("image/bmp"));
    assert_eq!(c[1]["type"], "text");
    assert_eq!(c[2]["type"], "image");
    assert!(c[3]["text"].as_str().unwrap().contains("application/msword"));
}

#[test]
fn messages_that_keep_the_rules_are_left_as_they_are() {
    let mut m = vec![user("hi")];
    m.extend(claude_turn(0, "a"));
    m.push(user("more"));
    let converted = messages(&m, "claude", true);
    let mut paired = converted.clone();
    pair_calls(&mut paired);
    assert_eq!(paired, converted);
}

/// Quick and Claude take turns within one turn of the user's (switches
/// mid-turn): while the reply a request continues is another provider's,
/// thinking is off for Claude (it would refuse the request); once Claude's
/// own reply, which begins with thinking, is the one continued, or a new
/// turn starts, it is on again. The context edit that needs thinking goes
/// with it.
#[test]
fn a_turn_begun_elsewhere_goes_on_without_thinking() {
    let think = |id: &str| reply(json!([
        {"type": "reasoning", "text": "t", "sig": "SIG", "by": "claude"},
        call(id, "READ", json!({})),
    ]));
    let quick = |id: &str| reply(json!([{"type": "reasoning", "text": "q", "by": "quick"}, {"type": "text", "text": "Reading."}, call(id, "READ", json!({}))]));
    let done = |id: &str| results(json!([result(id, "ok", false)]));
    let thinking_of = |m: &[Value]| {
        let b = crate::api::claude_body(&req(m.to_vec()));
        check(&b);
        let on = b["thinking"]["type"] != "disabled";
        assert_eq!(b.get("context_management").is_some(), on, "the context edit goes with thinking");
        on
    };
    let mut m = vec![user("go")];
    for k in 0..3 {
        // Quick's reply, then Claude continues it: off.
        m.push(quick(&format!("call_{k}")));
        m.push(done(&format!("call_{k}")));
        assert!(!thinking_of(&m), "Claude continuing Quick's reply {k}");
        // Claude's reply that didn't think, continued: its own, so on.
        m.push(reply(json!([call(&format!("toolu_n{k}"), "READ", json!({}))])));
        m.push(done(&format!("toolu_n{k}")));
        assert!(thinking_of(&m), "Claude's own thinking-less reply {k}");
        // Quick again, then Claude's thinking-less reply on top: on (the
        // continued reply is Claude's).
        m.push(quick(&format!("again_{k}")));
        m.push(done(&format!("again_{k}")));
        assert!(!thinking_of(&m), "Quick again {k}");
        m.push(reply(json!([call(&format!("toolu_m{k}"), "READ", json!({}))])));
        m.push(done(&format!("toolu_m{k}")));
        assert!(thinking_of(&m), "Claude after Quick {k}");
        // A Claude reply that thought: on.
        m.push(think(&format!("toolu_t{k}")));
        m.push(done(&format!("toolu_t{k}")));
        assert!(thinking_of(&m), "Claude continuing its own thinking reply {k}");
        // A Quick reply cut short mid-call, the user writing again: its
        // call is answered there (`pair_calls`), so that turn continues
        // Quick's reply: off.
        m.push(quick(&format!("cut_{k}")));
        m.push(user(&format!("next {k}")));
        assert!(!thinking_of(&m), "a cut Quick reply, answered {k}");
        // Quick's plain answer, then the user: a new turn, on.
        m.push(reply(json!([{"type": "text", "text": "Done.", "_oak_by": "quick"}])));
        m.push(user(&format!("then {k}")));
        assert!(thinking_of(&m), "a new turn {k}");
    }
    // Results with a mid-turn message beside them still continue the turn.
    let mixed = vec![user("go"), quick("c"), results(json!([result("c", "ok", false), {"type": "text", "text": "also this"}]))];
    assert!(!thinking_of(&mixed));
    // Codex's calls (its ids), Claude's unsigned thinking, Antigravity's
    // Claude (its signature is Google's): off. A reply with nothing to tell
    // by: Claude's, on.
    let codex = reply(json!([call("call_abc", "READ", json!({}))]));
    assert!(!thinking_of(&[user("go"), codex, done("call_abc")]));
    let unsigned = reply(json!([{"type": "reasoning", "text": "t", "by": "claude"}, call("toolu_u", "READ", json!({}))]));
    assert!(!thinking_of(&[user("go"), unsigned, done("toolu_u")]));
    let gravity = reply(json!([{"type": "reasoning", "text": "t", "sig": "G", "by": "antigravity"}, call("toolu_g", "READ", json!({}))]));
    assert!(!thinking_of(&[user("go"), gravity, done("toolu_g")]));
    // Thinking asked off stays off; other providers' bodies are as asked.
    let mut off = req(vec![user("go"), think("a"), done("a")]);
    off.thinking = false;
    assert_eq!(crate::api::claude_body(&off)["thinking"]["type"], "disabled");
    let mut r = req(vec![user("go"), quick("c"), done("c")]);
    assert_eq!(body(&r, "openrouter", false)["thinking"]["type"], "adaptive");
    r.thinking = false;
    assert_eq!(crate::api::claude_body(&r)["thinking"]["type"], "disabled");
}

/// A long turn on Claude alone, its replies thinking or not as adaptive
/// thinking has it: thinking and the context edit never change, so the
/// cache holds.
#[test]
fn a_turn_on_claude_alone_never_toggles_thinking() {
    let mut m = vec![user("go")];
    let mut seen = vec![];
    for k in 0..8 {
        let id = format!("toolu_{k}");
        let blocks = if k % 3 == 0 {
            json!([{"type": "reasoning", "text": "t", "sig": "SIG", "by": "claude"}, call(&id, "READ", json!({}))])
        } else if k % 3 == 1 {
            json!([call(&id, "READ", json!({}))])
        } else {
            json!([{"type": "text", "text": "Next."}, call(&id, "EDIT", json!({}))])
        };
        m.push(reply(blocks));
        m.push(results(json!([result(&id, "ok", false)])));
        let b = crate::api::claude_body(&req(m.clone()));
        check(&b);
        seen.push((b["thinking"].clone(), b["context_management"].clone()));
    }
    assert!(seen.iter().all(|s| s.0["type"] == "adaptive" && s.1.is_object()), "{seen:?}");
}

#[test]
fn claude_body_puts_its_breakpoint_on_the_reordered_turn() {
    let m = vec![
        user("go"),
        reply(json!([call("call_0", "READ", json!({}))])),
        user("never mind"),
    ];
    let b = crate::api::claude_body(&req(m));
    check(&b);
    let last = b["messages"][2]["content"].as_array().unwrap();
    assert_eq!(last[0]["type"], "tool_result");
    assert!(last[1].get("cache_control").is_some(), "{last:?}");
}
