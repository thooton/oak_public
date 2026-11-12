//! `oak --measure-thinking [conversation id…]`: how much of a Claude
//! conversation's prompt the thinking of earlier turns can be. Opus 5.5
//! keeps it in context and bills it as input, but only while everything
//! before a block is as it was when the block was written: an edit before
//! it (a new system prompt, a summary, cleared tool output) or another
//! account drops it (the API's prefix and account checks; `--probe` shows
//! both). So this is the upper bound: the thinking tokens each reply
//! reported, of the replies in the current view, against the last prompt.
//! The usage log's `think_dropped` says what was dropped in fact.
//!
//! `--probe [model] [--tool]`: one real reply with thinking, then the next
//! request counted and billed with and without its thinking, and with the
//! system prompt edited. `--probe-prefix`: the stable prefix's request
//! shapes and a keep-alive ping, live.

use crate::tools::Head;
use serde_json::{Value, json};


pub fn main(ids: &[String]) -> i32 {
    if !crate::api::load_auth() {
        eprintln!("No Claude account is signed in.");
        return 1;
    }
    if ids.first().is_some_and(|a| a == "--probe") {
        let model = ids.get(1).map_or("claude-opus-5-5", String::as_str);
        let tool = ids.iter().any(|a| a == "--tool");
        return match crate::api::probe_thinking(model, tool) {
            Ok(s) => {
                println!("{s}");
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
    }
    if ids.first().is_some_and(|a| a == "--probe-prefix") {
        let codex = (ids.get(1).map(String::as_str) == Some("codex")).then(|| ids.get(2).cloned().unwrap_or_else(|| "gpt-6.1-terra".into()));
        return probe_prefix(codex);
    }
    let dir = crate::store::conversations_dir();
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .map(|r| r.flatten().map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path())).collect())
        .unwrap_or_default();
    files.retain(|(_, p)| p.extension().is_some_and(|x| x == "json"));
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let (mut done, mut all, mut thinking) = (0, 0u64, 0u64);
    for (_, path) in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(mut c) = serde_json::from_str::<crate::store::Conversation>(&text) else { continue };
        if !c.model.starts_with("claude-") || c.rolling.context == 0 || (!ids.is_empty() && !ids.contains(&c.id)) {
            continue;
        }
        c.upgrade();
        // The thinking tokens each reply reported, of the replies the
        // current view still has.
        let base = c.rolling.base().min(c.messages.len());
        let blocks = c.messages[base..].iter().filter(|m| m["role"] == "assistant").flat_map(|m| m["content"].as_array().into_iter().flatten());
        let t: u64 = blocks.filter(|b| crate::oak::is_reasoning(b)).filter_map(|b| b["_oak_tokens"].as_u64()).sum();
        let p = c.rolling.context;
        println!("{}  {:<40}  prompt {:>7}, earlier thinking up to {:>6} ({:.1}%)", c.id, c.title.head(40), p, t, 100.0 * t as f64 / p as f64);
        all += p;
        thinking += t;
        done += 1;
        if done >= if ids.is_empty() { 8 } else { ids.len() } {
            break;
        }
    }
    if all > 0 {
        println!("all: earlier thinking up to {:.1}% of the prompts (all of it while nothing before it changed; none after an edit before it)", 100.0 * thinking as f64 / all as f64);
    }
    0
}

