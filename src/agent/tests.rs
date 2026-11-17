//! Agent loop regression and live smart-context tests.

use memory::*;
use tool_calls::*;
use super::*;
use super::reply::spread_thinking_tokens;

#[test]
fn clearing_a_goal_waits_for_the_next_request() {
    let mut c = Conversation::default();
    apply_command(&mut c, Command::Goal("ship it".into(), false));
    let (block, mark) = apply_command(&mut c, Command::Goal(String::new(), false));
    assert!(block.is_none(), "a cleared goal gives the model nothing to do");
    assert_eq!(mark.0, MarkKind::Goal);
    assert!(c.held[0].contains("cleared the goal"));
    // A new goal supersedes the waiting note.
    let (block, _) = apply_command(&mut c, Command::Goal("again".into(), false));
    assert!(block.is_some() && c.held.is_empty());
}

/// A reply's thinking-token count goes on its thinking blocks: all of it
/// on one, shared by length over several (adding up), none for a 0.
#[test]
fn thinking_tokens_spread_over_blocks() {
    let mut one = vec![json!({"type": "reasoning", "text": "plan"}), json!({"type": "text", "text": "hi"})];
    spread_thinking_tokens(&mut one, 1234);
    assert_eq!(one[0]["_oak_tokens"], 1234);
    assert!(one[1].get("_oak_tokens").is_none());

    let mut two = vec![
        json!({"type": "reasoning", "text": "aaa"}),
        json!({"type": "server_tool_use"}),
        json!({"type": "reasoning", "text": "a"}),
        json!({"type": "reasoning", "text": "", "sig": "x", "redacted": true}),
    ];
    spread_thinking_tokens(&mut two, 101);
    let n: Vec<u64> = [0, 2, 3].iter().map(|&i| two[i]["_oak_tokens"].as_u64().unwrap()).collect();
    assert_eq!(n, [50, 16, 35], "the redacted block weighs as the visible ones on average");
    assert_eq!(n.iter().sum::<u64>(), 101);
}

/// Switching models: reasoning another provider wrote is left out of
/// requests (kept for a model of the same provider); the OpenRouter
/// provider is forgotten; the effort stays.
#[test]
fn switching_models() {
    let think = |by: &str| json!({"type": "reasoning", "text": "hm", "sig": "s", "by": by});
    let mut c = Conversation {
        model: "claude-opus-5-5".into(),
        effort: "max".into(),
        served_by: "Morph".into(),
        messages: vec![
            json!({"role": "user", "content": [oak::text("hi")]}),
            json!({"role": "assistant", "content": [think("claude"), {"type": "tool_call", "id": "t1", "name": "READ", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "x"}]}),
            json!({"role": "assistant", "content": [{"type": "reasoning", "text": "", "sig": "r", "by": "claude", "redacted": true}]}),
        ].into(),
        ..Default::default()
    };
    // Its own provider's: everything goes as it is, borrowed.
    assert!(matches!(for_provider(&c.messages, api::Provider::Claude), std::borrow::Cow::Borrowed(_)));
    let (block, mark) = apply_command(&mut c, Command::Model(api::Provider::Codex, "gpt-6-luna".into()));
    assert!(block.is_none());
    assert_eq!((mark.0, c.model.as_str(), c.effort.as_str(), c.served_by.as_str()), (MarkKind::Model, "gpt-6-luna", "max", ""));
    c.messages.push(json!({"role": "user", "content": [oak::text("more")]}));
    c.messages.push(json!({"role": "assistant", "content": [think("codex"), oak::text("ok")]}));
    let sent = for_provider(&c.messages, api::Provider::Codex);
    assert_eq!(sent[1]["content"], json!([{"type": "tool_call", "id": "t1", "name": "READ", "input": {}}]), "Claude's thinking left out for Codex");
    assert_eq!(sent[3]["content"], json!([oak::text("(No reply text.)")]), "a reply of thinking only keeps a line");
    assert_eq!(sent[5]["content"][0]["type"], "reasoning", "its own thinking stays");
    // Switching to the model it is on changes nothing.
    let before = c.messages.clone();
    apply_command(&mut c, Command::Model(api::Provider::Codex, "gpt-6-luna".into()));
    assert_eq!(c.messages, before);
}

/// Messages between sessions carry who sent them, and whether it is a
/// final reply.
#[test]
fn agent_messages_round_trip() {
    let m = agent_message(&agent_label(2, "Look \"here\""), true, "  done.\n");
    assert_eq!(m, "<agent-message from=\"agent 2 (Look 'here')\" type=\"final reply\">\ndone.\n</agent-message>");
    assert_eq!(parse_agent_message(&m), Some(("agent 2 (Look 'here')", true, "done.")));
    assert_eq!(parse_agent_message(&agent_message(MAIN_LABEL, false, "go")), Some(("main session", false, "go")));
    assert_eq!(parse_agent_message("<agent-messages>"), None);
}

/// `conv` as a `unit-test-{name}` conversation, in the temp folder unless
/// it has one.
fn unit_conv(name: &str, conv: Conversation) -> Conversation {
    let folder = if conv.folder.as_os_str().is_empty() { std::env::temp_dir() } else { conv.folder };
    Conversation { id: format!("unit-test-{name}-{}", api::now_ms()), folder, ..conv }
}

/// A test's session, deleted (unregistered, its tasks killed and its
/// conversation removed) when dropped.
struct Deleting(Session);

impl std::ops::Deref for Deleting {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.0
    }
}

impl Drop for Deleting {
    fn drop(&mut self) {
        // (A failed test may leave its locks poisoned.)
        if !std::thread::panicking() {
            self.0.delete();
        }
    }
}

/// A worker that isn't running on `conv` made a `unit-test-{name}` one,
/// for calling its tools directly, and a session sharing its state,
/// registered so other sessions reach it.
fn idle_worker(name: &str, conv: Conversation) -> (Worker, Deleting) {
    let conv = unit_conv(name, conv);
    let tasks = Tasks::open(&conv.id);
    let (s, w) = Session::with_worker(conv, eframe::egui::Context::default(), false, tasks);
    SESSIONS.lock().unwrap().insert(s.conv.lock().unwrap().id.clone(), s.clone());
    (w, Deleting(s))
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(15), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The pause button during a request queues the pause, which starts as
/// the request ends (the reply kept); a second press pauses at once;
/// with no request under way it pauses at once.
#[test]
fn pause_waits_for_the_request() {
    let (w, s) = idle_worker("queued-pause", Conversation::default());
    let paused = || s.paused.load(Ordering::SeqCst) && s.conv.lock().unwrap().paused;
    // Mid-request: queued, not paused yet.
    w.set(|l| l.requesting = true);
    assert!(s.request_pause());
    assert!(s.pause_queued() && !s.paused.load(Ordering::SeqCst));
    // Saved, a queued pause holds if OAK closes before it takes hold.
    let mut saved = s.conv.lock().unwrap().clone();
    assert!(saved.pause_queued && !saved.paused);
    assert!(hold_queued_pause(&mut saved) && saved.paused && !saved.pause_queued);
    // The request ends: paused now, and the queue is done with.
    w.end_request();
    assert!(paused() && !s.pause_queued() && !s.conv.lock().unwrap().pause_queued);
    // Resumed, and queued again, the second press pauses at once.
    s.set_paused(false);
    w.set(|l| l.requesting = true);
    assert!(s.request_pause());
    s.set_paused(true);
    assert!(paused() && !s.pause_queued());
    // The request ending later doesn't pause again after a resume.
    s.set_paused(false);
    w.end_request();
    assert!(!s.paused.load(Ordering::SeqCst));
    // Between requests (a tool running, idle): paused at once.
    assert!(!s.request_pause());
    assert!(paused() && !s.pause_queued());
}

/// A /goal reaches the model as the goal prompt; ending a turn with
/// it unfinished brings the continue prompt; GOAL_JUSTIFICATION then
/// GOAL_FINISH end it when the turn ends, and its line keeps the
/// justification.
/// An endless goal can't be justified or finished: OAK keeps asking the
/// model to continue until the user pauses or clears it.
#[test]
fn endless_goal_never_finishes() {
    let goal = "Keep improving the docs.";
    let mut c = Conversation::default();
    let tool = |c: &mut Conversation, name: &str, input: Value| memory_tool(c, name, &input, 0);
    let (block, mark) = apply_command(&mut c, Command::Goal(goal.into(), true));
    let text = block.unwrap()["text"].as_str().unwrap().to_string();
    assert!(text.starts_with(ENDLESS_GOAL_TAG) && text == goal_prompt(goal, true));
    assert_eq!(mark, (MarkKind::Goal, goal.into()));
    assert!(c.goal_endless && render_memory(&c).contains("as endless"));
    assert!(tool(&mut c, "GOAL_JUSTIFICATION", json!({"justification": "done"})).is_err());
    assert!(tool(&mut c, "GOAL_FINISH", json!({})).is_err());
    assert!(c.goal_justification.is_empty() && !c.goal_finished);
    let more = after_turn(&mut c, false, 5).expect("asked to continue");
    assert_eq!(more["text"], goal_continue(goal, true));
    assert_eq!(c.goal, goal);
    // Set again without the mode: it can be finished.
    apply_command(&mut c, Command::Goal(goal.into(), false));
    assert!(!c.goal_endless && tool(&mut c, "GOAL_JUSTIFICATION", json!({"justification": "done"})).is_ok());
    // Clearing ends the mode with the goal.
    apply_command(&mut c, Command::Goal(goal.into(), true));
    apply_command(&mut c, Command::Goal(String::new(), true));
    assert!(c.goal.is_empty() && !c.goal_endless);
}

#[test]
fn goal_runs_until_finished() {
    let goal = "Do X, Y, and then Z.";
    let mut c = Conversation::default();
    let tool = |c: &mut Conversation, name: &str, input: Value| memory_tool(c, name, &input, 0);
    // No goal: nothing to justify or finish, and turns end as usual.
    assert!(tool(&mut c, "GOAL_JUSTIFICATION", json!({"justification": "x"})).is_err());
    assert!(tool(&mut c, "GOAL_FINISH", json!({})).is_err());
    assert_eq!(after_turn(&mut c, false, 0), None);

    let (block, _) = apply_command(&mut c, Command::Goal(goal.into(), false));
    assert_eq!(block.unwrap()["text"], goal_prompt(goal, false));
    // Ending the turn without finishing: asked to carry on, not when interrupted.
    let more = after_turn(&mut c, false, 5).expect("asked to continue");
    assert_eq!(more["text"], goal_continue(goal, false));
    assert!(is_hidden_block(&more));
    assert_eq!(c.marks.last().map(|m| m.kind), Some(MarkKind::GoalContinued));
    assert_eq!(after_turn(&mut c, true, 6), None);
    // Paused, the goal stays but nothing is sent or marked.
    c.goal_paused = true;
    let (marks, memory) = (c.marks.len(), render_memory(&c));
    assert_eq!(after_turn(&mut c, false, 6), None);
    assert_eq!((c.marks.len(), c.goal.as_str()), (marks, goal));
    assert_eq!(render_memory(&c), memory, "the model isn't told");
    c.goal_paused = false;
    // Finishing needs a justification first.
    assert_eq!(tool(&mut c, "GOAL_FINISH", json!({})), Err("Call GOAL action=justify first, with how you have completed the user's goal.".into()));
    assert!(tool(&mut c, "GOAL_JUSTIFICATION", json!({"justification": "Did X, Y and Z."})).is_ok());
    assert_eq!(tool(&mut c, "GOAL_FINISH", json!({})), Ok("The goal is finished. Control goes back to the user when you end your turn.".into()));
    assert_eq!(after_turn(&mut c, false, 7), None);
    let m = c.marks.last().unwrap();
    assert_eq!((m.kind, m.text.as_str()), (MarkKind::GoalFinished, "Did X, Y and Z."));
    assert!(c.goal.is_empty() && c.goal_justification.is_empty() && !c.goal_finished);
    // The marker key never reaches the API.
    let sent = crate::rolling::view(&[json!({"role": "user", "content": [more]})], None);
    assert!(!sent[0]["content"][0].as_object().unwrap().contains_key("_oak_goal"));
}

/// Kits and notes (Claude): LOAD adds AGENT's tool in a system message,
/// the prompt and tools as they were. A cold cache changes nothing; only a
/// summary that covers the LOAD brings the kit into the prefix (and the
/// note into the session memory, its call's text folded).
#[test]
fn kits_and_notes_wait_for_a_summary() {
    let (w, s) = idle_worker("kits", Conversation { model: "claude-opus-5-5".into(), effort: "high".into(), ..Default::default() });
    let names = |r: &oak::Request| r.tools.iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r1, ..) = w.request_body();
    assert!(names(&r1).contains(&"LOAD".to_string()) && !names(&r1).contains(&"AGENT".to_string()) && !names(&r1).contains(&"BROWSER".to_string()));
    assert!(!r1.system.contains("Subagents (AGENT) suit"));
    // Not loaded: AGENT's calls say how to get it.
    assert!(w.unloaded("AGENT_LIST").unwrap().contains("LOAD with name=AGENT"));
    // Answered, so the cache is warm.
    {
        let mut c = s.conv.lock().unwrap();
        let choice = crate::cache_ttl::Choice { ttl: oak::Ttl::FiveMinutes, anchor: None };
        c.cache.record(api::now_ms(), choice, 20_000, 1, 0);
        c.messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_call", "id": "l1", "name": "LOAD", "input": {"name": "agent"}},
            {"type": "tool_call", "id": "n1", "name": "NOTE", "input": {"action": "new", "text": "the db is postgres"}}]}));
    }
    let calls: Vec<Value> = s.conv.lock().unwrap().messages[1]["content"].as_array().unwrap().clone();
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    let loaded = s.conv.lock().unwrap().messages[2]["content"][0]["content"].as_str().unwrap_or("").to_string();
    assert!(loaded.starts_with("Loaded AGENT. The AGENT tool is in your tool list now.") && loaded.contains("Subagents (AGENT) suit"), "{loaded}");
    assert!(w.unloaded("AGENT_LIST").is_none());
    let (r2, ..) = w.request_body();
    assert_eq!((r2.system.clone(), r2.tools.clone()), (r1.system.clone(), r1.tools.clone()), "the prefix as cached");
    let last = r2.messages.last().unwrap();
    assert_eq!(last["role"], "system");
    assert_eq!(last["content"][0]["type"], "tool_definition");
    assert_eq!(last["content"][0]["definition"]["name"], "AGENT");
    assert!(!r2.system.contains("the db is postgres"), "the note waits");
    // Effort changed: per message, the request's own stays.
    s.conv.lock().unwrap().effort = "low".into();
    let (r3, ..) = w.request_body();
    assert_eq!((r3.effort.as_deref(), r3.messages.last().unwrap()["effort"].as_str()), (Some("high"), Some("low")));
    // Cold (expired, another account), and old tool output: nothing sent
    // changes, nothing is cleared or folded, the effort stays per message.
    {
        let mut c = s.conv.lock().unwrap();
        let big = "x".repeat(200_000);
        c.messages.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "cat big"}}]}));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "e1", "content": big}]}));
        c.messages.push(json!({"role": "assistant", "content": [oak::text("done")]}));
        c.messages.push(json!({"role": "user", "content": [oak::text("next")]}));
        c.cache.cold();
    }
    let (r4, ..) = w.request_body();
    assert_eq!((&r4.system, &r4.tools, r4.effort.as_deref()), (&r1.system, &r1.tools, Some("high")));
    assert_eq!(&r4.messages[..r3.messages.len()], &r3.messages[..]);
    let flat = serde_json::to_string(&r4.messages).unwrap();
    assert!(flat.contains("the db is postgres") && !flat.contains(crate::prefix::NOTE_FOLDED) && flat.contains(&"x".repeat(200_000)));
    // A summary covers the LOAD: the kit joins the prefix, and no message
    // adds its tool any more.
    {
        let mut c = s.conv.lock().unwrap();
        c.rolling.summary = Some(crate::rolling::Summary { cut: 3, text: "Loaded AGENT, noted the db.".into(), made_at: 1, clear: 3 });
    }
    let (r6, ..) = w.request_body();
    assert!(names(&r6).contains(&"AGENT".to_string()) && r6.system.contains("Subagents (AGENT) suit"));
    assert!(r6.system.contains("[1] the db is postgres") && r6.effort.as_deref() == Some("low"));
    assert!(r6.messages.iter().all(|m| m["role"] != "system"));
    let flat = serde_json::to_string(&r6.messages).unwrap();
    assert!(!flat.contains("\"LOAD\"") && !flat.contains("Subagents (AGENT) suit"), "{flat}");
}

