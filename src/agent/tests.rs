//! Agent loop regression and live smart-context tests.

use memory::*;
use tool_calls::*;
use super::*;
use super::reply::spread_thinking_tokens;

#[test]
fn clearing_a_goal_waits_for_the_next_request() {
    let mut c = Conversation::default();
    apply_command(&mut c, Command::Goal("ship it".into()));
    let (block, mark) = apply_command(&mut c, Command::Goal(String::new()));
    assert!(block.is_none(), "a cleared goal gives the model nothing to do");
    assert_eq!(mark.0, MarkKind::Goal);
    assert!(c.held[0].contains("cleared the goal"));
    // A new goal supersedes the waiting note.
    let (block, _) = apply_command(&mut c, Command::Goal("again".into()));
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
    let (s, w) = Session::with_worker(unit_conv(name, conv), eframe::egui::Context::default(), false);
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
    // The request ends: paused now, and the queue is done with.
    w.end_request();
    assert!(paused() && !s.pause_queued());
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
#[test]
fn goal_runs_until_finished() {
    let goal = "Do X, Y, and then Z.";
    let mut c = Conversation::default();
    let tool = |c: &mut Conversation, name: &str, input: Value| memory_tool(c, name, &input, 0);
    // No goal: nothing to justify or finish, and turns end as usual.
    assert!(tool(&mut c, "GOAL_JUSTIFICATION", json!({"justification": "x"})).is_err());
    assert!(tool(&mut c, "GOAL_FINISH", json!({})).is_err());
    assert_eq!(after_turn(&mut c, false, 0), None);

    let (block, _) = apply_command(&mut c, Command::Goal(goal.into()));
    assert_eq!(block.unwrap()["text"], goal_prompt(goal));
    // Ending the turn without finishing: asked to carry on, not when interrupted.
    let more = after_turn(&mut c, false, 5).expect("asked to continue");
    assert_eq!(more["text"], goal_continue(goal));
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

/// Kits and notes on a stable prefix (Claude): LOAD adds AGENT's tool in
/// a system message while the cache is warm, the prompt and tools as
/// they were; at the next cold moment the tool and its prompt block join
/// the prefix and the LOAD result and NOTE text are folded.
#[test]
fn kits_and_notes_wait_for_a_cold_moment() {
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
    // Cold: everything as it is now, the duplicates folded.
    s.conv.lock().unwrap().cache.cold();
    let (r4, ..) = w.request_body();
    assert!(names(&r4).contains(&"AGENT".to_string()) && r4.system.contains("Subagents (AGENT) suit") && r4.system.contains("[1] the db is postgres"));
    assert_eq!(r4.effort.as_deref(), Some("low"));
    assert!(r4.messages.iter().all(|m| m["role"] != "system"));
    let flat = serde_json::to_string(&r4.messages).unwrap();
    assert!(flat.contains("Loaded AGENT: it is in your tools and system prompt now.") && !flat.contains("Subagents (AGENT) suit"));
    assert!(flat.contains(crate::prefix::NOTE_FOLDED) && !flat.contains("the db is postgres"));
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

    apply_command(&mut s.conv.lock().unwrap(), Command::Goal("ship it".into()));
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
    apply_command(&mut s.conv.lock().unwrap(), Command::Goal(String::new()));
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

/// The memory block (goal, notes) changes only when the prompt cache is
/// being rebuilt anyway: after a summary, or a cold cache.
#[test]
fn memory_changes_only_with_the_cache() {
    let mut c = Conversation::default();
    assert_eq!(memory_for_request(&mut c, 0, false), "");
    let (block, mark) = apply_command(&mut c, Command::Goal("ship v2".into()));
    let block = block.unwrap();
    assert!(block["text"].as_str().unwrap().starts_with(GOAL_TAG) && is_hidden_block(&block));
    assert_eq!(mark, (MarkKind::Goal, "ship v2".into()));
    memory_tool(&mut c, "NOTE_NEW", &json!({"text": "the db is postgres"}), 0).unwrap();
    // Requests keep the cache warm: the block stays as it was.
    let ttl = c.cache.ttl_ms();
    assert_eq!(memory_for_request(&mut c, ttl / 2, false), "", "a pause shorter than the cache's lifetime");
    assert_eq!(memory_for_request(&mut c, ttl + ttl / 2 - 1_000, false), "");
    // After a quiet cache lifetime the cache is gone anyway: bring it up to date.
    let later = 2 * ttl + ttl / 2 + 10_000;
    let m = memory_for_request(&mut c, later, false);
    assert!(m.contains("# Session memory") && m.contains("ship v2") && m.contains("[1] the db is postgres"), "{m}");
    // A summary moves the cut, which rewrites the cache too.
    memory_tool(&mut c, "NOTE_NEW", &json!({"text": "second note"}), 0).unwrap();
    assert!(!memory_for_request(&mut c, later + 1_000, false).contains("second note"));
    c.rolling.apply(Summary { cut: 2, text: "s".into(), made_at: 0, ..Default::default() }, 0, Default::default());
    assert!(memory_for_request(&mut c, later + 2_000, false).contains("[2] second note"));
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
        marks: vec![Mark { at: 3, kind: MarkKind::Effort, text: "low".into(), time: 0, done: 0, sent: String::new() }],
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