/// `oak --measure-thinking --probe-prefix [codex [model]]`: the stable
/// prefix's request shapes through the real path (`api::stream_messages`),
/// as `crate::prefix` sends them: one conversation whose requests add, in
/// turn, a system message with text, one that redefines a tool, one that
/// adds a new tool (which the model is asked to call) with a new effort,
/// one with an effort only, and last an edited system prompt. Each line
/// shows what the request read from and wrote to the cache (and, for
/// Claude, the API's cache diagnostics from usage.jsonl). Codex gets
/// developer messages and `additional_tools` items.
fn probe_prefix(codex: Option<String>) -> i32 {
    use crate::prefix::Mode;
    let mode = if codex.is_some() { Mode::Codex } else { Mode::Claude };
    let conv = format!("probe-prefix-{}", crate::api::now_ms());
    let user = |t: &str| json!({"role": "user", "content": [{"type": "text", "text": t}]});
    let tool = |name: &str, d: &str| json!({"name": name, "description": d, "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]}});
    let sys = |content: Value, effort: Option<&str>| {
        let mut m = json!({"role": "system", "content": content});
        if let Some(e) = effort {
            m["effort"] = json!(e);
        }
        m
    };
    // Enough to be cached (Opus needs 4096 tokens or more).
    let filler: String = (0..900).map(|i| format!("Rule {i}: answer briefly. ")).collect();
    let mut req = crate::oak::Request {
        provider: if codex.is_some() { crate::api::Provider::Codex } else { crate::api::Provider::Claude },
        model: codex.clone().unwrap_or_else(|| "claude-opus-5-5".into()),
        max_tokens: 2000,
        system: format!("You are a test. {filler}"),
        messages: vec![user("Say hi in two words.")],
        tools: vec![tool("lookup", "Looks things up."), json!({"name": "noop", "description": "Does nothing.", "input_schema": {"type": "object", "properties": {}}})],
        thinking: true,
        effort: Some("low".into()),
        conv: Some(conv.clone()),
        ttl: crate::oak::Ttl::FiveMinutes,
        ..Default::default()
    };
    let effort = |e: &'static str| (mode == Mode::Claude).then_some(e);
    let steps: Vec<(&str, Option<Value>)> = vec![
        ("plain", None),
        ("+ system text", Some(sys(json!([{"type": "text", "text": "Today's date is now 2026-11-07."}]), None))),
        ("+ tool redefined", Some(sys(json!([
            {"type": "tool_definition", "definition": tool("lookup", "Looks things up, now in two places.")},
            {"type": "text", "text": "The lookup tool changed: its new definition replaces the one before."}]), None))),
        ("+ new tool and effort (call it)", Some(sys(json!([{"type": "tool_definition", "definition": tool("weather", "The weather in city `q`.")}]), effort("medium")))),
        ("+ effort only", (mode == Mode::Claude).then(|| sys(json!([]), Some("low")))),
        ("system prompt edited (should miss)", None),
    ];
    for (name, sys) in steps {
        if name.starts_with("+ new tool") {
            // The question goes before the message, which must come right
            // before a reply.
            let last = req.messages.last_mut().and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut());
            if let Some(b) = last {
                b["text"] = json!("Call the weather tool for Paris, nothing else.");
            }
        }
        match sys {
            Some(s) => req.messages.push(s),
            None if name.starts_with("system prompt") => req.system = format!("You are a test, edited. {filler}"),
            None if name != "plain" => continue,
            None => {}
        }
        let (tx, rx) = std::sync::mpsc::channel();
        crate::api::stream_messages(req.clone(), tx);
        let mut events = vec![];
        let mut err = None;
        for s in rx {
            match s {
                crate::api::Sse::Event(e) => events.push(e),
                crate::api::Sse::Err(e) => err = Some(e),
                _ => {}
            }
        }
        if let Some(e) = err {
            println!("{name}: error {e}");
            return 1;
        }
        let got = crate::oak::collect(&events);
        let u = events.iter().rev().find_map(|e| if let crate::oak::Event::Usage(u) = e { Some(*u) } else { None }).unwrap_or_default();
        let line = std::fs::read_to_string(crate::store::dir().join("usage.jsonl")).ok().and_then(|t| t.lines().rev().find(|l| l.contains(&conv)).map(str::to_string)).unwrap_or_default();
        let r: Value = serde_json::from_str(&line).unwrap_or_default();
        let blocks = tool_calls(&events);
        let calls: Vec<&str> = blocks.iter().filter_map(|b| b["name"].as_str()).collect();
        println!(
            "{name}: reply {:?} calls {:?}; read {} write {} input {}; miss {} {}; still on: prefix {} codex items {}",
            got.text.head(40),
            calls, u.cache_read, u.write, u.input, r["miss"], r["missed"], crate::api::stable_prefix(), crate::prefix::codex_native()
        );
        if name == "+ tool redefined" && mode == Mode::Claude {
            // The keep-alive ping of that request: reads it all, writes nothing.
            match crate::api::keep_warm(&req) {
                Ok(u) => println!("keep-alive ping: read {} write {} input {} output {}", u.cache_read, u.write, u.input, u.output),
                Err(e) => println!("keep-alive ping: error {e}"),
            }
        }
        // The reply, its calls answered.
        let mut content = vec![json!({"type": "text", "text": if got.text.is_empty() { "Hi there." } else { got.text.as_str() }})];
        content.extend(blocks.iter().cloned());
        let results: Vec<Value> = content.iter().filter(|b| b["type"] == "tool_call").map(|b| json!({"type": "tool_result", "call_id": b["id"], "content": "Sunny, 20 C."})).collect();
        req.messages.push(json!({"role": "assistant", "content": content}));
        let mut next = results;
        next.push(json!({"type": "text", "text": "Again, two words."}));
        req.messages.push(json!({"role": "user", "content": next}));
    }
    0
}

/// The tool calls a reply's events make, with their input.
fn tool_calls(events: &[crate::oak::Event]) -> Vec<Value> {
    use crate::oak::Event;
    let mut calls: Vec<(usize, Value, String)> = vec![];
    for e in events {
        match e {
            Event::Block { index, block } if block["type"] == "tool_call" => calls.push((*index, block.clone(), String::new())),
            Event::Input { index, json } => {
                if let Some(c) = calls.iter_mut().find(|c| c.0 == *index) {
                    c.2.push_str(json);
                }
            }
            _ => {}
        }
    }
    calls
        .into_iter()
        .map(|(_, mut b, input)| {
            b["input"] = serde_json::from_str(&input).unwrap_or(json!({}));
            b
        })
        .collect()
}