/// A transcript in which the model loaded AGENT (message 1) and BROWSER
/// (message 3), each result followed by more work.
fn two_loads() -> Conversation {
    let load = |id: &str, kit: &str| json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": "LOAD", "input": {"name": kit}}]});
    let result = |id: &str, kit: &str| json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": format!("Loaded {kit}. The {kit} tool is in your tool list now.\n\n{kit} instructions")}]});
    Conversation {
        model: "claude-opus-5-5".into(),
        effort: "high".into(),
        messages: vec![
            json!({"role": "user", "content": [oak::text("go")]}),
            load("a", "agent"),
            result("a", "AGENT"),
            load("b", "BROWSER"),
            result("b", "BROWSER"),
            json!({"role": "assistant", "content": [oak::text("both loaded")]}),
            json!({"role": "user", "content": [oak::text("next")]}),
        ]
        .into(),
        prefix: crate::prefix::State { kits: vec!["AGENT".into(), "BROWSER".into()], ..Default::default() },
        ..Default::default()
    }
}

/// Each kit is in the prefix exactly when a summary covers its LOAD;
/// otherwise its tool is told right after its LOAD's result, at every
/// renewal, and through a save and reload.
#[test]
fn a_kit_joins_the_prefix_only_once_a_summary_covers_its_load() {
    let (w, s) = idle_worker("two-loads", two_loads());
    let names = |r: &oak::Request| r.tools.iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    let added = |r: &oak::Request| -> Vec<(usize, String)> {
        let at = |i: usize, m: &Value| m["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_definition").map(|b| (i, b["definition"]["name"].as_str().unwrap().to_string())).collect::<Vec<_>>();
        r.messages.iter().enumerate().filter(|(_, m)| m["role"] == "system").flat_map(|(i, m)| at(i, m)).collect()
    };
    // The first request (nothing cached): neither kit in the prefix.
    let (r1, ..) = w.request_body();
    assert!(!names(&r1).contains(&"AGENT".to_string()) && !names(&r1).contains(&"BROWSER".to_string()));
    assert!(!r1.system.contains("Subagents (AGENT) suit") && !r1.system.contains("BROWSER kit"));
    // Each tool after its LOAD's result, before the reply that follows.
    assert_eq!(added(&r1), [(3, "AGENT".to_string()), (6, "BROWSER".to_string())]);
    assert_eq!((r1.messages[2]["role"].as_str(), r1.messages[5]["content"][0]["call_id"].as_str()), (Some("user"), Some("b")));
    // As sent to Claude: each `tool_addition` right after its LOAD's
    // result, the tool list and system prompt without either kit.
    let body = api::claude_body(&r1);
    let sent: Vec<(String, String)> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["role"].as_str().unwrap().to_string(), m["content"][0]["type"].as_str().unwrap_or("").to_string()))
        .collect();
    let at = |role: &str, kind: &str| (role.to_string(), kind.to_string());
    assert_eq!(&sent[1..7], [at("assistant", "tool_use"), at("user", "tool_result"), at("system", "tool_addition"), at("assistant", "tool_use"), at("user", "tool_result"), at("system", "tool_addition")]);
    assert_eq!(body["messages"][3]["content"][0]["tool"]["definition"]["name"], "AGENT");
    assert!(!body["tools"].as_array().unwrap().iter().any(|t| t["name"] == "AGENT" || t["name"] == "BROWSER"));
    // Cold again (expired, another account): the same request.
    s.conv.lock().unwrap().cache.cold();
    let (r2, ..) = w.request_body();
    assert_eq!((&r2.system, &r2.tools, &r2.messages), (&r1.system, &r1.tools, &r1.messages));
    // Saved and loaded again: the same request.
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let back: Conversation = serde_json::from_str(&saved).unwrap();
    let (w2, _s2) = idle_worker("two-loads-reloaded", back);
    let (r3, ..) = w2.request_body();
    assert_eq!((&r3.system, &r3.tools, &r3.messages), (&r1.system, &r1.tools, &r1.messages));
    // A summary covers AGENT's LOAD, not BROWSER's: AGENT joins the
    // prefix, BROWSER's tool stays after its LOAD.
    s.conv.lock().unwrap().rolling.summary = Some(crate::rolling::Summary { cut: 3, text: "Loaded AGENT.".into(), made_at: 1, clear: 3 });
    let (r4, ..) = w.request_body();
    assert!(names(&r4).contains(&"AGENT".to_string()) && !names(&r4).contains(&"BROWSER".to_string()));
    assert!(r4.system.contains("Subagents (AGENT) suit"));
    // The view: the summary, then messages 3.. (BROWSER's LOAD first).
    assert_eq!(r4.messages[1]["content"][0]["input"]["name"], "BROWSER");
    assert_eq!(added(&r4), [(3, "BROWSER".to_string())]);
    // Saved and loaded again, warm: the same request.
    {
        let mut c = s.conv.lock().unwrap();
        let choice = crate::cache_ttl::Choice { ttl: oak::Ttl::FiveMinutes, anchor: None };
        let base = c.rolling.base();
        c.cache.record(api::now_ms(), choice, 20_000, r4.messages.len(), base);
    }
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let back: Conversation = serde_json::from_str(&saved).unwrap();
    let (w3, _s3) = idle_worker("two-loads-reloaded-again", back);
    let (r5, ..) = w3.request_body();
    assert_eq!((&r5.system, &r5.tools, &r5.messages), (&r4.system, &r4.tools, &r4.messages));
    // A summary that covers both: both in the prefix, no tool messages.
    s.conv.lock().unwrap().rolling.summary = Some(crate::rolling::Summary { cut: 5, text: "Loaded both.".into(), made_at: 2, clear: 5 });
    let (r6, ..) = w.request_body();
    assert!(names(&r6).contains(&"AGENT".to_string()) && names(&r6).contains(&"BROWSER".to_string()));
    assert!(added(&r6).is_empty() && r6.messages.iter().all(|m| m["role"] != "system"));
}

/// Where tools can't be added (OpenRouter), a kit loaded in view is
/// reached through LOAD; one loaded with a Claude model, whose LOAD said
/// it was in the tool list, is told so in text after its result.
#[test]
fn a_kit_loaded_in_view_is_called_through_load_where_tools_cant_be_added() {
    let (w, s) = idle_worker("two-loads-generic", Conversation { provider: Some(crate::providers::Kind::OpenRouter), model: "openai/gpt-5".into(), ..two_loads() });
    assert_eq!(crate::prefix::mode(s.conv.lock().unwrap().provider(), "openai/gpt-5"), Some(crate::prefix::Mode::Generic));
    let (r, ..) = w.request_body();
    let names: Vec<&str> = r.tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!names.contains(&"AGENT") && !names.contains(&"BROWSER") && names.contains(&"LOAD"));
    let flat = serde_json::to_string(&r.messages).unwrap();
    assert!(flat.contains("The AGENT tool isn't in your tool list here: call it through LOAD, with name=AGENT"), "{flat}");
    assert!(flat.contains("The BROWSER tool isn't in your tool list here: call it through LOAD, with name=BROWSER"));
    // A LOAD whose result said so already isn't told again.
    let mut c = s.conv.lock().unwrap().clone();
    c.messages[2]["content"][0]["content"] = json!("Loaded AGENT. Its tool isn't in your tool list: call it through LOAD, with name=AGENT …");
    c.prefix = crate::prefix::State { kits: c.prefix.kits.clone(), ..Default::default() };
    let (w, _s) = idle_worker("two-loads-generic-said", c);
    let (r, ..) = w.request_body();
    let flat = serde_json::to_string(&r.messages).unwrap();
    assert!(!flat.contains("The AGENT tool isn't in your tool list here") && flat.contains("The BROWSER tool isn't in your tool list here"));
}

/// A revert to before a LOAD unloads its kit, and what was told after
/// the revert's point goes with it.
#[test]
fn a_revert_before_a_load_unloads_its_kit() {
    let mut c = two_loads();
    c.prefix.sys.push(crate::prefix::SysMsg { at: 5, kind: "tool:BROWSER".into(), ..Default::default() });
    c.prefix.sys.push(crate::prefix::SysMsg { at: 3, kind: "tool:AGENT".into(), ..Default::default() });
    let was = c.messages[3]["content"][0].clone();
    assert!(revert_to(&mut c, 3, 0, &was));
    assert_eq!(c.prefix.kits, ["AGENT"]);
    assert_eq!(c.prefix.sys.iter().map(|m| m.at).collect::<Vec<_>>(), [3]);
}

/// While a goal is active, ASK needs a REQUEST_ASK the user accepted,
/// and then works once. Without a goal, ASK works as it always did.
#[test]
fn request_ask_gates_ask_during_a_goal() {
    let (w, s) = idle_worker("request-ask", Conversation::default());
    let question = json!({"questions": [{"question": "Which one?", "options": ["a", "b"]}]});
    // No goal: REQUEST_ASK isn't needed, and ASK isn't gated.
    assert!(w.tool("REQUEST_ASK", &json!({"about": "x"})).0.unwrap_err().contains("call ASK directly"));
    assert_eq!(ask_gate(&mut s.conv.lock().unwrap()), Ok(()));

    apply_command(&mut s.conv.lock().unwrap(), Command::Goal("ship it".into(), false));
    let refused = w.tool("ASK", &question).0.unwrap_err();
    assert!(refused.starts_with("ASK isn't available while a goal is active. Call REQUEST_ASK"), "{refused}");
    w.tool("REQUEST_ASK", &json!({"about": "Which database to use"})).0.unwrap();
    assert!(w.tool("ASK", &question).0.unwrap_err().contains("is still waiting for the user"));

    // Declined: the model is told, and ASK stays closed.
    s.answer_ask_request(false);
    {
        let mut c = s.conv.lock().unwrap();
        assert_eq!(c.inbox, vec![ask_reply_message("Which database to use", false)]);
        assert!(c.inbox[0].starts_with(ASK_TAG) && c.ask_request.is_empty() && !c.ask_granted);
        c.inbox.clear();
    }
    assert!(w.tool("ASK", &question).0.is_err());

    // Accepted: the model is told, and may ASK once (here the user skips).
    w.tool("REQUEST_ASK", &json!({"about": "Which database to use"})).0.unwrap();
    s.answer_ask_request(true);
    assert!(s.conv.lock().unwrap().ask_granted);
    std::thread::scope(|sc| {
        let asking = sc.spawn(|| w.tool("ASK", &question).0);
        wait_until("the questions", || s.live.lock().unwrap().ask.is_some());
        s.live.lock().unwrap().ask.as_mut().unwrap().skipped = true;
        let _ = s.wake.send(());
        assert_eq!(asking.join().unwrap(), Ok("The user skipped these questions without answering.".into()));
    });
    assert!(w.tool("ASK", &question).0.unwrap_err().contains("Call REQUEST_ASK"), "used up");

    // A cleared goal drops a waiting request.
    w.tool("REQUEST_ASK", &json!({"about": "x"})).0.unwrap();
    apply_command(&mut s.conv.lock().unwrap(), Command::Goal(String::new(), false));
    let c = s.conv.lock().unwrap();
    assert!(c.ask_request.is_empty() && !c.ask_granted);
}

/// Calls left without a result (OAK closed while they ran) get one, in
/// the next user message.
#[test]
fn lost_calls_answered() {
    let call = |id: &str| json!({"type": "tool_call", "id": id, "name": "EXECUTE", "input": {}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": "ok"});
    let msg = |role: &str, content: Vec<Value>| json!({"role": role, "content": content});
    let ids = |m: &Value| m["content"].as_array().unwrap().iter().map(|b| b["call_id"].as_str().or(b["text"].as_str()).unwrap_or("").to_string()).collect::<Vec<_>>();
    // Closed mid-call, then the user wrote again: their message follows the result.
    let mut c = Conversation {
        messages: vec![
            msg("user", vec![oak::text("go")]),
            msg("assistant", vec![call("a")]),
            msg("user", vec![result("a")]),
            msg("assistant", vec![call("b"), call("c")]),
            msg("user", vec![result("b"), oak::text("continue")]),
        ].into(),
        ..Default::default()
    };
    assert!(answer_lost_calls(&mut c));
    assert_eq!(c.messages.len(), 5);
    assert_eq!(ids(&c.messages[4]), ["c", "b", "continue"]);
    assert_eq!(c.messages[4]["content"][0]["content"], LOST_RESULT);
    assert_eq!(c.messages[4]["content"][0]["is_error"], true);
    // Mended, it stays as it is.
    assert!(!answer_lost_calls(&mut c));
    // Closed mid-call with nothing after: a new user message.
    c.messages.push(msg("assistant", vec![json!({"type": "text", "text": "hm"}), call("d")]));
    assert!(answer_lost_calls(&mut c));
    assert_eq!((c.messages[6]["role"].as_str(), ids(&c.messages[6])), (Some("user"), vec!["d".to_string()]));
}

/// A TASK wait Oak closed on, in the turn under way and on a task Oak
/// still knows, stays unanswered to carry on; the turn's other calls get
/// the lost result, and so do a wait in an earlier turn and one on a task
/// Oak doesn't know.
#[test]
fn task_waits_left_open_to_carry_on() {
    let tasks = Tasks::open("");
    tasks.add(1, tasks::spawn("echo hi", &std::env::temp_dir()).unwrap(), "x");
    let wait = |id: &str, task: u64| json!({"type": "tool_call", "id": id, "name": "TASK", "input": {"action": "wait", "id": task, "timeout": 60}});
    let exec = |id: &str| json!({"type": "tool_call", "id": id, "name": "EXECUTE", "input": {}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": "ok"});
    let msg = |role: &str, content: Vec<Value>| json!({"role": role, "content": content});
    let ids = |m: &Value| m["content"].as_array().unwrap().iter().map(|b| b["call_id"].as_str().or(b["text"].as_str()).unwrap_or("").to_string()).collect::<Vec<_>>();
    let mut c = Conversation {
        messages: vec![
            msg("assistant", vec![wait("old", 1)]),
            msg("user", vec![oak::text("next")]),
            msg("assistant", vec![exec("e"), wait("w", 1), wait("x", 9), exec("f")]),
            msg("user", vec![result("f")]),
        ]
        .into(),
        ..Default::default()
    };
    let (changed, kept) = answer_lost_calls_but(&mut c, |call| waits_on_known_task(call, &tasks));
    assert!(changed);
    assert_eq!(kept.iter().map(|k| k["id"].as_str().unwrap()).collect::<Vec<_>>(), ["w"]);
    assert_eq!(ids(&c.messages[1]), ["old", "next"]);
    assert_eq!(ids(&c.messages[3]), ["e", "x", "f"]);
    // The reply alone at the end: still the turn under way.
    let mut c = Conversation { messages: vec![msg("user", vec![oak::text("go")]), msg("assistant", vec![wait("w", 1)])].into(), ..Default::default() };
    let (changed, kept) = answer_lost_calls_but(&mut c, |call| waits_on_known_task(call, &tasks));
    assert!(!changed && kept.len() == 1 && c.messages.len() == 2);
    tasks.kill_all();
}

/// A carried-on wait keeps the time it waited until Oak closed, and adds
/// up the time Oak was closed; records of other waits go.
#[test]
fn carried_waits_count_only_the_time_oak_ran() {
    use store::TaskWait;
    let call = json!({"type": "tool_call", "id": "w", "name": "TASK", "input": {"action": "wait", "id": 1, "timeout": 60}, "_oak_at": 500});
    let mut c = Conversation {
        task_waits: vec![TaskWait { call: "w".into(), since: 1_000, waited_ms: 500, closed_ms: 0 }, TaskWait { call: "gone".into(), ..Default::default() }],
        ..Default::default()
    };
    assert!(carry_waits(&mut c, std::slice::from_ref(&call), Some(4_000), 10_000));
    assert_eq!(c.task_waits, [TaskWait { call: "w".into(), since: 10_000, waited_ms: 3_500, closed_ms: 6_000 }]);
    // After a crash the records may be older than the stretch: none of it counts.
    carry_waits(&mut c, std::slice::from_ref(&call), Some(9_000), 12_000);
    assert_eq!(c.task_waits, [TaskWait { call: "w".into(), since: 12_000, waited_ms: 3_500, closed_ms: 8_000 }]);
    // Without a record: since the call came in.
    c.task_waits.clear();
    carry_waits(&mut c, std::slice::from_ref(&call), Some(2_000), 3_000);
    assert_eq!(c.task_waits, [TaskWait { call: "w".into(), since: 3_000, waited_ms: 1_500, closed_ms: 1_000 }]);
    // No waits: no records.
    assert!(carry_waits(&mut c, &[], None, 4_000) && c.task_waits.is_empty());
}

/// A wait carried on counts what it waited before toward its timeout and
/// says Oak was closed meanwhile; closing Oak with the tasks kept doesn't
/// end a wait, while an ultrapause does.
#[test]
fn task_wait_across_a_close() {
    let (w, _s) = idle_worker("wait-close", Conversation::default());
    w.tool("TASK_NEW", &json!({"command": "sleep 20", "summary": "x"})).0.unwrap();
    let wait = json!({"id": 0, "timeout": 60});
    w.conv.lock().unwrap().task_waits.push(store::TaskWait { call: "w".into(), since: api::now_ms(), waited_ms: 59_000, closed_ms: 3_600_000 });
    let t = Instant::now();
    let out = w.tool_for("w", "TASK_WAIT", &wait).0.unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "only the second left: {:?}", t.elapsed());
    assert!(out.starts_with("Task 0 is running") && out.contains("still running after waiting 60s") && out.contains("Oak was closed for 1h00m during this wait"), "{out}");
    assert!(w.conv.lock().unwrap().task_waits.is_empty(), "its record goes");
    // Oak closing with the tasks left running: the wait goes on.
    w.set(|l| (l.ultra, l.closing) = (true, true));
    let t = Instant::now();
    let out = w.tool_for("c", "TASK_WAIT", &json!({"id": 0, "timeout": 1})).0.unwrap();
    assert!(t.elapsed() >= Duration::from_secs(1) && out.contains("still running after waiting 1s") && !out.contains("Oak was closed"), "{out}");
    // A plain ultrapause ends it.
    w.set(|l| l.closing = false);
    let out = w.tool_for("u", "TASK_WAIT", &json!({"id": 0, "timeout": 60})).0.unwrap();
    assert!(out.contains("ultrapause"), "{out}");
}

/// At start, a wait Oak closed on runs again first thing: its result joins
/// the turn's other results, the task's notification is dropped (the
/// result says it ended), and the turn is to carry on.
#[test]
fn open_waits_carry_on_first() {
    let (w, _s) = idle_worker("carry-on", Conversation::default());
    w.tool("TASK_NEW", &json!({"command": "echo done", "summary": "x"})).0.unwrap();
    wait_until("the notification", || !w.live.lock().unwrap().notes.is_empty());
    let call = json!({"type": "tool_call", "id": "w", "name": "TASK", "input": {"action": "wait", "id": 0, "timeout": 60}});
    w.push("assistant", vec![json!({"type": "tool_call", "id": "e", "name": "EXECUTE", "input": {}}), call.clone()]);
    w.push("user", vec![json!({"type": "tool_result", "call_id": "e", "content": "ok"})]);
    w.conv.lock().unwrap().task_waits.push(store::TaskWait { call: "w".into(), since: api::now_ms(), waited_ms: 1_000, closed_ms: 5_000 });
    *w.open_waits.lock().unwrap() = vec![call];
    w.carry_on_waits();
    let c = w.conv.lock().unwrap();
    let last = c.messages.last().unwrap();
    assert_eq!(c.messages.len(), 2);
    let results: Vec<&str> = last["content"].as_array().unwrap().iter().map(|b| b["call_id"].as_str().unwrap()).collect();
    assert_eq!(results, ["w", "e"]);
    let text = last["content"][0]["content"].as_str().unwrap();
    assert!(text.starts_with("Task 0 exited with code 0") && text.contains("Oak was closed for 5s during this wait"), "{text}");
    assert!(c.task_waits.is_empty());
    drop(c);
    assert!(w.live.lock().unwrap().notes.is_empty(), "the result told of the exit");
    assert!(w.resume.load(Ordering::SeqCst));
}

/// A conversation read from its file is looked at without being parsed,
/// and mended exactly as one in memory.
#[test]
fn lost_calls_answered_as_read_from_the_file() {
    let call = |id: Value| json!({"type": "tool_call", "id": id, "name": "EXECUTE", "input": {}});
    let result = |id: Value| json!({"type": "tool_result", "call_id": id, "content": "ok\n\"quoted\""});
    let msg = |role: &str, content: Value| json!({"role": role, "content": content});
    let cases = [
        vec![msg("user", json!("go")), msg("assistant", json!([call(json!("a"))])), msg("user", json!([result(json!("a"))]))],
        vec![msg("user", json!("go")), msg("assistant", json!([call(json!("a"))]))],
        vec![msg("assistant", json!([call(json!(1)), call(json!("b"))])), msg("user", json!([result(json!(1)), oak::text("next")]))],
        vec![msg("assistant", json!([call(json!("a"))])), msg("user", json!("a reply as text"))],
        // Two replies in a row: not mended.
        vec![msg("assistant", json!([call(json!("a"))])), msg("assistant", json!([oak::text("again")]))],
        vec![msg("assistant", json!([{"type": "text", "text": "tool_call"}])), json!("not a message")],
        vec![json!({"content": [call(json!("x"))], "role": "assistant", "extra": {"role": "user"}}), msg("user", json!([result(json!("x"))]))],
    ];
    for messages in cases {
        let mut mem = Conversation { messages: messages.clone().into(), ..Default::default() };
        let mut read: Conversation = serde_json::from_slice(&serde_json::to_vec(&mem).unwrap()).unwrap();
        let mended = answer_lost_calls(&mut mem);
        assert_eq!(answer_lost_calls(&mut read), mended, "{messages:?}");
        if !mended && messages.iter().all(Value::is_object) {
            assert!(read.messages.unread().is_some(), "parsed for nothing: {messages:?}");
        }
        assert_eq!(read.messages, mem.messages);
    }
}

/// Calls run together unless they must not: ASK goes alone, and calls
/// on one file that writes it keep their order.
#[test]
fn tool_call_order() {
    let call = |name: &str, input: Value| json!({"type": "tool_call", "id": name, "name": name, "input": input});
    let folder = Path::new(if cfg!(windows) { "C:\\work" } else { "/work" });
    let calls = [
        call("READ", json!({"path": "a.rs"})),
        call("READ", json!({"path": "b.rs"})),
        call("EXECUTE", json!({"command": "ls", "summary": "List", "timeout": 5})),
        call("READ", json!({"path": if cfg!(windows) { "C:/work/A.rs" } else { "/work/a.rs" }})), // the same file, read again: fine
        call("EDIT", json!({"path": "a.rs"})), // writes what was read
        call("EDIT", json!({"path": "b.rs"})),
        call("EDIT", json!({"path": "a.rs"})), // writes what was written
        call("ASK", json!({})),
        call("SEARCH", json!({})),
    ];
    let none: Vec<usize> = vec![];
    assert_eq!(waits_for(&calls, folder), vec![none.clone(), none.clone(), none.clone(), none, vec![0, 3], vec![1], vec![0, 3, 4], (0..7).collect(), vec![7]]);
    // CD goes alone too: what follows it means another folder.
    let calls = [call("READ", json!({"path": "a.rs"})), call("CD", json!({"path": "sub"})), call("READ", json!({"path": "a.rs"})), call("SEARCH", json!({}))];
    assert_eq!(waits_for(&calls, folder), vec![vec![], vec![0], vec![1], vec![1]]);
    // The browser's calls keep their order, also those made through LOAD
    // (before BROWSER is in the tool list); loading a kit doesn't wait.
    let calls = [
        call("BROWSER", json!({"action": "click", "ref": 1})),
        call("LOAD", json!({"name": "BROWSER", "input": {"action": "screenshot"}})),
        call("LOAD", json!({"name": "AGENT"})),
        call("LOAD", json!({"name": "BROWSER", "input": {"action": "read"}})),
    ];
    assert_eq!(waits_for(&calls, folder), vec![vec![], vec![0], vec![], vec![0, 1]]);
}

/// CD switches the working folder (relative, `..`, `~`), keeps the
/// scratchpad where it was and returns the new folder's instructions; a
/// missing folder or a file is refused, and the system prompt keeps the
/// folder the cache has.
#[test]
fn cd_switches_the_working_folder() {
    let root = std::env::temp_dir().join(format!("oak-cd-{}", api::now_ms()));
    let sub = root.join("proj");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("CLAUDE.md"), "Use tabs.").unwrap();
    std::fs::write(sub.join("a.txt"), "in proj").unwrap();
    let (w, s) = idle_worker("cd", Conversation { folder: root.clone(), ..Default::default() });
    let pad = scratchpad(&mut s.conv.lock().unwrap());
    let cd = |path: &str| w.tool("CD", &json!({"path": path})).0;

    assert!(cd("nowhere").unwrap_err().starts_with("No folder"));
    assert!(cd("proj/a.txt").unwrap_err().contains("is a file"));
    assert!(cd(".").unwrap().contains("already"));
    let out = cd("proj").unwrap();
    assert!(out.contains(&format!("now {}", sub.display())) && out.contains("Use tabs.") && out.contains(&pad.display().to_string()), "{out}");
    assert_eq!(s.conv.lock().unwrap().folder, sub);
    assert!(w.tool("READ", &json!({"path": "a.txt"})).0.unwrap().contains("in proj"));
    // The scratchpad stays, and is still found by the scan.
    assert_eq!(scratchpad(&mut s.conv.lock().unwrap()), pad);
    let owners = [crate::scratch::Owner { id: "x".into(), folder: sub.clone(), scratch: s.conv.lock().unwrap().scratch.clone(), last_ms: 0, in_use: false }];
    assert!(crate::scratch::pads(&owners).iter().any(|p| p.dir == pad && p.owners == ["x"]));
    // Back up, with `..`; the old folder has no instructions.
    let out = cd("../proj/..").unwrap();
    assert!(out.contains(&format!("now {}", root.display())) && !out.contains("instructions"), "{out}");
    assert_eq!(scratchpad(&mut s.conv.lock().unwrap()), pad);
    // The pin keeps the folder the cache was written with.
    let mut p = crate::prefix::State::default();
    let pin = |folder: &Path| crate::prefix::Pin { day: "d".into(), folder: folder.display().to_string(), tools: vec![json!({"name": "READ"})], ..Default::default() };
    let setup = |n, now: crate::prefix::Pin| crate::prefix::Setup { mode: crate::prefix::Mode::Claude, n, cut: 0, add: true, effort_per_message: true, base: now.clone(), now, joins: vec![], notes: 0 };
    assert_eq!(p.prepare(setup(1, pin(&root))).folder, root.display().to_string());
    assert_eq!(p.prepare(setup(3, pin(&sub))).folder, root.display().to_string());
    assert!(p.sys.is_empty());
    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}

/// A reply's tool calls start as they stream in, when the calls before
/// them allow (a READ of a file waits for its WRITE; ASK waits for the
/// reply's end); their results wait out of the transcript till the
/// reply is in it, then come in the calls' order.
#[test]
fn calls_start_while_the_reply_streams() {
    let dir = std::env::temp_dir().join(format!("oak-early-calls-{}", api::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let (w, s) = idle_worker("early", Conversation { folder: dir.clone(), ..Default::default() });
    let call = |id: &str, name: &str, input: Value| json!({"type": "tool_call", "id": id, "name": name, "input": input});
    let calls = [
        call("c1", "EXECUTE", json!({"command": "sleep 1; echo slow", "summary": "Wait", "timeout": 20})),
        call("c2", "WRITE", json!({"path": "a.txt", "content": "hi"})),
        call("c3", "READ", json!({"path": "a.txt"})),
        call("c4", "ASK", json!({"questions": [{"question": "Ok?"}]})),
    ];
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    std::thread::scope(|scope| {
        let mut b = Batch::new(&w, scope);
        b.add(calls[0].clone());
        b.pump(false, oak::stop::TOOL_CALLS);
        b.add(calls[1].clone());
        b.add(calls[2].clone());
        b.pump(false, oak::stop::TOOL_CALLS);
        // The EXECUTE and the WRITE run; the READ waits for the WRITE only.
        wait_until("the WRITE's result", || w.live.lock().unwrap().early.as_ref().is_some_and(|e| e.iter().any(|r| r["call_id"] == "c2")));
        assert!(!b.started[2] && b.started[0]);
        b.pump(false, oak::stop::TOOL_CALLS);
        assert!(b.started[2], "the READ starts once the WRITE is done");
        b.add(calls[3].clone());
        b.pump(false, oak::stop::TOOL_CALLS);
        assert!(!b.started[3], "ASK waits for the reply's end");
        assert_eq!(s.conv.lock().unwrap().messages.len(), 1, "the results wait for the reply");
        // A reply cut short here keeps the calls that started.
        let mut blocks: Vec<Value> = vec![oak::text("on it")];
        blocks.extend(calls.iter().cloned());
        blocks.push(oak::text("and"));
        let kept = b.kept(blocks);
        assert_eq!(kept.iter().map(|k| k["id"].as_str().unwrap_or("text")).collect::<Vec<_>>(), ["text", "c1", "c2", "c3"]);
        // The reply as it came: its results, all of them, in order (the
        // ASK, answered by nobody, cut short).
        w.push("assistant", kept);
        let stopped = b.finish(&calls[..3], CUT_SHORT);
        assert!(!stopped);
    });
    let c = s.conv.lock().unwrap();
    let results = c.messages[2]["content"].as_array().unwrap();
    assert_eq!(results.iter().map(|r| r["call_id"].as_str().unwrap()).collect::<Vec<_>>(), ["c1", "c2", "c3", "c4"]);
    assert!(results[0]["content"].as_str().unwrap().contains("slow"));
    assert!(results[2]["content"].as_str().unwrap().contains("hi"));
    assert!(results[3]["content"].as_str().unwrap().starts_with("Not run: the reply was cut short"));
    assert!(w.live.lock().unwrap().early.is_none());
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first request puts the identity, instructions and tools lines at
/// the top, folding in the instruction files' line, each opening to what
/// was sent.
#[test]
fn start_marks_show_what_the_model_is_given() {
    let mut c = Conversation { model: "claude-opus-5-5".into(), ..Default::default() };
    c.marks.push(Mark::new(0, MarkKind::Goal, "g", 0));
    c.marks.push(Mark { sent: "ctx".into(), ..Mark::new(1, MarkKind::Instructions, "instructions loaded: CLAUDE.md", 0) });
    let tools = tools::definitions(false);
    let p = api::preamble(&c.model, "SYS", &tools);
    start_marks(&mut c, 1, &p, "ctx", 0);
    let kinds: Vec<MarkKind> = c.marks.iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MarkKind::Goal, MarkKind::Identity, MarkKind::Instructions, MarkKind::Tools]);
    assert_eq!(c.marks[2].text, "instructions loaded: Normal system prompt, CLAUDE.md (1 tokens)");
    assert_eq!(store::split_sent(&c.marks[2].sent), [("system prompt", "SYS"), ("start of the first message", "ctx")]);
    let n = tools.as_array().unwrap().len();
    assert!(c.marks[3].text.starts_with(&format!("tools loaded: {n} tools (")) && c.marks[3].text.ends_with("k tokens)"), "{}", c.marks[3].text);
    // Freebuff: no identity, its own prompt and the CLI's tools.
    let mut c = Conversation { model: "deepseek/deepseek-v4-flash".into(), ..Default::default() };
    let p = api::preamble(&c.model, "SYS", &tools);
    start_marks(&mut c, 0, &p, "", 0);
    assert_eq!(c.marks.iter().map(|m| m.kind).collect::<Vec<_>>(), [MarkKind::Instructions, MarkKind::Tools]);
    assert_eq!(store::split_sent(&c.marks[1].sent)[0].0, "read_files");
    // Switched back to Claude later on: the lines come again, there.
    c.model = "claude-opus-5-5".into();
    c.messages = vec![json!({"role": "user", "content": "hi"})].into();
    let p = api::preamble(&c.model, "SYS", &tools);
    assert!(preamble_changed(&c, &p));
    let n = c.marks.len();
    start_marks(&mut c, n, &p, "", 1);
    assert_eq!((c.marks[2].kind, c.marks[2].at), (MarkKind::Identity, 1));
    assert!(!preamble_changed(&c, &p));
    // A tool's wording changing (a provider's models leaving the AGENT
    // tool's list) is no switch: no new lines.
    let mut reworded = api::preamble(&c.model, "SYS", &tools);
    reworded.tools[0]["description"] = json!("other words");
    assert!(!preamble_changed(&c, &reworded));
}

/// The memory block (goal, notes) is part of the prefix: it changes only
/// with a summary, never because the cache expired or went cold.
#[test]
fn memory_changes_only_with_a_summary() {
    let mut c = Conversation::default();
    assert_eq!(memory_for_request(&mut c, 0), "");
    let (block, mark) = apply_command(&mut c, Command::Goal("ship v2".into(), false));
    let block = block.unwrap();
    assert!(block["text"].as_str().unwrap().starts_with(GOAL_TAG) && is_hidden_block(&block));
    assert_eq!(mark, (MarkKind::Goal, "ship v2".into()));
    memory_tool(&mut c, "NOTE_NEW", &json!({"text": "the db is postgres"}), 0).unwrap();
    let ttl = c.cache.ttl_ms();
    assert_eq!(memory_for_request(&mut c, ttl / 2), "", "a pause shorter than the cache's lifetime");
    // The cache expired, or went cold (another account): the same.
    let later = 2 * ttl + ttl / 2 + 10_000;
    assert_eq!(memory_for_request(&mut c, later), "");
    c.cache.cold();
    assert_eq!(memory_for_request(&mut c, later + 500), "");
    // A summary moves the cut: brought up to date.
    c.rolling.apply(Summary { cut: 2, text: "s".into(), made_at: 0, ..Default::default() }, 0, Default::default());
    let m = memory_for_request(&mut c, later + 1_000);
    assert!(m.contains("# Session memory") && m.contains("ship v2") && m.contains("[1] the db is postgres"), "{m}");
    memory_tool(&mut c, "NOTE_NEW", &json!({"text": "second note"}), 0).unwrap();
    assert!(!memory_for_request(&mut c, later + 10 * ttl).contains("second note"));
}

/// Reverting goes back to before a message the user sent: it and all
/// after it go, as do later lines and a summary that covered them; the
/// earlier blocks of its turn (tool results) stay.
#[test]
fn revert_drops_the_message_and_what_follows() {
    let user = |t: &str| json!({"role": "user", "content": [oak::text(t)]});
    let reply = |t: &str| json!({"role": "assistant", "content": [oak::text(t)]});
    let mut c = Conversation {
        messages: vec![
            user("one"),
            json!({"role": "assistant", "content": [{"type": "tool_call", "id": "a", "name": "READ", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "call_id": "a", "content": "x"}, oak::text("two")]}),
            reply("ok"),
            user("three"),
            reply("done"),
        ].into(),
        marks: vec![Mark::new(2, MarkKind::Effort, "low", 0), Mark::new(5, MarkKind::Effort, "max", 0)],
        ..Default::default()
    };
    c.rolling.summary = Some(crate::rolling::Summary { cut: 4, text: "s".into(), made_at: 0, ..Default::default() });
    assert!(!revert_to(&mut c, 2, 1, &oak::text("other")), "only the block the user saw");
    assert!(!revert_to(&mut c, 2, 5, &oak::text("two")), "no such block");
    assert!(revert_to(&mut c, 2, 1, &oak::text("two")));
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[2]["content"].as_array().unwrap().len(), 1, "the tool result stays");
    assert_eq!(c.marks.len(), 1);
    assert!(c.rolling.summary.is_none(), "the summary covered what was reverted");
}

/// Reverting mid-turn, to before a block of a reply: the blocks before
/// it stay (thinking left at the end goes), with the results of the
/// calls among them; the calls cut lose theirs, and what came after goes.
#[test]
fn revert_into_a_reply_keeps_what_came_before() {
    let call = |id: &str| json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": id}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": format!("out {id}")});
    let thinking = json!({"type": "reasoning", "text": "hmm", "sig": "s", "by": "claude"});
    let base = Conversation {
        messages: vec![
            json!({"role": "user", "content": [oak::text("go")]}),
            json!({"role": "assistant", "content": [thinking.clone(), oak::text("looking"), call("a"), call("b"), call("c")]}),
            json!({"role": "user", "content": [result("a"), result("b"), result("c"), oak::text("note")]}),
            json!({"role": "assistant", "content": [thinking.clone(), oak::text("done")]}),
        ].into(),
        marks: vec![Mark { at: 3, kind: MarkKind::Effort, text: "low".into(), time: 0, done: 0, sent: String::new(), scope: String::new() }],
        ..Default::default()
    };
    // Before call b: a and its result stay.
    let mut c = base.clone();
    let mut b = call("b");
    b["_oak_at"] = json!(5); // OAK's own fields don't count
    assert!(revert_to(&mut c, 1, 3, &b));
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[1]["content"].as_array().unwrap().len(), 3);
    assert_eq!(c.messages[2]["content"], json!([result("a")]));
    assert!(c.marks.is_empty());
    // Before call a: the reply keeps its text, and the turn ends there.
    let mut c = base.clone();
    assert!(revert_to(&mut c, 1, 2, &call("a")));
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[1]["content"], json!([thinking.clone(), oak::text("looking")]));
    // Before the text: only thinking is left, so the reply goes.
    let mut c = base.clone();
    assert!(revert_to(&mut c, 1, 1, &oak::text("looking")));
    assert_eq!(c.messages.len(), 1);
    // Before the last reply's text: back to the end of the results.
    let mut c = base.clone();
    assert!(revert_to(&mut c, 3, 1, &oak::text("done")));
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.marks.len(), 1, "the line before that reply stays");
    // Not the block the user saw.
    let mut c = base.clone();
    assert!(!revert_to(&mut c, 1, 3, &call("c")));
    assert_eq!(c.messages, base.messages);
    // A call whose result never came gets one.
    let mut c = base.clone();
    c.messages.truncate(2);
    assert!(revert_to(&mut c, 1, 3, &call("b")));
    assert_eq!(c.messages[2]["content"][0]["call_id"], "a");
    assert_eq!(c.messages[2]["content"][0]["is_error"], true);
    // Before the reply streaming in: what the conversation had then.
    let mut c = base.clone();
    assert!(revert_to_end(&mut c, 3));
    assert_eq!(c.messages.len(), 3);
    assert!(!revert_to_end(&mut c, 9));
}

/// A session's worker applies a revert into a reply (`Session::rewind`)
/// and is left idle, ready for the user's next message.
#[test]
fn rewind_goes_through_the_worker() {
    let call = |id: &str| json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": id}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": "x"});
    let conv = Conversation {
        messages: vec![
            json!({"role": "user", "content": [oak::text("go")]}),
            json!({"role": "assistant", "content": [call("a"), call("b")]}),
            json!({"role": "user", "content": [result("a"), result("b")]}),
            json!({"role": "assistant", "content": [oak::text("done")]}),
        ].into(),
        ..Default::default()
    };
    let s = Deleting(Session::start(unit_conv("rewind", conv), eframe::egui::Context::default()));
    s.rewind(RevertTo::Block(1, 1, call("b")));
    wait_until("the revert", || s.conv.lock().unwrap().messages.len() == 3 && !s.live.lock().unwrap().busy);
    assert_eq!(s.conv.lock().unwrap().messages[2]["content"], json!([result("a")]));
    assert!(s.live.lock().unwrap().revert.is_none());
    s.rewind(RevertTo::End(1));
    wait_until("the second revert", || s.conv.lock().unwrap().messages.len() == 1);
}

/// A task that exits mid-turn is notified with the next request, after
/// that request's tool results; one the model already saw exit isn't
/// notified at all.
#[test]
fn task_notifications_arrive_mid_turn() {
    let (w, _s) = idle_worker("notes-mid-turn", Conversation::default());
    w.tool("TASK_NEW", &json!({"command": "echo quick", "summary": "x"})).0.unwrap();
    w.tool("TASK_NEW", &json!({"command": "echo seen", "summary": "y"})).0.unwrap();
    wait_until("the notifications", || w.live.lock().unwrap().notes.len() >= 2);
    w.tool("TASK_GET_NEW_OUTPUT", &json!({"id": 1})).0.unwrap();
    w.push("assistant", vec![json!({"type": "tool_call", "id": "t1", "name": "EXECUTE", "input": {}})]);
    w.push("user", vec![json!({"type": "tool_result", "call_id": "t1", "content": "ok"})]);
    w.absorb_notices();
    assert!(w.live.lock().unwrap().notes.is_empty());
    let c = w.conv.lock().unwrap();
    let last = c.messages.last().unwrap()["content"].as_array().unwrap().clone();
    drop(c);
    assert_eq!(last[0]["type"], "tool_result", "the tool results stay first");
    let texts: Vec<&str> = last[1..].iter().filter_map(|b| b["text"].as_str()).collect();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].starts_with(NOTE_TAG) && texts[0].contains("Task 0 "), "{texts:?}");
}

/// Against the real API, with the signed-in account (run with
/// `cargo test live_rolling -- --ignored --nocapture`): a seeded ~30k
/// conversation passes a low trigger, gets summarized in the background
/// while the agent runs a tool, and is then asked about something only
/// the summary still holds. Checks that the summary request is served
/// from the prompt cache and that the API accepts the rolled view.
#[test]
#[ignore]
fn live_rolling() {
    assert!(api::load_auth(), "sign in to Oak first");
    let model = api::latest_models().unwrap().into_iter().find(|m| m.id.contains("sonnet")).expect("a Sonnet model").id;
    rolling::set_config(rolling::Config { trigger: 20_000, keep: 5_000 });

    let filler = |turn: usize| -> String {
        (0..300).map(|i| format!("Build log {turn}.{i}: compiled crate number {i} in {} ms without warnings.\n", 100 + i)).collect()
    };
    let mut messages = vec![];
    for turn in 0..6 {
        let mut text = filler(turn);
        if turn == 0 {
            text = format!("Remember this instruction for the whole session: the deploy codeword is PELICAN-42.\n\n{text}");
        }
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": text}]}));
        messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": format!("Noted log batch {turn}.")}]}));
    }
    let folder = std::env::temp_dir().join("oak-live-rolling");
    std::fs::create_dir_all(&folder).unwrap();
    let conv = Conversation {
        id: format!("live-test-{}", api::now_ms()),
        title: "live rolling test".into(),
        folder,
        model,
        effort: "low".into(),
        messages: messages.into(),
        ..Default::default()
    };
    let id = conv.id.clone();
    let s = Session::start(conv, eframe::egui::Context::default());
    let wait = |what: &str| {
        let t = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let l = s.live.lock().unwrap();
            if !l.busy && !l.compacting && l.queued.is_empty() {
                assert!(l.error.is_none(), "{what}: {:?}", l.error);
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(300), "{what} timed out");
        }
    };
    s.send("Use EXECUTE to run `echo rolling-test`, then reply with just DONE.".into(), vec![]);
    wait("first turn");
    s.send("What is the deploy codeword? Reply with just the codeword.".into(), vec![]);
    wait("second turn");

    let c = s.conv.lock().unwrap().clone();
    crate::store::remove_conversation(&id);
    let r = &c.rolling;
    let cost = r.last_cost.expect("a summary was made");
    println!("summary cost: {cost:?}; saved ~{} tokens; context now {}", r.saved, r.context);
    let answer = c.messages.last().unwrap()["content"].to_string();
    println!("answer: {answer}");
    assert!(r.last_error.is_none());
    assert_eq!(r.compressions, 1);
    assert!(cost.cache_read > 20_000, "the summary should read the cached prefix: {cost:?}");
    assert!(cost.fresh < 3_000, "only the instruction should be new input: {cost:?}");
    assert!(r.context < 20_000, "the rolled context is smaller: {}", r.context);
    assert!(answer.contains("PELICAN-42"), "the summary kept the instruction");
}

/// A session waiting for a usage limit's reset tries again at the reset, or
/// as soon as an account is signed in or comes free early.
#[test]
fn a_limit_wait_ends_when_an_account_comes_free() {
    assert!(!limit_over(60_000, 4, 4));
    assert!(limit_over(0, 4, 4));
    assert!(limit_over(60_000, 4, 5));
}

/// Picking another model while a session waits out one model's limit ends
/// the wait at once (the change is still only queued then), and the next
/// request goes on the new model.
#[test]
fn a_model_change_ends_a_limit_wait() {
    let far = api::now_ms() + 24 * 3_600_000;
    let (w, s) = idle_worker("limit-model", Conversation { model: "claude-fable-5-2".into(), effort: "high".into(), limit_wait: Some(far), ..Default::default() });
    s.command(Command::Model(api::Provider::Claude, "claude-opus-5-5".into()));
    let t = Instant::now();
    assert!(w.wait_for_limit());
    assert!(t.elapsed() < Duration::from_secs(5), "waited {:?}", t.elapsed());
    assert!(s.conv.lock().unwrap().limit_wait.is_none());
    w.take_commands();
    assert_eq!(s.conv.lock().unwrap().model, "claude-opus-5-5");
}

/// A chat runs only the web, ASK and notes: every other tool is refused
/// at run time (whether or not the model was given it, called directly,
/// as a family, through LOAD or the browser), and nothing touches the
/// folder. Its requests carry only those tools, the chat prompt, no
/// folder and no scratchpad; goals and Freebuff never reach it.
#[test]
fn a_chat_runs_no_file_command_or_agent_tool() {
    let dir = std::env::temp_dir().join(format!("oak-chat-test-{}", api::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("secret.txt"), "hidden").unwrap();
    let (w, s) = idle_worker("chat", Conversation { chat: true, model: "claude-opus-5-5".into(), effort: "high".into(), ..Default::default() });
    s.conv.lock().unwrap().folder = PathBuf::new();
    let secret = dir.join("secret.txt").display().to_string();
    let made = dir.join("made.txt");
    let refused = [
        ("READ", json!({"path": secret})),
        ("WRITE", json!({"path": made.display().to_string(), "content": "x"})),
        ("EDIT", json!({"path": secret, "old_string": "hidden", "new_string": "shown"})),
        ("EXECUTE", json!({"command": format!("cat {secret} > {}", made.display())})),
        ("TASK_NEW", json!({"command": "true"})),
        ("DISPLAY", json!({"paths": [secret]})),
        ("SCREENSHOT", json!({"url": format!("file://{secret}")})),
        ("CD", json!({"path": dir.display().to_string()})),
        ("AGENT_NEW", json!({"name": "x", "task": "read the files"})),
        ("HEARTBEAT_START", json!({"every_min": 5, "message": "x"})),
        ("PROGRESS_SET", json!({"label": "x", "command": "ls", "every_min": 1})),
        ("GOAL_FINISH", json!({})),
        ("BROWSER_EVAL", json!({"script": "1"})),
    ];
    for (name, input) in &refused {
        let e = w.tool(name, input).0.unwrap_err();
        assert!(e.contains("isn't available in a chat"), "{name}: {e}");
    }
    // As the model would call them: families, LOAD, the browser, an unknown tool.
    let calls = vec![
        json!({"type": "tool_call", "id": "a", "name": "TASK", "input": {"action": "new", "command": "true"}}),
        json!({"type": "tool_call", "id": "b", "name": "LOAD", "input": {"name": "AGENT", "input": {"action": "new", "name": "x", "task": "y"}}}),
        json!({"type": "tool_call", "id": "c", "name": "BROWSER", "input": {"action": "eval", "script": "1"}}),
        json!({"type": "tool_call", "id": "d", "name": "EXECUTE", "input": {"command": "true"}}),
        json!({"type": "tool_call", "id": "e", "name": "NOPE", "input": {}}),
        json!({"type": "tool_call", "id": "f", "name": "NOTE", "input": {"action": "new", "text": "likes tea"}}),
    ];
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    s.conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": calls}));
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    {
        let c = s.conv.lock().unwrap();
        let results = c.messages[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 6);
        for r in &results[..5] {
            assert!(r["content"].to_string().contains("isn't available in a chat"), "{r}");
        }
        assert!(!results[5]["content"].to_string().contains("isn't available"), "{}", results[5]);
        assert_eq!(c.notes.len(), 1);
        assert!(c.agents.is_empty() && c.scratch.is_empty() && c.prefix.kits.is_empty());
    }
    assert!(!made.exists() && std::fs::read_to_string(dir.join("secret.txt")).unwrap() == "hidden");
    // The request: the chat's prompt and tools only.
    let (r, ..) = w.request_body();
    let mut names: Vec<String> = r.tools.iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect();
    names.sort();
    assert_eq!(names, ["ASK", "FETCH", "NOTE", "SEARCH"]);
    assert!(r.system.contains("This is a chat, not a coding session") && !r.system.contains("Working folder") && !r.system.contains("EXECUTE"));
    assert!(r.system.contains("[1] likes tea"));
    assert!(r.folder.is_none());
    assert!(s.conv.lock().unwrap().scratch.is_empty());
    // No goal, no Freebuff; effort and other models go through.
    s.command(Command::Goal("ship it".into(), false));
    s.command(Command::Model(api::Provider::Freebuff, "freebuff".into()));
    s.command(Command::Effort("low".into()));
    w.take_commands();
    {
        let c = s.conv.lock().unwrap();
        assert!(c.goal.is_empty() && c.provider() != api::Provider::Freebuff && c.effort == "low");
    }
    assert!(chat_refuses_provider(true, api::Provider::Freebuff).is_some());
    assert!(chat_refuses_provider(false, api::Provider::Freebuff).is_none() && chat_refuses_provider(true, api::Provider::Claude).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A coding conversation keeps every tool; `chat` isn't written for it, so
/// its file reads as before, and a chat's reads back as a chat.
#[test]
fn chat_is_kept_and_coding_conversations_are_unchanged() {
    let coding = Conversation { id: "c".into(), ..Default::default() };
    let text = serde_json::to_string(&coding).unwrap();
    assert!(!text.contains("\"chat\""));
    assert!(!serde_json::from_str::<Conversation>(&text).unwrap().chat);
    let chat = Conversation { id: "c".into(), chat: true, ..Default::default() };
    assert!(serde_json::from_str::<Conversation>(&serde_json::to_string(&chat).unwrap()).unwrap().chat);
    let (w, s) = idle_worker("coding", Conversation { model: "claude-opus-5-5".into(), effort: "high".into(), ..Default::default() });
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r, ..) = w.request_body();
    let names: Vec<&str> = r.tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"EXECUTE") && names.contains(&"READ") && names.contains(&"LOAD"), "{names:?}");
    assert!(r.system.contains("Working folder") && r.folder.is_some());
    assert!(w.tool("NOTE_NEW", &json!({"text": "x"})).0.is_ok());
    assert!(prompt::ROLES.iter().all(|r| r.id != "chat" && r.id != "tools"));
}

/// A super agent (the New screen's Super agent) asks as one, with the
/// AGENT kit to load; its subagents, of its role, ask as one agent with
/// one task.
#[test]
fn super_agents_coordinate_and_their_agents_work() {
    let role = crate::agent::prompt::SUPER_ROLE.to_string();
    let (w, s) = idle_worker("super", Conversation { model: "claude-opus-5-5".into(), role: role.clone(), title: "Release train".into(), titled: true, ..Default::default() });
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("ship it")]}));
    let (r, ..) = w.request_body();
    assert!(r.system.starts_with("You are a super agent") && r.system.contains("Load the AGENT kit") && r.system.contains("Every EXECUTE and TASK command"));
    assert!(r.tools.iter().any(|t| t["name"] == "LOAD"));
    let (w, s) = idle_worker("super-agent1", Conversation { model: "claude-opus-5-5".into(), role, parent: Some("unit-test-super".into()), agent_id: 1, ..Default::default() });
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("do x")]}));
    let (r, ..) = w.request_body();
    assert!(r.system.starts_with("You are Oak, a coding agent") && r.system.contains("started by a super agent") && !r.system.contains("Load the AGENT kit"));
}

/// EXECUTE returns as soon as its command exits, not on its next tick;
/// a message from the user still moves a running one to the background.
#[test]
fn execute_returns_when_its_command_exits() {
    let dir = std::env::temp_dir();
    let (w, _s) = idle_worker("exec-exit", Conversation { folder: dir.clone(), ..Default::default() });
    let run = |cmd: &str| w.tool("EXECUTE", &json!({"command": cmd, "summary": "Run it", "timeout": 20}));
    let quickest = |f: &dyn Fn()| (0..10).map(|_| { let t = Instant::now(); f(); t.elapsed() }).min().unwrap();
    let execute = quickest(&|| {
        let (r, interrupted) = run("echo hi");
        assert!(r.as_ref().is_ok_and(|r| r.starts_with("hi\n[exit code 0")) && !interrupted, "{r:?}"); // "[exit code 0, ran 7.1s]" on a busy machine
    });
    let bare = quickest(&|| assert_eq!(tasks::spawn("echo hi", &dir).unwrap().wait_exit(Duration::from_secs(20)), Some(0)));
    assert!(execute < bare + Duration::from_millis(15), "EXECUTE took {execute:?}, the command itself {bare:?}");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            w.interrupt.store(true, Ordering::SeqCst);
        });
        let t = Instant::now();
        let (r, interrupted) = run("echo started; until [ -e /oak-never ]; do sleep 1; done");
        let r = r.unwrap();
        assert!(interrupted && r.contains("started") && r.contains("moved to the background"), "{r}");
        assert!(t.elapsed() < Duration::from_secs(3));
    });
    w.tasks.kill_all();
}

#[test]
fn phase_reads_oaks_own_block_types() {
    let mut l = Live { busy: true, ..Live::default() };
    assert_eq!(l.plugin_phase().0, "requesting");
    l.blocks = vec![json!({"type": "reasoning", "text": "hm", "by": "claude"})];
    assert_eq!(l.plugin_phase(), ("thinking", String::new()));
    l.blocks.push(json!({"type": "text", "text": "So"}));
    assert_eq!(l.plugin_phase().0, "writing");
    l.blocks.push(json!({"type": "tool_call", "id": "t", "name": "READ", "input": {}}));
    assert_eq!(l.plugin_phase(), ("tool", "READ".to_string()));
}

/// A loaded kit's tool called through LOAD runs as that tool would called
/// directly: its input repaired as that tool's as it arrives (here sent as
/// JSON text, with the agent's id as a string), its result under the
/// LOAD call's id, the call named as the tool it ran.
#[test]
fn calls_through_load_run_as_their_tool() {
    let (w, s) = idle_worker("through-load", Conversation { model: "claude-opus-5-5".into(), effort: "high".into(), ..Default::default() });
    s.conv.lock().unwrap().prefix.kits.push("AGENT".into());
    let input = tools::repair("LOAD", json!({"name": "AGENT", "input": "{\"action\": \"send\", \"id\": \"7\", \"message\": \"hi\", \"timeout\": null}"}));
    assert_eq!(input, json!({"name": "AGENT", "input": {"action": "send", "id": 7, "message": "hi"}}));
    let calls = vec![
        json!({"type": "tool_call", "id": "s1", "name": "LOAD", "input": input}),
        json!({"type": "tool_call", "id": "l1", "name": "LOAD", "input": {"name": "AGENT", "input": {"action": "list"}}}),
        json!({"type": "tool_call", "id": "k1", "name": "LOAD", "input": {"name": "AGENT"}}),
    ];
    let shown: Vec<String> = calls.iter().map(crate::transcript::shown).collect();
    assert_eq!(shown, ["AGENT send", "AGENT list", "LOAD"]);
    s.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    s.conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": calls.clone()}));
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    let c = s.conv.lock().unwrap();
    let results = c.messages[2]["content"].as_array().unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r["call_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["s1", "l1", "k1"]);
    let text = |i: usize| results[i]["content"].to_string();
    assert!(text(0).contains("No agent 7.") && !text(0).contains("Invalid input"), "{}", text(0));
    assert!(text(1).contains("No agents yet") && results[1]["is_error"] != true, "{}", text(1));
    assert!(text(2).contains("AGENT is loaded already"), "{}", text(2));
}

/// `v` without its cache breakpoints, which move with every request and
/// are no part of what the model reads.
fn without_breakpoints(mut v: Value) -> Value {
    match &mut v {
        Value::Object(o) => {
            o.remove("cache_control");
            o.values_mut().for_each(|x| *x = without_breakpoints(x.take()));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| *x = without_breakpoints(x.take())),
        _ => {}
    }
    v
}

/// What a request sends the model as its provider's wire body has it: the
/// system prompt, the tools and the messages.
type Sent = (Value, Value, Vec<Value>);

fn claude_sent(r: &oak::Request) -> Sent {
    let b = without_breakpoints(api::claude_body(r));
    (b["system"].clone(), b["tools"].clone(), b["messages"].as_array().unwrap().clone())
}

fn codex_sent(r: &oak::Request) -> Sent {
    let b = without_breakpoints(crate::codex::request_body(r));
    (b["instructions"].clone(), b["tools"].clone(), b["input"].as_array().unwrap().clone())
}

fn generic_sent(r: &oak::Request) -> Sent {
    let b = without_breakpoints(crate::wire_anthropic::body(r, "openrouter", false));
    (b["system"].clone(), b["tools"].clone(), b["messages"].as_array().unwrap().clone())
}

/// `b` sends all `a` sent, unchanged, at its start (where they differ,
/// briefly).
fn assert_starts_with(a: &Sent, b: &Sent, what: &str) {
    let short = |v: &Value| v.to_string().chars().take(300).collect::<String>();
    let diff = |x: &Value, y: &Value| {
        let (x, y) = (x.to_string(), y.to_string());
        let i = x.chars().zip(y.chars()).take_while(|(a, b)| a == b).count();
        let around = |s: &str| s.chars().skip(i.saturating_sub(80)).take(240).collect::<String>();
        format!("from char {i}:\n  {}\n  {}", around(&x), around(&y))
    };
    assert!(a.0 == b.0, "{what}: the system prompt differs {}", diff(&a.0, &b.0));
    assert!(a.1 == b.1, "{what}: the tools differ {}", diff(&a.1, &b.1));
    assert!(a.2.len() <= b.2.len(), "{what}: {} messages, not {} (last: {})", b.2.len(), a.2.len(), b.2.last().map(short).unwrap_or_default());
    for (i, (x, y)) in a.2.iter().zip(&b.2).enumerate() {
        assert!(x == y, "{what}: message {i} differs {}", diff(x, y));
    }
}

/// `next` sends all `prev` sent, unchanged, at its start.
fn assert_grows(prev: &Sent, next: &Sent, what: &str) {
    assert_eq!(prev.0, next.0, "{what}: the system prompt changed");
    assert_eq!(prev.1, next.1, "{what}: the tools changed");
    assert!(next.2.len() >= prev.2.len(), "{what}: messages went");
    for (i, (a, b)) in prev.2.iter().zip(&next.2).enumerate() {
        assert_eq!(a, b, "{what}: message {i} changed");
    }
}

/// The prefix invariant end to end (`crate::prefix`), as each provider's
/// wire body has it: whatever happens between requests (the date, effort,
/// model, account or cache expiring, notes, a goal, a kit loading,
/// instruction files changing, old tool output, smart context turned off
/// and on, a save and reload, a fork), every later request sends what an
/// earlier one sent, unchanged, at its start. Only a summary changes it.
fn prefix_only_grows(name: &str, conv: Conversation, other_model: &str, sent: fn(&oak::Request) -> Sent) {
    let folder = std::env::temp_dir().join(format!("oak-prefix-{name}-{}", api::now_ms()));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("AGENTS.md"), "Rule one.").unwrap();
    let (w, s) = idle_worker(name, Conversation { effort: "high".into(), folder: folder.clone(), messages: vec![json!({"role": "user", "content": [oak::text("start")]})].into(), ..conv });
    let req = |w: &Worker| sent(&w.request_body().0);
    let reply = |text: &str, next: &str| {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [oak::text(text)]}));
        c.messages.push(json!({"role": "user", "content": [oak::text(next)]}));
    };
    let mut prev = req(&w);
    let flat = |x: &Sent| serde_json::to_string(&(&x.0, &x.2)).unwrap();
    assert!(flat(&prev).contains("Rule one."));
    let step = |prev: &mut Sent, w: &Worker, what: &str| {
        let next = req(w);
        assert_grows(prev, &next, what);
        *prev = next;
        prev.clone()
    };
    // The model loads a kit and takes a note.
    s.conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": [
        {"type": "tool_call", "id": "l1", "name": "LOAD", "input": {"name": "AGENT"}},
        {"type": "tool_call", "id": "n1", "name": "NOTE", "input": {"action": "new", "text": "the db is postgres"}}]}));
    let calls = s.conv.lock().unwrap().messages[1]["content"].as_array().unwrap().clone();
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    step(&mut prev, &w, "a kit loaded, a note taken");
    // The effort, the date and the model change.
    s.conv.lock().unwrap().effort = "low".into();
    reply("ok", "go on");
    step(&mut prev, &w, "another effort");
    s.conv.lock().unwrap().prefix.pin.as_mut().unwrap().day = "2000-01-01".into();
    reply("ok", "and on");
    assert!(flat(&step(&mut prev, &w, "another day")).contains("Today's date is now "));
    s.conv.lock().unwrap().model = other_model.into();
    reply("ok", "still on");
    assert!(flat(&step(&mut prev, &w, "another model")).contains("Your underlying model is now "));
    // The cache expires or the account changes; a goal is set; the
    // instruction files change; old tool output piles up.
    s.conv.lock().unwrap().cache.cold();
    reply("ok", "after a pause");
    step(&mut prev, &w, "a cold cache");
    {
        let mut c = s.conv.lock().unwrap();
        let (block, _) = apply_command(&mut c, Command::Goal("ship v2".into(), false));
        c.messages.push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages.push(json!({"role": "user", "content": [block.unwrap()]}));
    }
    step(&mut prev, &w, "a goal");
    std::fs::write(folder.join("AGENTS.md"), "Rule two.").unwrap();
    {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "cat big"}}]}));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "e1", "content": "x".repeat(300_000)}]}));
        c.cache.cold();
    }
    let after = step(&mut prev, &w, "instruction files changed, old tool output, cold again");
    assert!(!flat(&after).contains("Rule two.") && flat(&after).contains(&"x".repeat(300_000)));
    // Smart context off and on.
    s.conv.lock().unwrap().rolling.off = true;
    reply("ok", "off");
    step(&mut prev, &w, "smart context off");
    // (What a revert to here, past the summary below, sends again.)
    let (back_to, sent_then) = (s.conv.lock().unwrap().messages.len(), prev.clone());
    s.conv.lock().unwrap().rolling.off = false;
    reply("ok", "on");
    step(&mut prev, &w, "smart context on");
    // Saved and loaded: the same request, and it goes on growing.
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let (w2, s2) = idle_worker(&format!("{name}-reloaded"), serde_json::from_str(&saved).unwrap());
    assert_eq!(req(&w2), prev, "reloaded");
    s2.conv.lock().unwrap().cache.cold();
    s2.conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": [oak::text("back")]}));
    s2.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("again")]}));
    step(&mut prev, &w2, "reloaded, cold");
    // A fork sends what its source sent.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [oak::text("done")]}));
    }
    let source = req(&w);
    let forked = crate::side_chat::fork(&s.conv.lock().unwrap(), format!("unit-test-{name}-fork-{}", api::now_ms()), 0);
    let (w3, _s3) = idle_worker(&format!("{name}-fork"), forked);
    assert_grows(&source, &req(&w3), "a fork");
    // A summary: the one moment the prefix changes. The note joins the
    // session memory, the kit the prefix, the effort and model as they are.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "user", "content": [oak::text("more")]}));
        let cut = c.messages.len() - 2;
        install_summary(&mut c, crate::rolling::Summary { cut, text: "Loaded AGENT, noted the db, set a goal.".into(), made_at: 1, clear: cut }, 0, Default::default());
    }
    let summarized = req(&w);
    assert_ne!(summarized.0, prev.0, "the system prompt is brought up to date");
    let system = serde_json::to_string(&summarized.0).unwrap();
    assert!(system.contains("the db is postgres") && system.contains("ship v2"), "{system}");
    assert!(flat(&summarized).contains("Rule two.") && !flat(&summarized).contains(&"x".repeat(300_000)));
    prev = summarized;
    // After it, the same again.
    s.conv.lock().unwrap().effort = "high".into();
    s.conv.lock().unwrap().cache.cold();
    reply("ok", "after the summary");
    step(&mut prev, &w, "after the summary: effort, cold");
    s.conv.lock().unwrap().rolling.off = true;
    reply("ok", "off again");
    step(&mut prev, &w, "after the summary: smart context off keeps it");
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let (w4, s4) = idle_worker(&format!("{name}-reloaded-again"), serde_json::from_str(&saved).unwrap());
    assert_eq!(req(&w4), prev, "reloaded after the summary");
    // A second summary, and the session memory changes since.
    {
        let mut c = s4.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages.push(json!({"role": "user", "content": [oak::text("later")]}));
        memory_tool(&mut c, "NOTE_NEW", &json!({"text": "the cache is redis"}), 0).unwrap();
        let cut = c.messages.len() - 1;
        install_summary(&mut c, crate::rolling::Summary { cut, text: "Later.".into(), made_at: 2, clear: cut }, 0, Default::default());
    }
    assert!(serde_json::to_string(&req(&w4).0).unwrap().contains("the cache is redis"));
    // The user reverts (or edits a message) to before both summaries' cuts:
    // what was sent then is sent again, exactly, from the state saved.
    assert!(s4.conv.lock().unwrap().rolling.summary.as_ref().is_some_and(|s| s.cut > back_to));
    assert!(revert_to_end(&mut s4.conv.lock().unwrap(), back_to));
    let saved = serde_json::to_string(&*s4.conv.lock().unwrap()).unwrap();
    let (w5, s5) = idle_worker(&format!("{name}-reverted"), serde_json::from_str(&saved).unwrap());
    // (Then, as after any pause, what changed since is added: here the
    // effort.)
    let back = req(&w5);
    assert_starts_with(&sent_then, &back, "reverted to before the summaries, reloaded");
    assert_eq!(back.2.len(), sent_then.2.len() + usize::from(name == "prefix-claude"), "only the effort is added");
    assert_starts_with(&sent_then, &req(&w4), "reverted to before the summaries");
    let mut back = back;
    s5.conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": [oak::text("again")]}));
    s5.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("onwards")]}));
    step(&mut back, &w5, "after the revert");
    drop((w2, s2));
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn the_prefix_only_grows_on_claude() {
    prefix_only_grows("prefix-claude", Conversation { model: "claude-opus-5-5".into(), ..Default::default() }, "claude-sonnet-5-5", claude_sent);
}

#[test]
fn the_prefix_only_grows_on_codex() {
    let conv = Conversation { provider: Some(crate::providers::Kind::Codex), model: "gpt-5.6-sol".into(), ..Default::default() };
    prefix_only_grows("prefix-codex", conv, "gpt-5.6-terra", codex_sent);
}

#[test]
fn the_prefix_only_grows_where_system_messages_cant_go() {
    let conv = Conversation { provider: Some(crate::providers::Kind::OpenRouter), model: "openai/gpt-5".into(), ..Default::default() };
    prefix_only_grows("prefix-generic", conv, "deepseek/deepseek-v4-flash", generic_sent);
}

/// NOTE with the notes' limit at `limit` tokens on this thread (100 tokens:
/// up to 353 bytes of notes, `rolling::estimate`).
fn limited_notes(limit: u64) -> impl Fn(&mut Conversation, &str, Value) -> Result<String, String> {
    crate::agent::memory::set_test_notes_limit(Some(limit));
    |c: &mut Conversation, name: &str, input: Value| memory_tool(c, name, &input, 0)
}

fn note_lens(c: &Conversation) -> Vec<usize> {
    c.notes.iter().map(|n| n.text.len()).collect()
}

/// Notes up to the limit are saved; one byte past it is refused, and
/// nothing changes.
#[test]
fn notes_fill_up_to_their_limit() {
    let tool = limited_notes(100);
    let mut c = Conversation::default();
    tool(&mut c, "NOTE_NEW", json!({"text": "a".repeat(300)})).unwrap();
    let full = tool(&mut c, "NOTE_NEW", json!({"text": "b".repeat(53)})).unwrap();
    assert!(!full.contains("over their limit"), "{full}");
    assert_eq!(notes_tokens(&c.notes), 100);
    let err = tool(&mut c, "NOTE_NEW", json!({"text": "c"})).unwrap_err();
    assert!(err.starts_with("Not saved: it would take your notes to about 101 tokens, past their limit of 100"), "{err}");
    assert_eq!((note_lens(&c), c.next_note_id), (vec![300, 53], 2));
    // An update that would make a note longer past the limit: refused too.
    let err = tool(&mut c, "NOTE_UPDATE", json!({"id": 2, "text": "b".repeat(54)})).unwrap_err();
    assert!(err.starts_with("Not updated: it would make note 2 longer"), "{err}");
    assert_eq!(note_lens(&c), [300, 53]);
    // Room made, the same fit.
    tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "a".repeat(299)})).unwrap();
    tool(&mut c, "NOTE_NEW", json!({"text": "c"})).unwrap();
    assert_eq!(note_lens(&c), [299, 53, 1]);
    // Growing under the limit, and up to it, is fine.
    tool(&mut c, "NOTE_DELETE", json!({"id": 3})).unwrap();
    tool(&mut c, "NOTE_UPDATE", json!({"id": 2, "text": "b".repeat(54)})).unwrap();
    assert_eq!(notes_tokens(&c.notes), 100);
    crate::agent::memory::set_test_notes_limit(None);
}

/// The limit lowered under notes already taken: they are all kept, the
/// model told; only shortening or deleting goes through until they are
/// under it again.
#[test]
fn notes_over_a_lowered_limit_must_shrink_first() {
    let tool = limited_notes(1_000);
    let mut c = Conversation::default();
    for _ in 0..3 {
        tool(&mut c, "NOTE_NEW", json!({"text": "n".repeat(200)})).unwrap();
    }
    let tool = limited_notes(100);
    assert_eq!((note_lens(&c), notes_tokens(&c.notes)), (vec![200, 200, 200], 171));
    assert!(render_memory(&c).contains("over their limit of 100"));
    // No new note, however small; no update that makes a note longer.
    let err = tool(&mut c, "NOTE_NEW", json!({"text": "x"})).unwrap_err();
    assert!(err.starts_with("Not saved: your notes take about 171 tokens, over their limit of 100."), "{err}");
    assert!(tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "n".repeat(201)})).is_err());
    assert_eq!((note_lens(&c), c.next_note_id), (vec![200, 200, 200], 3));
    // Rewording at the same length, shrinking and deleting all go through,
    // each saying what is left to do while still over.
    let same = tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "m".repeat(200)})).unwrap();
    assert!(same.contains("over their limit of 100"), "{same}");
    let shrunk = tool(&mut c, "NOTE_UPDATE", json!({"id": 2, "text": "n".repeat(100)})).unwrap();
    assert!(shrunk.contains("about 142 tokens, over their limit"), "{shrunk}");
    let deleted = tool(&mut c, "NOTE_DELETE", json!({"id": 3})).unwrap();
    assert_eq!(deleted, "Deleted note 3.");
    assert_eq!((note_lens(&c), notes_tokens(&c.notes)), (vec![200, 100], 85));
    // Under it: adding works again, up to the limit.
    assert!(tool(&mut c, "NOTE_NEW", json!({"text": "x".repeat(54)})).is_err());
    tool(&mut c, "NOTE_NEW", json!({"text": "x".repeat(53)})).unwrap();
    crate::agent::memory::set_test_notes_limit(None);
}

/// With the Notes reminder off, notes over the limit still can't grow, but
/// the model is never nagged to shrink them: not in NOTE results, not in
/// the memory block.
#[test]
fn notes_over_the_limit_with_the_reminder_off_are_not_nagged() {
    let tool = limited_notes(1_000);
    let mut c = Conversation::default();
    for _ in 0..3 {
        tool(&mut c, "NOTE_NEW", json!({"text": "n".repeat(200)})).unwrap();
    }
    let tool = limited_notes(100);
    crate::agent::memory::set_test_notes_alert(Some(false));
    assert!(!render_memory(&c).contains("over their limit"), "{}", render_memory(&c));
    assert_eq!(tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "m".repeat(200)})).unwrap(), "Updated note 1.");
    assert_eq!(tool(&mut c, "NOTE_DELETE", json!({"id": 3})).unwrap(), "Deleted note 3.");
    // The hard limit itself still holds.
    assert!(tool(&mut c, "NOTE_NEW", json!({"text": "x"})).is_err());
    crate::agent::memory::set_test_notes_alert(None);
    crate::agent::memory::set_test_notes_limit(None);
}

/// An update crossing the limit: down across it goes through (and the
/// model is no longer told it is over), up across it is refused.
#[test]
fn note_updates_crossing_the_limit() {
    let tool = limited_notes(1_000);
    let mut c = Conversation::default();
    tool(&mut c, "NOTE_NEW", json!({"text": "a".repeat(500)})).unwrap();
    let tool = limited_notes(100);
    let down = tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "a".repeat(300)})).unwrap();
    assert!(!down.contains("over their limit"), "{down}");
    assert!(tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "a".repeat(400)})).is_err());
    tool(&mut c, "NOTE_UPDATE", json!({"id": 1, "text": "a".repeat(353)})).unwrap();
    assert_eq!(note_lens(&c), [353]);
    crate::agent::memory::set_test_notes_limit(None);
}

/// The "Notes limit" setting: 2,000 tokens by default (an older
/// settings.json without it too), kept across a save, brought into range.
#[test]
fn the_notes_limit_setting_persists() {
    assert_eq!(crate::store::Settings::default().notes_limit, 2_000);
    let older: crate::store::Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(older.notes_limit, 2_000);
    let mut set = crate::store::Settings::default();
    set.notes_limit = 8_000;
    let back: crate::store::Settings = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
    assert_eq!(back.notes_limit, 8_000);
    assert_eq!(crate::agent::memory::sane_notes_limit(0), 2_000);
    assert_eq!(crate::agent::memory::sane_notes_limit(5), 100);
    assert_eq!(crate::agent::memory::sane_notes_limit(4_000), 4_000);
    assert!(NOTES_LIMITS.iter().any(|&(n, _)| n == DEFAULT_NOTES_LIMIT));
}

/// The system prompt names the limit it was sent with. Changing the
/// setting mid-session leaves it (and everything sent) as it was and adds
/// a message, kept across a save and reload; a summary brings the system
/// prompt up to date. NOTE goes by the limit now throughout.
#[test]
fn a_new_notes_limit_keeps_the_prefix() {
    let limit = |n| crate::agent::memory::set_test_notes_limit(Some(n));
    limit(2_000);
    let conv = Conversation { model: "claude-opus-5-5".into(), messages: vec![json!({"role": "user", "content": [oak::text("start")]})].into(), ..Default::default() };
    let (w, s) = idle_worker("notes-limit", conv);
    let req = |w: &Worker| claude_sent(&w.request_body().0);
    let flat = |x: &Sent| serde_json::to_string(&(&x.0, &x.2)).unwrap();
    let mut prev = req(&w);
    let system = serde_json::to_string(&prev.0).unwrap();
    assert!(system.contains("Your notes may take at most about 2,000 tokens in all"), "{system}");
    let reply = |text: &str| {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages.push(json!({"role": "user", "content": [oak::text(text)]}));
    };
    memory_tool(&mut s.conv.lock().unwrap(), "NOTE_NEW", &json!({"text": "n".repeat(3_500)}), 0).unwrap();
    reply("noted");
    let next = req(&w);
    assert_grows(&prev, &next, "a note");
    prev = next;
    // Lowered under the notes: a message, the prefix as it was.
    limit(500);
    reply("go on");
    let next = req(&w);
    assert_grows(&prev, &next, "a lower limit");
    let told = "The limit on your notes is now about 500 tokens in all (it was 2,000). They take about 1000 now, over it";
    assert!(flat(&next).contains(told), "{}", flat(&next));
    assert!(serde_json::to_string(&next.0).unwrap().contains("about 2,000 tokens in all"));
    prev = next;
    assert!(memory_tool(&mut s.conv.lock().unwrap(), "NOTE_NEW", &json!({"text": "more"}), 0).is_err());
    memory_tool(&mut s.conv.lock().unwrap(), "NOTE_UPDATE", &json!({"id": 1, "text": "n".repeat(1_000)}), 0).unwrap();
    // Told once.
    reply("and on");
    let next = req(&w);
    assert_grows(&prev, &next, "the same limit");
    assert_eq!(flat(&next).matches("The limit on your notes is now").count(), 1);
    prev = next;
    // Saved and loaded: the same request.
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let (w2, _s2) = idle_worker("notes-limit-reloaded", serde_json::from_str(&saved).unwrap());
    assert_eq!(req(&w2), prev, "reloaded");
    // A summary: the system prompt names the limit now, and no message does.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages.push(json!({"role": "user", "content": [oak::text("more")]}));
        let cut = c.messages.len() - 1;
        install_summary(&mut c, crate::rolling::Summary { cut, text: "Noted things.".into(), made_at: 1, clear: cut }, 0, Default::default());
    }
    let summarized = req(&w);
    let system = serde_json::to_string(&summarized.0).unwrap();
    assert!(system.contains("Your notes may take at most about 500 tokens in all"), "{system}");
    assert!(!flat(&summarized).contains("The limit on your notes is now"));
    crate::agent::memory::set_test_notes_limit(None);
}

/// AGENT_PAUSE and AGENT_RESUME by model pick the agents on it by id,
/// family word or provider, and nothing that merely shares a version.
#[test]
fn group_pause_picks_by_model() {
    use crate::providers::Kind;
    use super::subagents::on_model;
    for (word, model, kind) in [
        ("opus", "claude-opus-4-7", Kind::Claude),
        ("Opus", "anthropic/claude-opus-4", Kind::OpenRouter),
        ("claude-opus-4-7", "claude-opus-4-7", Kind::Claude),
        ("sol", "gpt-6.1-sol", Kind::Codex),
        ("gpt", "gpt-6.1-sol", Kind::Codex),
        ("codex", "gpt-6.1-sol", Kind::Codex),
        ("claude", "claude-sonnet-5", Kind::Claude),
    ] {
        assert!(on_model(word, model, kind), "{word} {model}");
    }
    for (word, model, kind) in [
        ("opus", "gpt-6.1-sol", Kind::Codex),
        ("sonnet", "claude-opus-4-7", Kind::Claude),
        ("4", "claude-opus-4-7", Kind::Claude),
        ("6.1", "gpt-6.1-sol", Kind::Codex),
        ("codex", "claude-opus-4-7", Kind::Claude),
        ("op", "claude-opus-4-7", Kind::Claude),
    ] {
        assert!(!on_model(word, model, kind), "{word} {model}");
    }
}

/// A session's pulse: its last progress (a reply, a tool's result by its
/// tool's name), kept with the conversation; a wait its status line names
/// is what blocks it, and so is the error its last turn ended on.
#[test]
fn the_pulse_says_progress_and_blockers() {
    let (w, s) = idle_worker("pulse", Conversation::default());
    let p = super::pulse::Pulse::of(&s);
    assert_eq!((p.phase, p.progress_at, p.blocker.as_str()), (super::pulse::IDLE, 0, ""));
    w.set(|l| {
        l.busy = true;
        l.status = "Waiting for the model".into();
    });
    let p = super::pulse::Pulse::of(&s);
    assert_eq!(p.phase, super::pulse::MODEL);
    assert!(p.since > 0 && p.detail(api::now_ms()).ends_with("no progress yet"), "{p:?}");

    w.push("assistant", vec![json!({"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "echo hush", "summary": "Say"}})]);
    assert_eq!(s.conv.lock().unwrap().progress, "Its model replied with 1 tool call");
    w.set(|l| l.status = "Running Say".into());
    w.push("user", vec![oak::tool_result(&json!("e1"), json!("hush"), false)]);
    let p = super::pulse::Pulse::of(&s);
    assert_eq!((p.phase, p.progress.as_str()), (super::pulse::TOOLS, "EXECUTE returned"));
    assert!(!format!("{p:?}").contains("echo hush"));

    w.set(|l| l.status = "Retry 1/5 in 3s after: HTTP 529".into());
    let p = super::pulse::Pulse::of(&s);
    assert_eq!((p.phase, p.blocker.as_str()), (super::pulse::WAITING, "Retry 1/5 in 3s after: HTTP 529"));

    w.set(|l| {
        l.busy = false;
        l.status.clear();
        l.error = Some("Anthropic answered HTTP 500:\n overloaded".into());
    });
    let p = super::pulse::Pulse::of(&s);
    assert_eq!((p.phase, p.blocker.as_str()), (super::pulse::BLOCKED, "Its last turn ended on an error: Anthropic answered HTTP 500: overloaded"));
    // The last progress is saved with the conversation.
    let saved = serde_json::to_value(&*s.conv.lock().unwrap()).unwrap();
    assert_eq!(saved["progress"], "EXECUTE returned");
}
