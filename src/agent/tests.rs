//! Agent loop regression and live smart-context tests.

use super::reply::spread_thinking_tokens;
use super::*;
use memory::*;
use tool_calls::*;

#[test]
fn clearing_a_goal_waits_for_the_next_request() {
    let mut c = Conversation::default();
    apply_command(&mut c, Command::Goal("ship it".into(), false));
    let (block, mark) = apply_command(&mut c, Command::Goal(String::new(), false));
    assert!(
        block.is_none(),
        "a cleared goal gives the model nothing to do"
    );
    assert_eq!(mark.unwrap().0, MarkKind::Goal);
    assert!(c.held[0].contains("cleared the goal"));
    // A new goal supersedes the waiting note.
    let (block, _) = apply_command(&mut c, Command::Goal("again".into(), false));
    assert!(block.is_some() && c.held.is_empty());
}

/// A reply's thinking-token count goes on its thinking blocks: all of it
/// on one, shared by length over several (adding up), none for a 0.
#[test]
fn thinking_tokens_spread_over_blocks() {
    let mut one = vec![
        json!({"type": "reasoning", "text": "plan"}),
        json!({"type": "text", "text": "hi"}),
    ];
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
    let n: Vec<u64> = [0, 2, 3]
        .iter()
        .map(|&i| two[i]["_oak_tokens"].as_u64().unwrap())
        .collect();
    assert_eq!(
        n,
        [50, 16, 35],
        "the redacted block weighs as the visible ones on average"
    );
    assert_eq!(n.iter().sum::<u64>(), 101);
}

/// A message Quick's agent took in mid-turn isn't in Quick's requests (it
/// would look new there once Oak's keys are dropped, and go again), but is
/// in any other provider's.
#[test]
fn a_message_quick_took_in_is_left_out_of_its_requests() {
    let messages = vec![
        json!({"role": "user", "content": [oak::text("fix the bug")]}),
        json!({"role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "run_python_on_device", "input": {}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "ok"}, {"type": "text", "text": "NO TESTING", crate::quick::STEERED_KEY: true}]}),
    ];
    let quick = for_provider(&messages, api::Provider::Quick);
    assert_eq!(quick[2]["content"], json!([{"type": "tool_result", "call_id": "c1", "content": "ok"}]));
    assert_eq!(quick[0], messages[0]);
    assert!(matches!(
        for_provider(&messages, api::Provider::Claude),
        std::borrow::Cow::Borrowed(_)
    ));
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
    assert!(matches!(
        for_provider(&c.messages, api::Provider::Claude),
        std::borrow::Cow::Borrowed(_)
    ));
    let (block, mark) = apply_command(
        &mut c,
        Command::Model(api::Provider::Codex, "gpt-6-luna".into()),
    );
    assert!(
        block.is_none() && mark.is_none(),
        "a switch is seamless: no block, no transcript line"
    );
    assert_eq!(
        (c.model.as_str(), c.effort.as_str(), c.served_by.as_str()),
        ("gpt-6-luna", "max", "")
    );
    c.messages
        .push(json!({"role": "user", "content": [oak::text("more")]}));
    c.messages
        .push(json!({"role": "assistant", "content": [think("codex"), oak::text("ok")]}));
    let sent = for_provider(&c.messages, api::Provider::Codex);
    assert_eq!(
        sent[1]["content"],
        json!([{"type": "tool_call", "id": "t1", "name": "READ", "input": {}}]),
        "Claude's thinking left out for Codex"
    );
    assert_eq!(
        sent[3]["content"],
        json!([oak::text("(No reply text.)")]),
        "a reply of thinking only keeps a line"
    );
    assert_eq!(
        sent[5]["content"][0]["type"], "reasoning",
        "its own thinking stays"
    );
    // Switching to the model it is on changes nothing.
    let before = c.messages.clone();
    apply_command(
        &mut c,
        Command::Model(api::Provider::Codex, "gpt-6-luna".into()),
    );
    assert_eq!(c.messages, before);
}

/// A reply's blocks are marked with the provider that wrote them (moved
/// here as providers are: `for_provider`), never sent.
#[test]
fn replies_are_marked_with_their_provider() {
    let mut blocks = vec![oak::text("hi"), json!({"type": "tool_call", "id": "c", "name": "READ", "input": {}})];
    mark_by(&mut blocks, Some(api::Provider::Codex));
    let marked = vec![json!({"role": "assistant", "content": blocks})];
    assert_eq!(written_by(&marked[0]), Some("codex"));
    assert!(marked[0]["content"][0][BY_KEY].is_string());
    assert!(crate::rolling::view(&marked, None)[0]["content"][0].get(BY_KEY).is_none(), "never sent");
}

/// For a provider that keeps the conversation itself (Quick), what another
/// provider's model wrote since its own last reply is sent as well, so a
/// switch to it loses nothing; with none of its replies in view, all of it.
#[test]
fn a_thread_hears_what_other_models_wrote() {
    let reply = |by: &str, text: &str| {
        let mut blocks = vec![oak::text(text)];
        mark_by(&mut blocks, api::Provider::from_id(by));
        json!({"role": "assistant", "content": blocks})
    };
    let user = |t: &str| json!({"role": "user", "content": [oak::text(t)]});
    let transcript = vec![user("do it"), reply("quick", "on it"), user("more"), reply("claude", "done"), user("and now")];
    let asked = unseen_by(&transcript, &transcript, api::Provider::Quick, 0);
    assert_eq!(asked, transcript[2..4].to_vec(), "what came after Quick's own reply, without the new message");
    let after = [transcript.clone(), vec![reply("quick", "done too")]].concat();
    assert!(unseen_by(&after, &after, api::Provider::Quick, 0).is_empty());
    // No reply of its own since the summary: the summary and all after it.
    let old = vec![user("summarized"), reply("claude", "a"), user("b"), reply("claude", "c"), user("d")];
    assert_eq!(unseen_by(&old, &old, api::Provider::Quick, 0), old[0..4].to_vec());
    // Nothing says another model wrote it (an older Oak's replies): nothing sent.
    let plain = vec![user("a"), json!({"role": "assistant", "content": [oak::text("b")]}), user("c")];
    assert!(unseen_by(&plain, &plain, api::Provider::Quick, 0).is_empty());
    // Under a summary (cut 0), the view carries it first: the index of a
    // transcript message is its own, plus the summary's two.
    let summary = vec![user("Summary."), reply("claude", "ack")];
    let view = [summary.clone(), transcript.clone()].concat();
    assert_eq!(
        unseen_by(&transcript, &view, api::Provider::Quick, 0),
        view[4..6].to_vec()
    );
    // A cut past Quick's reply: the summary and all after it.
    let cut_view = [summary, transcript[2..].to_vec()].concat();
    assert_eq!(
        unseen_by(&transcript, &cut_view, api::Provider::Quick, 2),
        cut_view[0..4].to_vec()
    );
}

/// A conversation moves between providers seamlessly: what one provider
/// can't take, or mustn't see twice, stays out of another's request.
#[test]
fn a_switch_leaves_out_what_the_provider_cannot_take() {
    let freebuff = crate::freebuff_tools::KEY;
    let quiet = crate::quick::QUIET_KEY;
    let messages = vec![
        json!({"role": "user", "content": [oak::text("hi")]}),
        json!({"role": "assistant", "content": [
            json!({"type": "tool_call", "id": "snip", "name": "run_python_on_device", quiet: true, "input": {"code": "READ(path=\"a\") "}}),
            json!({"type": "tool_call", "id": "inner", "name": "READ", "input": {"path": "a"}}),
        ]}),
        json!({"role": "user", "content": [
            json!({"type": "tool_result", "call_id": "snip", "content": "ran it"}),
            json!({"type": "tool_result", "call_id": "inner", "content": "fn main"}),
        ]}),
        json!({"role": "assistant", "content": [
            json!({"type": "tool_call", "id": "f1", "name": "READ", "input": {"path": "b", freebuff: {"name": "read_files", "args": {}}}}),
        ]}),
    ];
    // Quick keeps its own snippets: the others don't run its tool.
    let claude = for_provider(&messages, api::Provider::Claude).into_owned();
    let blocks = &claude[1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "the snippet's call stays out: {blocks:?}");
    assert_eq!(claude[2]["content"].as_array().unwrap().len(), 1, "its result too");
    assert!(claude[3]["content"][0]["input"].get(freebuff).is_none(), "Freebuff's own call goes only to Freebuff");
    assert_eq!(claude[3]["content"][0]["input"], json!({"path": "b"}));
    // Freebuff keeps the call it made, and loses the snippet's with the rest.
    let free = for_provider(&messages, api::Provider::Freebuff).into_owned();
    assert!(free[3]["content"][0]["input"].get(freebuff).is_some());
    assert_eq!(free[2]["content"].as_array().unwrap().len(), 1);
    // Quick takes its own as it is, and Oak's wrapped calls without
    // Freebuff's own record of them.
    let quick = for_provider(&messages, api::Provider::Quick).into_owned();
    assert_eq!(quick[1]["content"].as_array().unwrap().len(), 2);
    assert_eq!(quick[2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(quick[3]["content"][0]["input"], json!({"path": "b"}));
}

/// A failed model's conversation moves to the first of its fallbacks that
/// can take the request: not its model now, nor one that failed this turn,
/// nor one a chat can't use, nor one that can't send now.
#[test]
fn a_fallback_is_the_first_that_can_take_the_request() {
    let f = |p: api::Provider, m: &str| store::Fallback {
        provider: p,
        model: m.into(),
    };
    let mut c = Conversation {
        provider: Some(api::Provider::Claude),
        model: "claude-opus-5-5".into(),
        fallbacks: vec![
            f(api::Provider::Claude, "claude-opus-5-5"),
            f(api::Provider::Freebuff, "freebuff"),
            f(api::Provider::Codex, "gpt-6-luna"),
            f(api::Provider::OpenRouter, "deepseek/deepseek-v4.1-flash"),
            f(api::Provider::Claude, "claude-sonnet-5-5"),
        ],
        ..Default::default()
    };
    let any = |_: api::Provider, _: &str| true;
    assert_eq!(
        pick_fallback(&c, &[], any),
        Some(f(api::Provider::Freebuff, "freebuff")),
        "never the model it is on"
    );
    c.chat = true;
    assert_eq!(
        pick_fallback(&c, &[], any),
        Some(f(api::Provider::Codex, "gpt-6-luna")),
        "a chat skips Freebuff"
    );
    let tried = [(api::Provider::Codex, "gpt-6-luna".to_string())];
    assert_eq!(
        pick_fallback(&c, &tried, any).unwrap().provider,
        api::Provider::OpenRouter,
        "one that failed this turn is skipped"
    );
    let no_openrouter = |p: api::Provider, _: &str| p != api::Provider::OpenRouter;
    assert_eq!(
        pick_fallback(&c, &tried, no_openrouter),
        Some(f(api::Provider::Claude, "claude-sonnet-5-5")),
        "nor one that can't send"
    );
    assert_eq!(pick_fallback(&c, &tried, |_, _| false), None);
    c.fallbacks.clear();
    assert_eq!(pick_fallback(&c, &[], any), None);
}

/// Falling back switches the conversation as the session box does (the
/// effort stays, the cache goes cold), ends a wait for the old provider's
/// limit and says so in the transcript; with every fallback tried, the
/// turn fails as before. A model the user picked meanwhile goes first.
#[test]
fn falling_back_moves_the_conversation() {
    let fallbacks = vec![
        store::Fallback {
            provider: api::Provider::Codex,
            model: "gpt-6-luna".into(),
        },
        store::Fallback {
            provider: api::Provider::OpenRouter,
            model: "deepseek/deepseek-v4.1-flash".into(),
        },
    ];
    let conv = Conversation {
        provider: Some(api::Provider::Claude),
        model: "claude-opus-5-5".into(),
        effort: "high".into(),
        limit_wait: Some(api::now_ms() + 3_600_000),
        fallbacks,
        ..Default::default()
    };
    let (w, s) = idle_worker("fall-back", conv);
    let any = |_: &str, _: api::Provider, _: &str| true;
    let mut tried = vec![];
    assert!(w.fall_back_if("Overloaded", &mut tried, any));
    {
        let c = s.conv.lock().unwrap();
        assert_eq!(
            (c.provider(), c.model.as_str(), c.effort.as_str()),
            (api::Provider::Codex, "gpt-6-luna", "high")
        );
        assert!(c.limit_wait.is_none());
        let m = c.marks.last().unwrap();
        assert_eq!(m.kind, MarkKind::FellBack);
        assert!(
            m.text.ends_with("on Codex, after: Overloaded"),
            "{}",
            m.text
        );
    }
    assert!(w.fall_back_if("Stream ended", &mut tried, any));
    assert_eq!(s.conv.lock().unwrap().provider(), api::Provider::OpenRouter);
    assert!(
        !w.fall_back_if("again", &mut tried, any),
        "every fallback failed this turn"
    );
    assert_eq!(
        tried.len(),
        3,
        "the model it started on, then each fallback"
    );
    s.set_fallbacks(vec![store::Fallback {
        provider: api::Provider::Claude,
        model: "claude-opus-5-5".into(),
    }]);
    s.command(Command::Model(
        api::Provider::Claude,
        "claude-sonnet-5-5".into(),
    ));
    assert!(
        !w.fall_back_if("x", &mut vec![], any),
        "the user's pick wins"
    );
}

/// Messages between sessions carry who sent them, and whether it is a
/// final reply.
#[test]
fn agent_messages_round_trip() {
    let m = agent_message(&agent_label(2, "Look \"here\""), true, "  done.\n");
    assert_eq!(
        m,
        "<agent-message from=\"agent 2 (Look 'here')\" type=\"final reply\">\ndone.\n</agent-message>"
    );
    assert_eq!(
        parse_agent_message(&m),
        Some(("agent 2 (Look 'here')", true, "done."))
    );
    assert_eq!(
        parse_agent_message(&agent_message(MAIN_LABEL, false, "go")),
        Some(("main session", false, "go"))
    );
    assert_eq!(parse_agent_message("<agent-messages>"), None);
}

/// `conv` as a `unit-test-{name}` conversation, in the temp folder unless
/// it has one.
fn unit_conv(name: &str, conv: Conversation) -> Conversation {
    let folder = if conv.folder.as_os_str().is_empty() {
        std::env::temp_dir()
    } else {
        conv.folder
    };
    Conversation {
        id: format!("unit-test-{name}-{}", api::now_ms()),
        folder,
        ..conv
    }
}

/// A test's session, deleted (unregistered, its tasks killed and its
/// conversation removed) when dropped.
pub(super) struct Deleting(Session);

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
pub(super) fn idle_worker(name: &str, conv: Conversation) -> (Worker, Deleting) {
    let conv = unit_conv(name, conv);
    let tasks = Tasks::open(&conv.id);
    let (s, w) = Session::with_worker(conv, eframe::egui::Context::default(), false, tasks);
    SESSIONS
        .lock()
        .unwrap()
        .insert(s.conv.lock().unwrap().id.clone(), s.clone());
    (w, Deleting(s))
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}"
        );
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
    assert_eq!(mark, Some((MarkKind::Goal, goal.into())));
    assert!(c.goal_endless && render_memory(&c).contains("as endless"));
    assert!(
        tool(
            &mut c,
            "GOAL_JUSTIFICATION",
            json!({"justification": "done"})
        )
        .is_err()
    );
    assert!(tool(&mut c, "GOAL_FINISH", json!({})).is_err());
    assert!(c.goal_justification.is_empty() && !c.goal_finished);
    let more = after_turn(&mut c, false, 5).expect("asked to continue");
    assert_eq!(more["text"], goal_continue(goal, true));
    assert_eq!(c.goal, goal);
    // Set again without the mode: it can be finished.
    apply_command(&mut c, Command::Goal(goal.into(), false));
    assert!(
        !c.goal_endless
            && tool(
                &mut c,
                "GOAL_JUSTIFICATION",
                json!({"justification": "done"})
            )
            .is_ok()
    );
    // Clearing ends the mode with the goal.
    apply_command(&mut c, Command::Goal(goal.into(), true));
    apply_command(&mut c, Command::Goal(String::new(), true));
    assert!(c.goal.is_empty() && !c.goal_endless);
}

#[test]
fn quick_recovery_keeps_retrying_in_the_same_turn_and_stops_on_success() {
    use crate::quick::recovery;
    let (w, s) = idle_worker("quick-retry-turn", Conversation {
        model: "smart".into(),
        messages: vec![json!({"role": "user", "content": [oak::text("Continue the work")]})].into(),
        ..Default::default()
    });
    let mut request = 0;
    let mut delays = vec![];
    std::thread::scope(|scope| w.turn_loop_with(scope, |_| {
        let (req, ..) = w.request_body();
        let text = crate::quick::new_user_text(&req).unwrap();
        let stop = match request {
            0 => { assert_eq!(text, "Continue the work"); recovery::ERROR }
            1 => { assert_eq!(text, recovery::ERROR_RETRY); recovery::DEVICE }
            2..=6 => { assert_eq!(text, recovery::DEVICE_RETRY); recovery::DEVICE }
            7 => { assert_eq!(text, recovery::DEVICE_RETRY); oak::stop::COMPLETE }
            _ => panic!("A successful turn must not be retried"),
        };
        request += 1;
        Reply::Done(vec![oak::text(if request == 8 { "Finished" } else { "Partial reply" })], stop.into(), Usage::default())
    }, |attempts, _| {
        *attempts += 1;
        delays.push(api::retry_wait(*attempts));
        true
    }));
    assert_eq!(request, 8);
    assert_eq!(delays, [4, 4, 8, 16, 32, 90, 90]);
    assert_eq!(reply_text(&s.conv.lock().unwrap()).as_deref(), Some("Finished"));
}

#[test]
fn a_quick_handoff_keeps_the_chat_and_seeds_a_fresh_thread() {
    let (w, s) = idle_worker("quick-handoff-test", Conversation {
        provider: Some(api::Provider::Quick),
        model: "smart".into(),
        messages: vec![
            json!({"role": "user", "content": [oak::text("Original request")]}),
            json!({"role": "assistant", "content": [oak::text("Partial work")]})
        ].into(),
        ..Default::default()
    });
    let (id, parent) = { let c = s.conv.lock().unwrap(); (c.id.clone(), c.parent.clone()) };
    let thread = crate::quick::recovery::Session { account: "test-next-account".into(), thread: "test-new-quick-thread".into() };
    w.install_quick_handoff(thread.clone(), "Handoff note generated by Test model".into(), "Use checkpoint A.".into(), 0.0);
    {
        let c = s.conv.lock().unwrap();
        assert_eq!((&c.id, &c.parent), (&id, &parent));
        assert_eq!(c.marks.last().unwrap().kind, MarkKind::QuickHandoff);
        assert_eq!(c.marks.last().unwrap().sent, "Use checkpoint A.");
        assert!(is_hidden_block(&c.messages[2]["content"][0]));
        let loaded: Conversation = serde_json::from_value(serde_json::to_value(&*c).unwrap()).unwrap();
        assert_eq!(loaded.quick_session, Some(thread.clone()));
    }
    assert_eq!(crate::rotation::seat(api::Provider::Quick, &id), Some(thread.account.clone()));
    let (req, ..) = w.request_body();
    assert_eq!(req.conv.as_deref(), Some(id.as_str()));
    assert_eq!(req.provider, api::Provider::Quick);
    assert_eq!(req.quick_session, Some(thread));
    assert!(crate::quick::new_user_text(&req).unwrap().contains("Use checkpoint A."));
}

/// A turn its provider ran to the end itself (Quick's, `stop::COMPLETE`)
/// still carries an unfinished goal on: the continue prompt is what Quick
/// is sent next, and once GOAL finish is called the turn's end ends it.
#[test]
fn a_done_turn_carries_a_goal_on() {
    let goal = "Ship the Quick goals.";
    let (w, s) = idle_worker("quick-goal", Conversation { model: "smart".into(), ..Default::default() });
    {
        let mut c = s.conv.lock().unwrap();
        let (block, _) = apply_command(&mut c, Command::Goal(goal.into(), false));
        c.messages.push(json!({"role": "user", "content": [block.unwrap()]}));
    }
    let (r, ..) = w.request_body();
    assert_eq!(crate::quick::new_user_text(&r).as_deref(), Some(goal_prompt(goal, false).as_str()));
    s.conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": [oak::text("Done part of it.")]}));
    assert!(w.goal_step(), "an unfinished goal carries on");
    let (r, ..) = w.request_body();
    assert_eq!(crate::quick::new_user_text(&r).as_deref(), Some(goal_continue(goal, false).as_str()));
    {
        let mut c = s.conv.lock().unwrap();
        memory_tool(&mut c, "GOAL_JUSTIFICATION", &json!({"justification": "Shipped."}), 0).unwrap();
        memory_tool(&mut c, "GOAL_FINISH", &json!({}), 0).unwrap();
        c.messages.push(json!({"role": "assistant", "content": [oak::text("All done.")]}));
    }
    assert!(!w.goal_step(), "a finished goal hands control back");
    assert!(s.conv.lock().unwrap().goal.is_empty());
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
    assert_eq!(
        c.marks.last().map(|m| m.kind),
        Some(MarkKind::GoalContinued)
    );
    assert_eq!(after_turn(&mut c, true, 6), None);
    // Paused, the goal stays but nothing is sent or marked.
    c.goal_paused = true;
    let (marks, memory) = (c.marks.len(), render_memory(&c));
    assert_eq!(after_turn(&mut c, false, 6), None);
    assert_eq!((c.marks.len(), c.goal.as_str()), (marks, goal));
    assert_eq!(render_memory(&c), memory, "the model isn't told");
    c.goal_paused = false;
    // Finishing needs a justification first.
    assert_eq!(
        tool(&mut c, "GOAL_FINISH", json!({})),
        Err("Call GOAL action=justify first, with how you have completed the user's goal.".into())
    );
    assert!(
        tool(
            &mut c,
            "GOAL_JUSTIFICATION",
            json!({"justification": "Did X, Y and Z."})
        )
        .is_ok()
    );
    assert_eq!(
        tool(&mut c, "GOAL_FINISH", json!({})),
        Ok("The goal is finished. Control goes back to the user when you end your turn.".into())
    );
    assert_eq!(after_turn(&mut c, false, 7), None);
    let m = c.marks.last().unwrap();
    assert_eq!(
        (m.kind, m.text.as_str()),
        (MarkKind::GoalFinished, "Did X, Y and Z.")
    );
    assert!(c.goal.is_empty() && c.goal_justification.is_empty() && !c.goal_finished);
    // The marker key never reaches the API.
    let sent = crate::rolling::view(&[json!({"role": "user", "content": [more]})], None);
    assert!(
        !sent[0]["content"][0]
            .as_object()
            .unwrap()
            .contains_key("_oak_goal")
    );
}

/// Kits and notes (Claude): LOAD adds AGENT's tool in a system message,
/// the prompt and tools as they were. A cold cache changes nothing; only a
/// summary that covers the LOAD brings the kit into the prefix (and the
/// note into the session memory, its call's text kept as written).
#[test]
fn kits_and_notes_wait_for_a_summary() {
    let (w, s) = idle_worker(
        "kits",
        Conversation {
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Default::default()
        },
    );
    let names = |r: &oak::Request| {
        r.tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
    };
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r1, ..) = w.request_body();
    assert!(
        names(&r1).contains(&"LOAD".to_string())
            && !names(&r1).contains(&"AGENT".to_string())
            && !names(&r1).contains(&"BROWSER".to_string())
    );
    assert!(!r1.system.contains("Subagents (AGENT) suit"));
    // Not loaded: AGENT's calls say how to get it.
    assert!(
        w.unloaded("AGENT_LIST")
            .unwrap()
            .contains("LOAD with name=AGENT")
    );
    // Answered, so the cache is warm.
    {
        let mut c = s.conv.lock().unwrap();
        let choice = crate::cache_ttl::Choice {
            ttl: oak::Ttl::FiveMinutes,
            anchor: None,
        };
        c.cache.record(api::now_ms(), choice, 20_000, 1, 0);
        c.messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_call", "id": "l1", "name": "LOAD", "input": {"name": "agent"}},
            {"type": "tool_call", "id": "n1", "name": "NOTE", "input": {"action": "new", "text": "the db is postgres"}}]}));
    }
    let calls: Vec<Value> = s.conv.lock().unwrap().messages[1]["content"]
        .as_array()
        .unwrap()
        .clone();
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    let loaded = s.conv.lock().unwrap().messages[2]["content"][0]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        loaded.starts_with("Loaded AGENT. The AGENT tool is in your tool list now.")
            && loaded.contains("Subagents (AGENT) suit"),
        "{loaded}"
    );
    assert!(w.unloaded("AGENT_LIST").is_none());
    let (r2, ..) = w.request_body();
    assert_eq!(
        (r2.system.clone(), r2.tools.clone()),
        (r1.system.clone(), r1.tools.clone()),
        "the prefix as cached"
    );
    let last = r2.messages.last().unwrap();
    assert_eq!(last["role"], "system");
    assert_eq!(last["content"][0]["type"], "tool_definition");
    assert_eq!(last["content"][0]["definition"]["name"], "AGENT");
    assert!(!r2.system.contains("the db is postgres"), "the note waits");
    // Effort changed: per message, the request's own stays.
    s.conv.lock().unwrap().effort = "low".into();
    let (r3, ..) = w.request_body();
    assert_eq!(
        (
            r3.effort.as_deref(),
            r3.messages.last().unwrap()["effort"].as_str()
        ),
        (Some("high"), Some("low"))
    );
    // Cold (expired, another account), and old tool output: nothing sent
    // changes, nothing is cleared or folded, the effort stays per message.
    {
        let mut c = s.conv.lock().unwrap();
        let big = "x".repeat(200_000);
        c.messages.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "cat big"}}]}));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "e1", "content": big}]}));
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text("done")]}));
        c.messages
            .push(json!({"role": "user", "content": [oak::text("next")]}));
        c.cache.cold();
    }
    let (r4, ..) = w.request_body();
    assert_eq!(
        (&r4.system, &r4.tools, r4.effort.as_deref()),
        (&r1.system, &r1.tools, Some("high"))
    );
    assert_eq!(&r4.messages[..r3.messages.len()], &r3.messages[..]);
    let flat = serde_json::to_string(&r4.messages).unwrap();
    assert!(
        flat.contains("the db is postgres")
            && !flat.contains("Your session memory has the notes")
            && flat.contains(&"x".repeat(200_000))
    );
    // A summary covers the LOAD: the kit joins the prefix, and no message
    // adds its tool any more.
    {
        let mut c = s.conv.lock().unwrap();
        c.rolling.summary = Some(crate::rolling::Summary {
            cut: 3,
            text: "Loaded AGENT, noted the db.".into(),
            made_at: 1,
            clear: 3,
        });
    }
    let (r6, ..) = w.request_body();
    assert!(
        names(&r6).contains(&"AGENT".to_string()) && r6.system.contains("Subagents (AGENT) suit")
    );
    assert!(r6.system.contains("[1] (~5 tokens) the db is postgres") && r6.effort.as_deref() == Some("low"));
    assert!(r6.messages.iter().all(|m| m["role"] != "system"));
    let flat = serde_json::to_string(&r6.messages).unwrap();
    assert!(
        !flat.contains("\"LOAD\"") && !flat.contains("Subagents (AGENT) suit"),
        "{flat}"
    );
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
        prefix: crate::prefix::State {
            kits: vec!["AGENT".into(), "BROWSER".into()],
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Each kit is in the prefix exactly when a summary covers its LOAD;
/// otherwise its tool is told right after its LOAD's result, at every
/// renewal, and through a save and reload.
#[test]
fn a_kit_joins_the_prefix_only_once_a_summary_covers_its_load() {
    let (w, s) = idle_worker("two-loads", two_loads());
    let names = |r: &oak::Request| {
        r.tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
    };
    let added = |r: &oak::Request| -> Vec<(usize, String)> {
        let at = |i: usize, m: &Value| {
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_definition")
                .map(|b| (i, b["definition"]["name"].as_str().unwrap().to_string()))
                .collect::<Vec<_>>()
        };
        r.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "system")
            .flat_map(|(i, m)| at(i, m))
            .collect()
    };
    // The first request (nothing cached): neither kit in the prefix.
    let (r1, ..) = w.request_body();
    assert!(
        !names(&r1).contains(&"AGENT".to_string()) && !names(&r1).contains(&"BROWSER".to_string())
    );
    assert!(!r1.system.contains("Subagents (AGENT) suit") && !r1.system.contains("BROWSER kit"));
    // Each tool after its LOAD's result, before the reply that follows.
    assert_eq!(
        added(&r1),
        [(3, "AGENT".to_string()), (6, "BROWSER".to_string())]
    );
    assert_eq!(
        (
            r1.messages[2]["role"].as_str(),
            r1.messages[5]["content"][0]["call_id"].as_str()
        ),
        (Some("user"), Some("b"))
    );
    // As sent to Claude: each `tool_addition` right after its LOAD's
    // result, the tool list and system prompt without either kit.
    let body = api::claude_body(&r1);
    let sent: Vec<(String, String)> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["content"][0]["type"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let at = |role: &str, kind: &str| (role.to_string(), kind.to_string());
    assert_eq!(
        &sent[1..7],
        [
            at("assistant", "tool_use"),
            at("user", "tool_result"),
            at("system", "tool_addition"),
            at("assistant", "tool_use"),
            at("user", "tool_result"),
            at("system", "tool_addition")
        ]
    );
    assert_eq!(
        body["messages"][3]["content"][0]["tool"]["definition"]["name"],
        "AGENT"
    );
    assert!(
        !body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "AGENT" || t["name"] == "BROWSER")
    );
    // Cold again (expired, another account): the same request.
    s.conv.lock().unwrap().cache.cold();
    let (r2, ..) = w.request_body();
    assert_eq!(
        (&r2.system, &r2.tools, &r2.messages),
        (&r1.system, &r1.tools, &r1.messages)
    );
    // Saved and loaded again: the same request.
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let back: Conversation = serde_json::from_str(&saved).unwrap();
    let (w2, _s2) = idle_worker("two-loads-reloaded", back);
    let (r3, ..) = w2.request_body();
    assert_eq!(
        (&r3.system, &r3.tools, &r3.messages),
        (&r1.system, &r1.tools, &r1.messages)
    );
    // A summary covers AGENT's LOAD, not BROWSER's: AGENT joins the
    // prefix, BROWSER's tool stays after its LOAD.
    s.conv.lock().unwrap().rolling.summary = Some(crate::rolling::Summary {
        cut: 3,
        text: "Loaded AGENT.".into(),
        made_at: 1,
        clear: 3,
    });
    let (r4, ..) = w.request_body();
    assert!(
        names(&r4).contains(&"AGENT".to_string()) && !names(&r4).contains(&"BROWSER".to_string())
    );
    assert!(r4.system.contains("Subagents (AGENT) suit"));
    // The view: the summary, then messages 3.. (BROWSER's LOAD first).
    assert_eq!(r4.messages[1]["content"][0]["input"]["name"], "BROWSER");
    assert_eq!(added(&r4), [(3, "BROWSER".to_string())]);
    // Saved and loaded again, warm: the same request.
    {
        let mut c = s.conv.lock().unwrap();
        let choice = crate::cache_ttl::Choice {
            ttl: oak::Ttl::FiveMinutes,
            anchor: None,
        };
        let base = c.rolling.base();
        c.cache
            .record(api::now_ms(), choice, 20_000, r4.messages.len(), base);
    }
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let back: Conversation = serde_json::from_str(&saved).unwrap();
    let (w3, _s3) = idle_worker("two-loads-reloaded-again", back);
    let (r5, ..) = w3.request_body();
    assert_eq!(
        (&r5.system, &r5.tools, &r5.messages),
        (&r4.system, &r4.tools, &r4.messages)
    );
    // A summary that covers both: both in the prefix, no tool messages.
    s.conv.lock().unwrap().rolling.summary = Some(crate::rolling::Summary {
        cut: 5,
        text: "Loaded both.".into(),
        made_at: 2,
        clear: 5,
    });
    let (r6, ..) = w.request_body();
    assert!(
        names(&r6).contains(&"AGENT".to_string()) && names(&r6).contains(&"BROWSER".to_string())
    );
    assert!(added(&r6).is_empty() && r6.messages.iter().all(|m| m["role"] != "system"));
}

/// Where tools can't be added (OpenRouter), a kit loaded in view is
/// reached through LOAD; one loaded with a Claude model, whose LOAD said
/// it was in the tool list, is told so in text after its result.
#[test]
fn a_kit_loaded_in_view_is_called_through_load_where_tools_cant_be_added() {
    let (w, s) = idle_worker(
        "two-loads-generic",
        Conversation {
            provider: Some(crate::providers::Kind::OpenRouter),
            model: "openai/gpt-5".into(),
            ..two_loads()
        },
    );
    assert_eq!(
        crate::prefix::mode(s.conv.lock().unwrap().provider(), "openai/gpt-5"),
        Some(crate::prefix::Mode::Generic)
    );
    let (r, ..) = w.request_body();
    let names: Vec<&str> = r.tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!names.contains(&"AGENT") && !names.contains(&"BROWSER") && names.contains(&"LOAD"));
    let flat = serde_json::to_string(&r.messages).unwrap();
    assert!(
        flat.contains(
            "The AGENT tool isn't in your tool list here: call it through LOAD, with name=AGENT"
        ),
        "{flat}"
    );
    assert!(flat.contains(
        "The BROWSER tool isn't in your tool list here: call it through LOAD, with name=BROWSER"
    ));
    // A LOAD whose result said so already isn't told again.
    let mut c = s.conv.lock().unwrap().clone();
    c.messages[2]["content"][0]["content"] = json!(
        "Loaded AGENT. Its tool isn't in your tool list: call it through LOAD, with name=AGENT …"
    );
    c.prefix = crate::prefix::State {
        kits: c.prefix.kits.clone(),
        ..Default::default()
    };
    let (w, _s) = idle_worker("two-loads-generic-said", c);
    let (r, ..) = w.request_body();
    let flat = serde_json::to_string(&r.messages).unwrap();
    assert!(
        !flat.contains("The AGENT tool isn't in your tool list here")
            && flat.contains("The BROWSER tool isn't in your tool list here")
    );
}

/// A revert to before a LOAD unloads its kit, and what was told after
/// the revert's point goes with it.
#[test]
fn a_revert_before_a_load_unloads_its_kit() {
    let mut c = two_loads();
    c.prefix.sys.push(crate::prefix::SysMsg {
        at: 5,
        kind: "tool:BROWSER".into(),
        ..Default::default()
    });
    c.prefix.sys.push(crate::prefix::SysMsg {
        at: 3,
        kind: "tool:AGENT".into(),
        ..Default::default()
    });
    let was = c.messages[3]["content"][0].clone();
    assert!(revert_to(&mut c, 3, 0, &was));
    assert_eq!(c.prefix.kits, ["AGENT"]);
    assert_eq!(c.prefix.sys.iter().map(|m| m.at).collect::<Vec<_>>(), [3]);
}

/// ASK without an active goal waits for the answers. During a goal it
/// returns at once, its questions wait with the conversation, and the
/// answers come as a message with them. A new ASK replaces questions not
/// yet answered and says which, so the model can ask the full list again.
#[test]
fn ask_waits_without_a_goal_and_asks_asynchronously_during_one() {
    let (w, s) = idle_worker("ask-goal", Conversation::default());
    let q = |text: &str| json!({"questions": [{"question": text, "options": ["a", "b"]}]});
    // No goal: it waits (here the user skips).
    std::thread::scope(|sc| {
        let asking = sc.spawn(|| w.tool("ASK", &q("Which one?")).0);
        wait_until("the questions", || {
            s.live.lock().unwrap().waits_for_answers()
        });
        s.live.lock().unwrap().ask.as_mut().unwrap().skipped = true;
        let _ = s.wake.send(());
        assert_eq!(
            asking.join().unwrap(),
            Ok("The user skipped these questions without answering.".into())
        );
    });
    assert!(s.conv.lock().unwrap().pending_ask.is_null());

    // A goal: it returns at once, the questions kept for the user.
    apply_command(
        &mut s.conv.lock().unwrap(),
        Command::Goal("ship it".into(), false),
    );
    assert_eq!(
        w.tool("ASK", &q("Which database?")).0,
        Ok(ASKED_DURING_GOAL.into())
    );
    {
        let l = s.live.lock().unwrap();
        assert!(l.ask.as_ref().is_some_and(|a| a.background) && !l.waits_for_answers());
    }
    assert_eq!(s.conv.lock().unwrap().pending_ask, q("Which database?"));
    // Another replaces them, and says which it replaced.
    let r = w.tool("ASK", &q("Which port?")).0.unwrap();
    assert!(
        r.starts_with(ASKED_DURING_GOAL)
            && r.contains("replaced your earlier one")
            && r.contains("1. Which database?\n")
            && r.contains("full list"),
        "{r}"
    );
    assert_eq!(s.conv.lock().unwrap().pending_ask, q("Which port?"));
    // Answers to questions replaced meanwhile go nowhere.
    let mut stale = Ask {
        background: true,
        ..Ask::new(&q("Which database?")).unwrap()
    };
    stale.pick(0, Some(0));
    stale.sent = true;
    s.answer_questions(stale);
    assert!(s.conv.lock().unwrap().inbox.is_empty());
    // The user answers: the questions and answers reach the model together.
    let mut a = s.live.lock().unwrap().ask.take().unwrap();
    a.pick(0, Some(1));
    a.sent = true;
    s.answer_questions(a);
    {
        let mut c = s.conv.lock().unwrap();
        assert!(c.pending_ask.is_null());
        assert_eq!(
            c.inbox,
            vec![format!(
                "{ANSWERS_TAG}The user answered the questions you asked with ASK:\n1. Which port?\n   Answer: b\n</ask-answers>"
            )]
        );
        c.inbox.clear();
    }
    // Skipped, the model is told which questions.
    w.tool("ASK", &q("Which host?")).0.unwrap();
    let mut a = s.live.lock().unwrap().ask.take().unwrap();
    a.skipped = true;
    s.answer_questions(a);
    assert_eq!(
        s.conv.lock().unwrap().inbox,
        vec![format!(
            "{ANSWERS_TAG}The user skipped the questions you asked with ASK, without answering them:\n1. Which host?\n</ask-answers>"
        )]
    );

    // A paused goal: control goes back to the user, so ASK waits again,
    // and replaces what was still unanswered from the goal.
    w.tool("ASK", &q("Which region?")).0.unwrap();
    s.conv.lock().unwrap().goal_paused = true;
    std::thread::scope(|sc| {
        let asking = sc.spawn(|| w.tool("ASK", &q("Which one?")).0);
        wait_until("the questions", || {
            s.live.lock().unwrap().waits_for_answers()
        });
        assert!(s.conv.lock().unwrap().pending_ask.is_null());
        let mut l = s.live.lock().unwrap();
        let a = l.ask.as_mut().unwrap();
        a.pick(0, Some(0));
        a.sent = true;
        drop(l);
        let _ = s.wake.send(());
        let r = asking.join().unwrap().unwrap();
        assert!(
            r.starts_with("1. Which one?\n   Answer: a\n") && r.contains("1. Which region?"),
            "{r}"
        );
    });
}

/// Questions asked during a goal outlast a restart, and a REQUEST_ASK an
/// older Oak left waiting is retired: the model is told to ASK instead,
/// and a call of it (an older conversation still lists it) says so too.
#[test]
fn asked_questions_outlast_a_restart_and_request_ask_is_retired() {
    let mut old = serde_json::to_value(unit_conv(
        "ask-restart",
        Conversation {
            goal: "ship it".into(),
            ..Default::default()
        },
    ))
    .unwrap();
    old["ask_request"] = json!("Which database to use");
    old["ask_granted"] = json!(false);
    old["pending_ask"] = json!({"questions": [{"question": "Which port?"}]});
    let old: Conversation = serde_json::from_value(old).unwrap();
    let s = Deleting(Session::start(old, eframe::egui::Context::default()));
    {
        let l = s.live.lock().unwrap();
        assert_eq!(
            l.ask.as_ref().map(|a| (a.background, a.listed())),
            Some((true, "1. Which port?\n".into()))
        );
    }
    {
        let mut c = s.conv.lock().unwrap();
        assert_eq!(c.inbox, vec![request_ask_retired("Which database to use")]);
        assert!(c.inbox[0].starts_with(ANSWERS_TAG) && c.inbox[0].contains("call ASK"));
        let saved = serde_json::to_value(&*c).unwrap();
        assert!(saved.get("ask_request").is_none() && saved.get("ask_granted").is_none());
        assert_eq!(
            saved["pending_ask"],
            json!({"questions": [{"question": "Which port?"}]})
        );
        assert!(!retire_request_ask(&mut c), "once");
    }

    let (w, s) = idle_worker("request-ask-gone", Conversation::default());
    let call =
        json!({"type": "tool_call", "id": "r1", "name": "REQUEST_ASK", "input": {"about": "x"}});
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": [call.clone()]}));
    w.run_calls(&[call], oak::stop::TOOL_CALLS);
    let c = s.conv.lock().unwrap();
    let r = &c.messages[c.messages.len() - 1]["content"][0];
    assert_eq!(
        (r["content"].as_str(), r["is_error"].as_bool()),
        (Some(REQUEST_ASK_GONE), Some(true))
    );
}

/// Calls left without a result (OAK closed while they ran) get one, in
/// the next user message.
#[test]
fn lost_calls_answered() {
    let call = |id: &str| json!({"type": "tool_call", "id": id, "name": "EXECUTE", "input": {}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": "ok"});
    let msg = |role: &str, content: Vec<Value>| json!({"role": role, "content": content});
    let ids = |m: &Value| {
        m["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                b["call_id"]
                    .as_str()
                    .or(b["text"].as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    // Closed mid-call, then the user wrote again: their message follows the result.
    let mut c = Conversation {
        messages: vec![
            msg("user", vec![oak::text("go")]),
            msg("assistant", vec![call("a")]),
            msg("user", vec![result("a")]),
            msg("assistant", vec![call("b"), call("c")]),
            msg("user", vec![result("b"), oak::text("continue")]),
        ]
        .into(),
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
    c.messages.push(msg(
        "assistant",
        vec![json!({"type": "text", "text": "hm"}), call("d")],
    ));
    assert!(answer_lost_calls(&mut c));
    assert_eq!(
        (c.messages[6]["role"].as_str(), ids(&c.messages[6])),
        (Some("user"), vec!["d".to_string()])
    );
}

/// A TASK wait Oak closed on, in the turn under way and on a task Oak
/// still knows, stays unanswered to carry on; the turn's other calls get
/// the lost result, and so do a wait in an earlier turn and one on a task
/// Oak doesn't know.
#[test]
fn task_waits_left_open_to_carry_on() {
    let tasks = Tasks::open("");
    tasks.add(
        1,
        tasks::spawn("echo hi", &std::env::temp_dir()).unwrap(),
        "x",
    );
    let wait = |id: &str, task: u64| json!({"type": "tool_call", "id": id, "name": "TASK", "input": {"action": "wait", "id": task, "timeout": 60}});
    let exec = |id: &str| json!({"type": "tool_call", "id": id, "name": "EXECUTE", "input": {}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": "ok"});
    let msg = |role: &str, content: Vec<Value>| json!({"role": role, "content": content});
    let ids = |m: &Value| {
        m["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                b["call_id"]
                    .as_str()
                    .or(b["text"].as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    let mut c = Conversation {
        messages: vec![
            msg("assistant", vec![wait("old", 1)]),
            msg("user", vec![oak::text("next")]),
            msg(
                "assistant",
                vec![exec("e"), wait("w", 1), wait("x", 9), exec("f")],
            ),
            msg("user", vec![result("f")]),
        ]
        .into(),
        ..Default::default()
    };
    let (changed, kept) = answer_lost_calls_but(&mut c, |call| waits_on_known_task(call, &tasks));
    assert!(changed);
    assert_eq!(
        kept.iter()
            .map(|k| k["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["w"]
    );
    assert_eq!(ids(&c.messages[1]), ["old", "next"]);
    assert_eq!(ids(&c.messages[3]), ["e", "x", "f"]);
    // The reply alone at the end: still the turn under way.
    let mut c = Conversation {
        messages: vec![
            msg("user", vec![oak::text("go")]),
            msg("assistant", vec![wait("w", 1)]),
        ]
        .into(),
        ..Default::default()
    };
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
        task_waits: vec![
            TaskWait {
                call: "w".into(),
                since: 1_000,
                waited_ms: 500,
                closed_ms: 0,
            },
            TaskWait {
                call: "gone".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert!(carry_waits(
        &mut c,
        std::slice::from_ref(&call),
        Some(4_000),
        10_000
    ));
    assert_eq!(
        c.task_waits,
        [TaskWait {
            call: "w".into(),
            since: 10_000,
            waited_ms: 3_500,
            closed_ms: 6_000
        }]
    );
    // After a crash the records may be older than the stretch: none of it counts.
    carry_waits(&mut c, std::slice::from_ref(&call), Some(9_000), 12_000);
    assert_eq!(
        c.task_waits,
        [TaskWait {
            call: "w".into(),
            since: 12_000,
            waited_ms: 3_500,
            closed_ms: 8_000
        }]
    );
    // Without a record: since the call came in.
    c.task_waits.clear();
    carry_waits(&mut c, std::slice::from_ref(&call), Some(2_000), 3_000);
    assert_eq!(
        c.task_waits,
        [TaskWait {
            call: "w".into(),
            since: 3_000,
            waited_ms: 1_500,
            closed_ms: 1_000
        }]
    );
    // No waits: no records.
    assert!(carry_waits(&mut c, &[], None, 4_000) && c.task_waits.is_empty());
}

/// A wait carried on counts what it waited before toward its timeout and
/// says Oak was closed meanwhile; closing Oak with the tasks kept doesn't
/// end a wait, while an ultrapause does.
#[test]
fn task_wait_across_a_close() {
    let (w, _s) = idle_worker("wait-close", Conversation::default());
    w.tool("TASK_NEW", &json!({"command": "sleep 20", "summary": "x"}))
        .0
        .unwrap();
    let wait = json!({"id": 0, "timeout": 60});
    w.conv.lock().unwrap().task_waits.push(store::TaskWait {
        call: "w".into(),
        since: api::now_ms(),
        waited_ms: 59_000,
        closed_ms: 3_600_000,
    });
    let t = Instant::now();
    let out = w.tool_for("w", "TASK_WAIT", &wait).0.unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "only the second left: {:?}",
        t.elapsed()
    );
    assert!(
        out.starts_with("Task 0 is running")
            && out.contains("still running after waiting 60s")
            && out.contains("Oak was closed for 1h00m during this wait"),
        "{out}"
    );
    assert!(
        w.conv.lock().unwrap().task_waits.is_empty(),
        "its record goes"
    );
    // Oak closing with the tasks left running: the wait goes on.
    w.set(|l| (l.ultra, l.closing) = (true, true));
    let t = Instant::now();
    let out = w
        .tool_for("c", "TASK_WAIT", &json!({"id": 0, "timeout": 1}))
        .0
        .unwrap();
    assert!(
        t.elapsed() >= Duration::from_secs(1)
            && out.contains("still running after waiting 1s")
            && !out.contains("Oak was closed"),
        "{out}"
    );
    // A plain ultrapause ends it.
    w.set(|l| l.closing = false);
    let out = w
        .tool_for("u", "TASK_WAIT", &json!({"id": 0, "timeout": 60}))
        .0
        .unwrap();
    assert!(out.contains("ultrapause"), "{out}");
}

/// At start, a wait Oak closed on runs again first thing: its result joins
/// the turn's other results, the task's notification is dropped (the
/// result says it ended), and the turn is to carry on.
#[test]
fn open_waits_carry_on_first() {
    let (w, _s) = idle_worker("carry-on", Conversation::default());
    w.tool("TASK_NEW", &json!({"command": "echo done", "summary": "x"}))
        .0
        .unwrap();
    wait_until("the notification", || {
        !w.live.lock().unwrap().notes.is_empty()
    });
    let call = json!({"type": "tool_call", "id": "w", "name": "TASK", "input": {"action": "wait", "id": 0, "timeout": 60}});
    w.push(
        "assistant",
        vec![
            json!({"type": "tool_call", "id": "e", "name": "EXECUTE", "input": {}}),
            call.clone(),
        ],
    );
    w.push(
        "user",
        vec![json!({"type": "tool_result", "call_id": "e", "content": "ok"})],
    );
    w.conv.lock().unwrap().task_waits.push(store::TaskWait {
        call: "w".into(),
        since: api::now_ms(),
        waited_ms: 1_000,
        closed_ms: 5_000,
    });
    *w.open_waits.lock().unwrap() = vec![call];
    w.carry_on_waits();
    let c = w.conv.lock().unwrap();
    let last = c.messages.last().unwrap();
    assert_eq!(c.messages.len(), 2);
    let results: Vec<&str> = last["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["call_id"].as_str().unwrap())
        .collect();
    assert_eq!(results, ["w", "e"]);
    let text = last["content"][0]["content"].as_str().unwrap();
    assert!(
        text.starts_with("Task 0 exited with code 0")
            && text.contains("Oak was closed for 5s during this wait"),
        "{text}"
    );
    assert!(c.task_waits.is_empty());
    drop(c);
    assert!(
        w.live.lock().unwrap().notes.is_empty(),
        "the result told of the exit"
    );
    assert!(w.resume.load(Ordering::SeqCst));
}

/// A conversation read from its file is looked at without being parsed,
/// and mended exactly as one in memory.
#[test]
fn lost_calls_answered_as_read_from_the_file() {
    let call = |id: Value| json!({"type": "tool_call", "id": id, "name": "EXECUTE", "input": {}});
    let result =
        |id: Value| json!({"type": "tool_result", "call_id": id, "content": "ok\n\"quoted\""});
    let msg = |role: &str, content: Value| json!({"role": role, "content": content});
    let cases = [
        vec![
            msg("user", json!("go")),
            msg("assistant", json!([call(json!("a"))])),
            msg("user", json!([result(json!("a"))])),
        ],
        vec![
            msg("user", json!("go")),
            msg("assistant", json!([call(json!("a"))])),
        ],
        vec![
            msg("assistant", json!([call(json!(1)), call(json!("b"))])),
            msg("user", json!([result(json!(1)), oak::text("next")])),
        ],
        vec![
            msg("assistant", json!([call(json!("a"))])),
            msg("user", json!("a reply as text")),
        ],
        // Two replies in a row: not mended.
        vec![
            msg("assistant", json!([call(json!("a"))])),
            msg("assistant", json!([oak::text("again")])),
        ],
        vec![
            msg("assistant", json!([{"type": "text", "text": "tool_call"}])),
            json!("not a message"),
        ],
        vec![
            json!({"content": [call(json!("x"))], "role": "assistant", "extra": {"role": "user"}}),
            msg("user", json!([result(json!("x"))])),
        ],
    ];
    let file = crate::store::conversations_dir()
        .join(format!("unit-test-lost-{}.json", crate::api::now_ms()));
    for messages in cases {
        let mut mem = Conversation {
            messages: messages.clone().into(),
            ..Default::default()
        };
        std::fs::write(&file, serde_json::to_vec(&mem).unwrap()).unwrap();
        let mut read = crate::store::read_conversation(&file, &look_for_lost_calls).unwrap();
        let mended = answer_lost_calls(&mut mem);
        assert_eq!(answer_lost_calls(&mut read), mended, "{messages:?}");
        if !mended && messages.iter().all(Value::is_object) {
            assert!(!read.messages.loaded(), "read for nothing: {messages:?}");
        }
        assert_eq!(read.messages, mem.messages);
    }
    let _ = std::fs::remove_file(&file);
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
        call(
            "EXECUTE",
            json!({"command": "ls", "summary": "List", "timeout": 5}),
        ),
        call(
            "READ",
            json!({"path": if cfg!(windows) { "C:/work/A.rs" } else { "/work/a.rs" }}),
        ), // the same file, read again: fine
        call("EDIT", json!({"path": "a.rs"})), // writes what was read
        call("EDIT", json!({"path": "b.rs"})),
        call("EDIT", json!({"path": "a.rs"})), // writes what was written
        call("ASK", json!({})),
        call("SEARCH", json!({})),
    ];
    let none: Vec<usize> = vec![];
    assert_eq!(
        waits_for(&calls, folder),
        vec![
            none.clone(),
            none.clone(),
            none.clone(),
            none,
            vec![0, 3],
            vec![1],
            vec![0, 3, 4],
            (0..7).collect(),
            vec![7]
        ]
    );
    // CD goes alone too: what follows it means another folder.
    let calls = [
        call("READ", json!({"path": "a.rs"})),
        call("CD", json!({"path": "sub"})),
        call("READ", json!({"path": "a.rs"})),
        call("SEARCH", json!({})),
    ];
    assert_eq!(
        waits_for(&calls, folder),
        vec![vec![], vec![0], vec![1], vec![1]]
    );
    // The browser's calls keep their order, also those made through LOAD
    // (before BROWSER is in the tool list); loading a kit doesn't wait.
    let calls = [
        call("BROWSER", json!({"action": "click", "ref": 1})),
        call(
            "LOAD",
            json!({"name": "BROWSER", "input": {"action": "screenshot"}}),
        ),
        call("LOAD", json!({"name": "AGENT"})),
        call(
            "LOAD",
            json!({"name": "BROWSER", "input": {"action": "read"}}),
        ),
    ];
    assert_eq!(
        waits_for(&calls, folder),
        vec![vec![], vec![0], vec![], vec![0, 1]]
    );
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
    let (w, s) = idle_worker(
        "cd",
        Conversation {
            folder: root.clone(),
            ..Default::default()
        },
    );
    let pad = scratchpad(&mut s.conv.lock().unwrap());
    let cd = |path: &str| w.tool("CD", &json!({"path": path})).0;

    assert!(cd("nowhere").unwrap_err().starts_with("No folder"));
    assert!(cd("proj/a.txt").unwrap_err().contains("is a file"));
    assert!(cd(".").unwrap().contains("already"));
    let out = cd("proj").unwrap();
    assert!(
        out.contains(&format!("now {}", sub.display()))
            && out.contains("Use tabs.")
            && out.contains(&pad.display().to_string()),
        "{out}"
    );
    assert_eq!(s.conv.lock().unwrap().folder, sub);
    assert!(
        w.tool("READ", &json!({"path": "a.txt"}))
            .0
            .unwrap()
            .contains("in proj")
    );
    // The scratchpad stays, and is still found by the scan.
    assert_eq!(scratchpad(&mut s.conv.lock().unwrap()), pad);
    let owners = [crate::scratch::Owner {
        id: "x".into(),
        folder: sub.clone(),
        scratch: s.conv.lock().unwrap().scratch.clone(),
        last_ms: 0,
        in_use: false,
    }];
    assert!(
        crate::scratch::pads(&owners)
            .iter()
            .any(|p| p.dir == pad && p.owners == ["x"])
    );
    // Back up, with `..`; the old folder has no instructions.
    let out = cd("../proj/..").unwrap();
    assert!(
        out.contains(&format!("now {}", root.display())) && !out.contains("instructions"),
        "{out}"
    );
    assert_eq!(scratchpad(&mut s.conv.lock().unwrap()), pad);
    // The pin keeps the folder the cache was written with.
    let mut p = crate::prefix::State::default();
    let pin = |folder: &Path| crate::prefix::Pin {
        day: "d".into(),
        folder: folder.display().to_string(),
        tools: vec![json!({"name": "READ"})],
        ..Default::default()
    };
    let setup = |n, now: crate::prefix::Pin| crate::prefix::Setup {
        mode: crate::prefix::Mode::Claude,
        n,
        cut: 0,
        add: true,
        effort_per_message: true,
        base: now.clone(),
        now,
        joins: vec![],
        notes: 0,
    };
    assert_eq!(
        p.prepare(setup(1, pin(&root))).folder,
        root.display().to_string()
    );
    assert_eq!(
        p.prepare(setup(3, pin(&sub))).folder,
        root.display().to_string()
    );
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
    let (w, s) = idle_worker(
        "early",
        Conversation {
            folder: dir.clone(),
            ..Default::default()
        },
    );
    let call = |id: &str, name: &str, input: Value| json!({"type": "tool_call", "id": id, "name": name, "input": input});
    let calls = [
        call(
            "c1",
            "EXECUTE",
            json!({"command": "sleep 1; echo slow", "summary": "Wait", "timeout": 20}),
        ),
        call("c2", "WRITE", json!({"path": "a.txt", "content": "hi"})),
        call("c3", "READ", json!({"path": "a.txt"})),
        call("c4", "ASK", json!({"questions": [{"question": "Ok?"}]})),
    ];
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    std::thread::scope(|scope| {
        let mut b = Batch::new(&w, scope);
        b.add(calls[0].clone());
        b.pump(false, oak::stop::TOOL_CALLS);
        b.add(calls[1].clone());
        b.add(calls[2].clone());
        b.pump(false, oak::stop::TOOL_CALLS);
        // The EXECUTE and the WRITE run; the READ waits for the WRITE only.
        wait_until("the WRITE's result", || {
            w.live
                .lock()
                .unwrap()
                .early
                .as_ref()
                .is_some_and(|e| e.iter().any(|r| r["call_id"] == "c2"))
        });
        assert!(!b.started[2] && b.started[0]);
        b.pump(false, oak::stop::TOOL_CALLS);
        assert!(b.started[2], "the READ starts once the WRITE is done");
        b.add(calls[3].clone());
        b.pump(false, oak::stop::TOOL_CALLS);
        assert!(!b.started[3], "ASK waits for the reply's end");
        assert_eq!(
            s.conv.lock().unwrap().messages.len(),
            1,
            "the results wait for the reply"
        );
        // A reply cut short here keeps the calls that started.
        let mut blocks: Vec<Value> = vec![oak::text("on it")];
        blocks.extend(calls.iter().cloned());
        blocks.push(oak::text("and"));
        let kept = b.kept(blocks);
        assert_eq!(
            kept.iter()
                .map(|k| k["id"].as_str().unwrap_or("text"))
                .collect::<Vec<_>>(),
            ["text", "c1", "c2", "c3"]
        );
        // The reply as it came: its results, all of them, in order (the
        // ASK, answered by nobody, cut short).
        w.push("assistant", kept);
        let stopped = b.finish(&calls[..3], CUT_SHORT);
        assert!(!stopped);
    });
    let c = s.conv.lock().unwrap();
    let results = c.messages[2]["content"].as_array().unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| r["call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["c1", "c2", "c3", "c4"]
    );
    assert!(results[0]["content"].as_str().unwrap().contains("slow"));
    assert!(results[2]["content"].as_str().unwrap().contains("hi"));
    assert!(
        results[3]["content"]
            .as_str()
            .unwrap()
            .starts_with("Not run: the reply was cut short")
    );
    assert!(w.live.lock().unwrap().early.is_none());
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file the model read and then changed with a command (sed, a Python
/// rewrite) isn't reported back to it as changed on disk; a change made
/// outside Oak afterwards is.
#[test]
fn own_commands_changes_are_not_reported() {
    let dir = std::env::temp_dir().join(format!("oak-own-changes-{}", api::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    let (w, s) = idle_worker(
        "own-changes",
        Conversation {
            folder: dir.clone(),
            ..Default::default()
        },
    );
    let call = |id: &str, name: &str, input: Value| json!({"type": "tool_call", "id": id, "name": name, "input": input});
    let turn = |calls: Vec<Value>| {
        std::thread::scope(|scope| {
            w.push("assistant", calls.clone());
            Batch::new(&w, scope).finish(&calls, oak::stop::TOOL_CALLS);
        });
        w.report_changes();
    };
    let reported = || {
        s.conv
            .lock()
            .unwrap()
            .marks
            .iter()
            .filter(|m| m.kind == MarkKind::FilesChanged)
            .count()
    };
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    turn(vec![call("c1", "READ", json!({"path": "a.txt"}))]);
    turn(vec![call(
        "c2",
        "EXECUTE",
        json!({"command": "sed -i 's/two/TWO/' a.txt", "summary": "Edit", "timeout": 20}),
    )]);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\nTWO\n");
    assert_eq!(reported(), 0, "the model's own sed isn't news to it");
    // Someone else changes it: reported before the next request.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(dir.join("a.txt"), "one\nthree\n").unwrap();
    turn(vec![call("c3", "READ", json!({"path": "missing.txt"}))]);
    assert_eq!(reported(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A call of Quick's snippet (`quick::calls`) runs in the reply's batch
/// as any model's call does, and what it returns goes to the Quick turn
/// waiting for it.
#[test]
fn a_quick_snippets_call_reaches_the_turn_waiting_for_it() {
    let dir = std::env::temp_dir().join(format!("oak-quick-oak-call-{}", api::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "hello from oak\n").unwrap();
    let (w, s) = idle_worker(
        "quick-oak-call",
        Conversation {
            folder: dir.clone(),
            ..Default::default()
        },
    );
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    let rx = crate::quick::turn::wait_for_result("q1_oak0");
    std::thread::scope(|scope| {
        let mut b = Batch::new(&w, scope);
        b.add(json!({"type": "tool_call", "id": "q1_oak0", "name": "READ", "input": {"path": "a.txt"}}));
        b.pump(false, oak::stop::TOOL_CALLS);
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the turn gets the result");
        assert_eq!(r["call_id"], "q1_oak0");
        assert_eq!(r["is_error"], false);
        assert!(r.to_string().contains("hello from oak"), "{r}");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first request puts the identity, instructions and tools lines at
/// the top, folding in the instruction files' line, each opening to what
/// was sent.
#[test]
fn start_marks_show_what_the_model_is_given() {
    let mut c = Conversation {
        model: "claude-opus-5-5".into(),
        ..Default::default()
    };
    c.marks.push(Mark::new(0, MarkKind::Goal, "g", 0));
    c.marks.push(Mark {
        sent: "ctx".into(),
        ..Mark::new(
            1,
            MarkKind::Instructions,
            "instructions loaded: CLAUDE.md",
            0,
        )
    });
    let tools = tools::definitions(false);
    let p = api::preamble(&c.model, "SYS", &tools);
    start_marks(&mut c, 1, &p, "ctx", 0);
    let kinds: Vec<MarkKind> = c.marks.iter().map(|m| m.kind).collect();
    assert_eq!(
        kinds,
        [
            MarkKind::Goal,
            MarkKind::Identity,
            MarkKind::Instructions,
            MarkKind::Tools
        ]
    );
    assert_eq!(
        c.marks[2].text,
        "instructions loaded: Normal system prompt, CLAUDE.md (1 tokens)"
    );
    assert_eq!(
        store::split_sent(&c.marks[2].sent),
        [
            ("system prompt", "SYS"),
            ("start of the first message", "ctx")
        ]
    );
    let n = tools.as_array().unwrap().len();
    assert!(
        c.marks[3]
            .text
            .starts_with(&format!("tools loaded: {n} tools ("))
            && c.marks[3].text.ends_with("k tokens)"),
        "{}",
        c.marks[3].text
    );
    // Freebuff: no identity, its own prompt and the CLI's tools.
    let mut c = Conversation {
        model: "deepseek/deepseek-v4-flash".into(),
        ..Default::default()
    };
    let p = api::preamble(&c.model, "SYS", &tools);
    start_marks(&mut c, 0, &p, "", 0);
    assert_eq!(
        c.marks.iter().map(|m| m.kind).collect::<Vec<_>>(),
        [MarkKind::Instructions, MarkKind::Tools]
    );
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
    assert_eq!(mark, Some((MarkKind::Goal, "ship v2".into())));
    memory_tool(
        &mut c,
        "NOTE_NEW",
        &json!({"text": "the db is postgres"}),
        0,
    )
    .unwrap();
    let ttl = c.cache.ttl_ms();
    assert_eq!(
        memory_for_request(&mut c, ttl / 2),
        "",
        "a pause shorter than the cache's lifetime"
    );
    // The cache expired, or went cold (another account): the same.
    let later = 2 * ttl + ttl / 2 + 10_000;
    assert_eq!(memory_for_request(&mut c, later), "");
    c.cache.cold();
    assert_eq!(memory_for_request(&mut c, later + 500), "");
    // A summary moves the cut: brought up to date.
    c.rolling.apply(
        Summary {
            cut: 2,
            text: "s".into(),
            made_at: 0,
            ..Default::default()
        },
        0,
        Default::default(),
    );
    let m = memory_for_request(&mut c, later + 1_000);
    assert!(
        m.contains("# Session memory")
            && m.contains("ship v2")
            && m.contains("[1] (~5 tokens) the db is postgres"),
        "{m}"
    );
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
    c.rolling.summary = Some(crate::rolling::Summary {
        cut: 4,
        text: "s".into(),
        made_at: 0,
        ..Default::default()
    });
    assert!(
        !revert_to(&mut c, 2, 1, &oak::text("other")),
        "only the block the user saw"
    );
    assert!(!revert_to(&mut c, 2, 5, &oak::text("two")), "no such block");
    assert!(revert_to(&mut c, 2, 1, &oak::text("two")));
    assert_eq!(c.messages.len(), 3);
    assert_eq!(
        c.messages[2]["content"].as_array().unwrap().len(),
        1,
        "the tool result stays"
    );
    assert_eq!(c.marks.len(), 1);
    assert!(
        c.rolling.summary.is_none(),
        "the summary covered what was reverted"
    );
}

/// Reverting mid-turn, to before a block of a reply: the blocks before
/// it stay (thinking left at the end goes), with the results of the
/// calls among them; the calls cut lose theirs, and what came after goes.
#[test]
fn revert_into_a_reply_keeps_what_came_before() {
    let call =
        |id: &str| json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": id}});
    let result =
        |id: &str| json!({"type": "tool_result", "call_id": id, "content": format!("out {id}")});
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
    assert_eq!(
        c.messages[1]["content"],
        json!([thinking.clone(), oak::text("looking")])
    );
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
    let call =
        |id: &str| json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": id}});
    let result = |id: &str| json!({"type": "tool_result", "call_id": id, "content": "x"});
    let conv = Conversation {
        messages: vec![
            json!({"role": "user", "content": [oak::text("go")]}),
            json!({"role": "assistant", "content": [call("a"), call("b")]}),
            json!({"role": "user", "content": [result("a"), result("b")]}),
            json!({"role": "assistant", "content": [oak::text("done")]}),
        ]
        .into(),
        ..Default::default()
    };
    let s = Deleting(Session::start(
        unit_conv("rewind", conv),
        eframe::egui::Context::default(),
    ));
    s.rewind(RevertTo::Block(1, 1, call("b")));
    wait_until("the revert", || {
        s.conv.lock().unwrap().messages.len() == 3 && !s.live.lock().unwrap().busy
    });
    assert_eq!(
        s.conv.lock().unwrap().messages[2]["content"],
        json!([result("a")])
    );
    assert!(s.live.lock().unwrap().revert.is_none());
    s.rewind(RevertTo::End(1));
    wait_until("the second revert", || {
        s.conv.lock().unwrap().messages.len() == 1
    });
}

/// A task that exits mid-turn is notified with the next request, after
/// that request's tool results; one the model already saw exit isn't
/// notified at all.
#[test]
fn task_notifications_arrive_mid_turn() {
    let (w, _s) = idle_worker("notes-mid-turn", Conversation::default());
    w.tool(
        "TASK_NEW",
        &json!({"command": "echo quick", "summary": "x"}),
    )
    .0
    .unwrap();
    w.tool("TASK_NEW", &json!({"command": "echo seen", "summary": "y"}))
        .0
        .unwrap();
    wait_until("the notifications", || {
        w.live.lock().unwrap().notes.len() >= 2
    });
    w.tool("TASK_GET_NEW_OUTPUT", &json!({"id": 1})).0.unwrap();
    w.push(
        "assistant",
        vec![json!({"type": "tool_call", "id": "t1", "name": "EXECUTE", "input": {}})],
    );
    w.push(
        "user",
        vec![json!({"type": "tool_result", "call_id": "t1", "content": "ok"})],
    );
    w.absorb_notices();
    assert!(w.live.lock().unwrap().notes.is_empty());
    let c = w.conv.lock().unwrap();
    let last = c.messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone();
    drop(c);
    assert_eq!(
        last[0]["type"], "tool_result",
        "the tool results stay first"
    );
    let texts: Vec<&str> = last[1..]
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].starts_with(NOTE_TAG) && texts[0].contains("Task 0 "),
        "{texts:?}"
    );
}

/// Against the real API, with the signed-in account (run with
/// `cargo test live_rolling -- --ignored --nocapture`): a seeded ~30k
/// conversation passes a low trigger, gets summarized in the background
/// while the agent runs a tool, and is then asked about something only
/// the summary still holds. Checks that the summary request reads the
/// system prompt and tools from the prompt cache and that the API accepts
/// the summary request and the rolled view.
#[test]
#[ignore]
fn live_rolling() {
    assert!(api::load_auth(), "sign in to Oak first");
    let model = api::latest_models()
        .unwrap()
        .into_iter()
        .find(|m| m.id.contains("sonnet"))
        .expect("a Sonnet model")
        .id;
    rolling::set_config(rolling::Config {
        trigger: 20_000,
        keep: 5_000,
    });

    let filler = |turn: usize| -> String {
        (0..300)
            .map(|i| {
                format!(
                    "Build log {turn}.{i}: compiled crate number {i} in {} ms without warnings.\n",
                    100 + i
                )
            })
            .collect()
    };
    let mut messages = vec![];
    for turn in 0..6 {
        let mut text = filler(turn);
        if turn == 0 {
            text = format!(
                "Remember this instruction for the whole session: the deploy codeword is PELICAN-42.\n\n{text}"
            );
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
    s.send(
        "Use EXECUTE to run `echo rolling-test`, then reply with just DONE.".into(),
        vec![],
    );
    wait("first turn");
    s.send(
        "What is the deploy codeword? Reply with just the codeword.".into(),
        vec![],
    );
    wait("second turn");

    let c = s.conv.lock().unwrap().clone();
    crate::store::remove_conversation(&id);
    let r = &c.rolling;
    let cost = r.last_cost.expect("a summary was made");
    println!(
        "summary cost: {cost:?}; saved ~{} tokens; context now {}",
        r.saved, r.context
    );
    let answer = c.messages.last().unwrap()["content"].to_string();
    println!("answer: {answer}");
    assert!(r.last_error.is_none());
    assert_eq!(r.compressions, 1);
    // It sends the chat request's system prompt and tools (read from the
    // cache) and only the part it replaces, never the part kept.
    assert!(
        cost.cache_read > 0,
        "the summary should read the system prompt and tools from the cache: {cost:?}"
    );
    assert!(
        r.context < 20_000,
        "the rolled context is smaller: {}",
        r.context
    );
    assert!(
        answer.contains("PELICAN-42"),
        "the summary kept the instruction"
    );
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
    let (w, s) = idle_worker(
        "limit-model",
        Conversation {
            model: "claude-fable-5-2".into(),
            effort: "high".into(),
            limit_wait: Some(far),
            ..Default::default()
        },
    );
    s.command(Command::Model(
        api::Provider::Claude,
        "claude-opus-5-5".into(),
    ));
    let t = Instant::now();
    assert!(w.wait_for_limit());
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "waited {:?}",
        t.elapsed()
    );
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
    let (w, s) = idle_worker(
        "chat",
        Conversation {
            chat: true,
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Default::default()
        },
    );
    s.conv.lock().unwrap().folder = PathBuf::new();
    let secret = dir.join("secret.txt").display().to_string();
    let made = dir.join("made.txt");
    let refused = [
        ("READ", json!({"path": secret})),
        (
            "WRITE",
            json!({"path": made.display().to_string(), "content": "x"}),
        ),
        (
            "EDIT",
            json!({"path": secret, "old_string": "hidden", "new_string": "shown"}),
        ),
        (
            "EXECUTE",
            json!({"command": format!("cat {secret} > {}", made.display())}),
        ),
        ("TASK_NEW", json!({"command": "true"})),
        ("DISPLAY", json!({"paths": [secret]})),
        ("SCREENSHOT", json!({"url": format!("file://{secret}")})),
        ("CD", json!({"path": dir.display().to_string()})),
        ("AGENT_NEW", json!({"name": "x", "task": "read the files"})),
        ("HEARTBEAT_START", json!({"every_min": 5, "message": "x"})),
        (
            "PROGRESS_SET",
            json!({"label": "x", "command": "ls", "every_min": 1}),
        ),
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
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": calls}));
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    {
        let c = s.conv.lock().unwrap();
        let results = c.messages[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 6);
        for r in &results[..5] {
            assert!(
                r["content"]
                    .to_string()
                    .contains("isn't available in a chat"),
                "{r}"
            );
        }
        assert!(
            !results[5]["content"]
                .to_string()
                .contains("isn't available"),
            "{}",
            results[5]
        );
        assert_eq!(c.notes.len(), 1);
        assert!(c.agents.is_empty() && c.scratch.is_empty() && c.prefix.kits.is_empty());
    }
    assert!(!made.exists() && std::fs::read_to_string(dir.join("secret.txt")).unwrap() == "hidden");
    // The request: the chat's prompt and tools only.
    let (r, ..) = w.request_body();
    let mut names: Vec<String> = r
        .tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["ASK", "FETCH", "NOTE", "SEARCH"]);
    assert!(
        r.system.contains("This is a chat, not a coding session")
            && !r.system.contains("Working folder")
            && !r.system.contains("EXECUTE")
    );
    assert!(r.system.contains("[1] (~2 tokens) likes tea"));
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
    assert!(
        chat_refuses_provider(true, api::Provider::Freebuff).is_some()
            && chat_refuses_provider(true, api::Provider::Plugin).is_some()
    );
    assert!(
        chat_refuses_provider(false, api::Provider::Freebuff).is_none()
            && chat_refuses_provider(true, api::Provider::Claude).is_none()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A coding conversation keeps every tool; `chat` isn't written for it, so
/// its file reads as before, and a chat's reads back as a chat.
#[test]
fn chat_is_kept_and_coding_conversations_are_unchanged() {
    let coding = Conversation {
        id: "c".into(),
        ..Default::default()
    };
    let text = serde_json::to_string(&coding).unwrap();
    assert!(!text.contains("\"chat\""));
    assert!(!serde_json::from_str::<Conversation>(&text).unwrap().chat);
    let chat = Conversation {
        id: "c".into(),
        chat: true,
        ..Default::default()
    };
    assert!(
        serde_json::from_str::<Conversation>(&serde_json::to_string(&chat).unwrap())
            .unwrap()
            .chat
    );
    let (w, s) = idle_worker(
        "coding",
        Conversation {
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Default::default()
        },
    );
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r, ..) = w.request_body();
    let names: Vec<&str> = r.tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"EXECUTE") && names.contains(&"READ") && names.contains(&"LOAD"),
        "{names:?}"
    );
    assert!(r.system.contains("Working folder") && r.folder.is_some());
    assert!(w.tool("NOTE_NEW", &json!({"text": "x"})).0.is_ok());
    assert!(
        prompt::ROLES
            .iter()
            .all(|r| r.id != "chat" && r.id != "tools")
    );
}

/// A super agent (the New screen's Super agent) asks as one, with the
/// AGENT kit to load; its subagents, of its role, ask as one agent with
/// one task.
#[test]
fn super_agents_coordinate_and_their_agents_work() {
    let role = crate::agent::prompt::SUPER_ROLE.to_string();
    let (w, s) = idle_worker(
        "super",
        Conversation {
            model: "claude-opus-5-5".into(),
            role: role.clone(),
            title: "Release train".into(),
            titled: true,
            ..Default::default()
        },
    );
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("ship it")]}));
    let (r, ..) = w.request_body();
    assert!(
        r.system.starts_with("You are a super agent")
            && r.system.contains("Load the AGENT kit")
            && r.system.contains("Every EXECUTE and TASK command")
    );
    assert!(r.tools.iter().any(|t| t["name"] == "LOAD"));
    let (w, s) = idle_worker(
        "super-agent1",
        Conversation {
            model: "claude-opus-5-5".into(),
            role,
            parent: Some("unit-test-super".into()),
            agent_id: 1,
            ..Default::default()
        },
    );
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("do x")]}));
    let (r, ..) = w.request_body();
    assert!(
        r.system.starts_with("You are Oak, a coding agent")
            && r.system.contains("started by a super agent")
            && !r.system.contains("Load the AGENT kit")
    );
}

/// EXECUTE returns as soon as its command exits, not on its next tick;
/// a message from the user still moves a running one to the background.
#[test]
fn execute_returns_when_its_command_exits() {
    let dir = std::env::temp_dir();
    let (w, _s) = idle_worker(
        "exec-exit",
        Conversation {
            folder: dir.clone(),
            ..Default::default()
        },
    );
    let run = |cmd: &str| {
        w.tool(
            "EXECUTE",
            &json!({"command": cmd, "summary": "Run it", "timeout": 20}),
        )
    };
    // Interleave EXECUTE and bare runs so both see the same machine load
    // during a parallel `cargo test`.
    let (mut execute, mut bare) = (Duration::MAX, Duration::MAX);
    for _ in 0..10 {
        let t = Instant::now();
        let (r, interrupted) = run("echo hi");
        execute = execute.min(t.elapsed());
        assert!(
            r.as_ref().is_ok_and(|r| r.starts_with("hi\n[exit code 0")) && !interrupted,
            "{r:?}"
        ); // "[exit code 0, ran 7.1s]" on a busy machine
        let t = Instant::now();
        assert_eq!(
            tasks::spawn("echo hi", &dir)
                .unwrap()
                .wait_exit(Duration::from_secs(20)),
            Some(0)
        );
        bare = bare.min(t.elapsed());
    }
    assert!(
        execute < bare + Duration::from_millis(15),
        "EXECUTE took {execute:?}, the command itself {bare:?}"
    );
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            w.interrupt.store(true, Ordering::SeqCst);
        });
        let t = Instant::now();
        let (r, interrupted) = run("echo started; until [ -e /oak-never ]; do sleep 1; done");
        let r = r.unwrap();
        assert!(
            interrupted && r.contains("started") && r.contains("moved to the background"),
            "{r}"
        );
        assert!(t.elapsed() < Duration::from_secs(3));
    });
    w.tasks.kill_all();
}

#[test]
fn phase_reads_oaks_own_block_types() {
    let mut l = Live {
        busy: true,
        ..Live::default()
    };
    assert_eq!(l.plugin_phase().0, "requesting");
    l.blocks = vec![json!({"type": "reasoning", "text": "hm", "by": "claude"})];
    assert_eq!(l.plugin_phase(), ("thinking", String::new()));
    l.blocks.push(json!({"type": "text", "text": "So"}));
    assert_eq!(l.plugin_phase().0, "writing");
    l.blocks
        .push(json!({"type": "tool_call", "id": "t", "name": "READ", "input": {}}));
    assert_eq!(l.plugin_phase(), ("tool", "READ".to_string()));
}

/// A loaded kit's tool called through LOAD runs as that tool would called
/// directly: its input repaired as that tool's as it arrives (here sent as
/// JSON text, with the agent's id as a string), its result under the
/// LOAD call's id, the call named as the tool it ran.
#[test]
fn calls_through_load_run_as_their_tool() {
    let (w, s) = idle_worker(
        "through-load",
        Conversation {
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Default::default()
        },
    );
    s.conv.lock().unwrap().prefix.kits.push("AGENT".into());
    let input = tools::repair(
        "LOAD",
        json!({"name": "AGENT", "input": "{\"action\": \"send\", \"id\": \"7\", \"message\": \"hi\", \"timeout\": null}"}),
    );
    assert_eq!(
        input,
        json!({"name": "AGENT", "input": {"action": "send", "id": 7, "message": "hi"}})
    );
    let calls = vec![
        json!({"type": "tool_call", "id": "s1", "name": "LOAD", "input": input}),
        json!({"type": "tool_call", "id": "l1", "name": "LOAD", "input": {"name": "AGENT", "input": {"action": "list"}}}),
        json!({"type": "tool_call", "id": "k1", "name": "LOAD", "input": {"name": "AGENT"}}),
    ];
    let shown: Vec<String> = calls.iter().map(crate::transcript::shown).collect();
    assert_eq!(shown, ["AGENT send", "AGENT list", "LOAD"]);
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": calls.clone()}));
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    let c = s.conv.lock().unwrap();
    let results = c.messages[2]["content"].as_array().unwrap();
    let ids: Vec<&str> = results
        .iter()
        .map(|r| r["call_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["s1", "l1", "k1"]);
    let text = |i: usize| results[i]["content"].to_string();
    assert!(
        text(0).contains("No agent 7.") && !text(0).contains("Invalid input"),
        "{}",
        text(0)
    );
    assert!(
        text(1).contains("No agents yet") && results[1]["is_error"] != true,
        "{}",
        text(1)
    );
    assert!(text(2).contains("AGENT is loaded already"), "{}", text(2));
}

/// `v` without its cache breakpoints, which move with every request and
/// are no part of what the model reads.
fn without_breakpoints(mut v: Value) -> Value {
    match &mut v {
        Value::Object(o) => {
            o.remove("cache_control");
            o.values_mut()
                .for_each(|x| *x = without_breakpoints(x.take()));
        }
        Value::Array(a) => a
            .iter_mut()
            .for_each(|x| *x = without_breakpoints(x.take())),
        _ => {}
    }
    v
}

/// What a request sends the model as its provider's wire body has it: the
/// system prompt, the tools and the messages.
type Sent = (Value, Value, Vec<Value>);

fn claude_sent(r: &oak::Request) -> Sent {
    let b = without_breakpoints(api::claude_body(r));
    (
        b["system"].clone(),
        b["tools"].clone(),
        b["messages"].as_array().unwrap().clone(),
    )
}

fn codex_sent(r: &oak::Request) -> Sent {
    let b = without_breakpoints(crate::codex::request_body(r));
    (
        b["instructions"].clone(),
        b["tools"].clone(),
        b["input"].as_array().unwrap().clone(),
    )
}

fn generic_sent(r: &oak::Request) -> Sent {
    let b = without_breakpoints(crate::wire_anthropic::body(r, "openrouter", false));
    (
        b["system"].clone(),
        b["tools"].clone(),
        b["messages"].as_array().unwrap().clone(),
    )
}

fn antigravity_sent(r: &oak::Request) -> Sent {
    let b = crate::antigravity::request_body(r);
    let req = &b["request"];
    let mut contents = req["contents"].as_array().unwrap().clone();
    // `prefix_only_grows` probes the fork source right after an assistant reply;
    // Antigravity appends a synthetic user bridge only to satisfy Google's
    // "cannot end on model turn" wire rule.
    if r.messages.last().is_some_and(|m| m["role"] == "assistant")
        && contents.last().is_some_and(|t| {
            t["role"] == "user"
                && t["parts"][0]["text"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("Continue the active task"))
        })
    {
        contents.pop();
    }
    let system = req
        .get("systemInstruction")
        .cloned()
        .unwrap_or_else(|| contents[0]["parts"][0].clone());
    (system, req["tools"].clone(), contents)
}

/// `b` sends all `a` sent, unchanged, at its start (where they differ,
/// briefly).
fn assert_starts_with(a: &Sent, b: &Sent, what: &str) {
    let short = |v: &Value| v.to_string().chars().take(300).collect::<String>();
    let diff = |x: &Value, y: &Value| {
        let (x, y) = (x.to_string(), y.to_string());
        let i = x.chars().zip(y.chars()).take_while(|(a, b)| a == b).count();
        let around = |s: &str| {
            s.chars()
                .skip(i.saturating_sub(80))
                .take(240)
                .collect::<String>()
        };
        format!("from char {i}:\n  {}\n  {}", around(&x), around(&y))
    };
    assert!(
        a.0 == b.0,
        "{what}: the system prompt differs {}",
        diff(&a.0, &b.0)
    );
    assert!(a.1 == b.1, "{what}: the tools differ {}", diff(&a.1, &b.1));
    assert!(
        a.2.len() <= b.2.len(),
        "{what}: {} messages, not {} (last: {})",
        b.2.len(),
        a.2.len(),
        b.2.last().map(short).unwrap_or_default()
    );
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
fn prefix_only_grows(
    name: &str,
    conv: Conversation,
    other_model: &str,
    sent: fn(&oak::Request) -> Sent,
) {
    let folder = std::env::temp_dir().join(format!("oak-prefix-{name}-{}", api::now_ms()));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("AGENTS.md"), "Rule one.").unwrap();
    let (w, s) = idle_worker(
        name,
        Conversation {
            effort: "high".into(),
            folder: folder.clone(),
            messages: vec![json!({"role": "user", "content": [oak::text("start")]})].into(),
            ..conv
        },
    );
    let req = |w: &Worker| sent(&w.request_body().0);
    let reply = |text: &str, next: &str| {
        let mut c = s.conv.lock().unwrap();
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text(text)]}));
        c.messages
            .push(json!({"role": "user", "content": [oak::text(next)]}));
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
    let calls = s.conv.lock().unwrap().messages[1]["content"]
        .as_array()
        .unwrap()
        .clone();
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
    // (Seamless: the model isn't told it was switched to.)
    assert!(!flat(&step(&mut prev, &w, "another model")).contains("model is now"));
    // The cache expires or the account changes; a goal is set; the
    // instruction files change; old tool output piles up.
    s.conv.lock().unwrap().cache.cold();
    reply("ok", "after a pause");
    step(&mut prev, &w, "a cold cache");
    {
        let mut c = s.conv.lock().unwrap();
        let (block, _) = apply_command(&mut c, Command::Goal("ship v2".into(), false));
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages
            .push(json!({"role": "user", "content": [block.unwrap()]}));
    }
    step(&mut prev, &w, "a goal");
    std::fs::write(folder.join("AGENTS.md"), "Rule two.").unwrap();
    {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "cat big"}}]}));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "e1", "content": "x".repeat(300_000)}]}));
        c.cache.cold();
    }
    let after = step(
        &mut prev,
        &w,
        "instruction files changed, old tool output, cold again",
    );
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
    let (w2, s2) = idle_worker(
        &format!("{name}-reloaded"),
        serde_json::from_str(&saved).unwrap(),
    );
    assert_eq!(req(&w2), prev, "reloaded");
    s2.conv.lock().unwrap().cache.cold();
    s2.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": [oak::text("back")]}));
    s2.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("again")]}));
    step(&mut prev, &w2, "reloaded, cold");
    // A fork sends what its source sent.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text("done")]}));
    }
    let source = req(&w);
    let forked = crate::side_chat::fork(
        &s.conv.lock().unwrap(),
        format!("unit-test-{name}-fork-{}", api::now_ms()),
        0,
    );
    let (w3, _s3) = idle_worker(&format!("{name}-fork"), forked);
    assert_grows(&source, &req(&w3), "a fork");
    // A summary: the one moment the prefix changes. The note joins the
    // session memory, the kit the prefix, the effort and model as they are.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages
            .push(json!({"role": "user", "content": [oak::text("more")]}));
        let cut = c.messages.len() - 2;
        install_summary(
            &mut c,
            crate::rolling::Summary {
                cut,
                text: "Loaded AGENT, noted the db, set a goal.".into(),
                made_at: 1,
                clear: cut,
            },
            0,
            Default::default(),
        );
    }
    let summarized = req(&w);
    assert_ne!(
        summarized.0, prev.0,
        "the system prompt is brought up to date"
    );
    let system = serde_json::to_string(&summarized.0).unwrap();
    assert!(
        system.contains("the db is postgres") && system.contains("ship v2"),
        "{system}"
    );
    assert!(
        flat(&summarized).contains("Rule two.")
            && !flat(&summarized).contains(&"x".repeat(300_000))
    );
    prev = summarized;
    // After it, the same again.
    s.conv.lock().unwrap().effort = "high".into();
    s.conv.lock().unwrap().cache.cold();
    reply("ok", "after the summary");
    step(&mut prev, &w, "after the summary: effort, cold");
    s.conv.lock().unwrap().rolling.off = true;
    reply("ok", "off again");
    step(
        &mut prev,
        &w,
        "after the summary: smart context off keeps it",
    );
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let (w4, s4) = idle_worker(
        &format!("{name}-reloaded-again"),
        serde_json::from_str(&saved).unwrap(),
    );
    assert_eq!(req(&w4), prev, "reloaded after the summary");
    // A second summary, and the session memory changes since.
    {
        let mut c = s4.conv.lock().unwrap();
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages
            .push(json!({"role": "user", "content": [oak::text("later")]}));
        memory_tool(
            &mut c,
            "NOTE_NEW",
            &json!({"text": "the cache is redis"}),
            0,
        )
        .unwrap();
        let cut = c.messages.len() - 1;
        install_summary(
            &mut c,
            crate::rolling::Summary {
                cut,
                text: "Later.".into(),
                made_at: 2,
                clear: cut,
            },
            0,
            Default::default(),
        );
    }
    assert!(
        serde_json::to_string(&req(&w4).0)
            .unwrap()
            .contains("the cache is redis")
    );
    // The user reverts (or edits a message) to before both summaries' cuts:
    // what was sent then is sent again, exactly, from the state saved.
    assert!(
        s4.conv
            .lock()
            .unwrap()
            .rolling
            .summary
            .as_ref()
            .is_some_and(|s| s.cut > back_to)
    );
    assert!(revert_to_end(&mut s4.conv.lock().unwrap(), back_to));
    let saved = serde_json::to_string(&*s4.conv.lock().unwrap()).unwrap();
    let (w5, s5) = idle_worker(
        &format!("{name}-reverted"),
        serde_json::from_str(&saved).unwrap(),
    );
    // (Then, as after any pause, what changed since is added: here the
    // effort.)
    let back = req(&w5);
    assert_starts_with(
        &sent_then,
        &back,
        "reverted to before the summaries, reloaded",
    );
    assert_eq!(
        back.2.len(),
        sent_then.2.len() + usize::from(name == "prefix-claude"),
        "only the effort is added"
    );
    assert_starts_with(&sent_then, &req(&w4), "reverted to before the summaries");
    let mut back = back;
    s5.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": [oak::text("again")]}));
    s5.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("onwards")]}));
    step(&mut back, &w5, "after the revert");
    drop((w2, s2));
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn the_prefix_only_grows_on_claude() {
    prefix_only_grows(
        "prefix-claude",
        Conversation {
            model: "claude-opus-5-5".into(),
            ..Default::default()
        },
        "claude-sonnet-5-5",
        claude_sent,
    );
}

#[test]
fn the_prefix_only_grows_on_codex() {
    let conv = Conversation {
        provider: Some(crate::providers::Kind::Codex),
        model: "gpt-5.6-sol".into(),
        ..Default::default()
    };
    prefix_only_grows("prefix-codex", conv, "gpt-5.6-terra", codex_sent);
}

#[test]
fn the_prefix_only_grows_where_system_messages_cant_go() {
    let conv = Conversation {
        provider: Some(crate::providers::Kind::OpenRouter),
        model: "openai/gpt-5".into(),
        ..Default::default()
    };
    prefix_only_grows(
        "prefix-generic",
        conv,
        "deepseek/deepseek-v4-flash",
        generic_sent,
    );
}

#[test]
fn the_prefix_only_grows_on_antigravity() {
    let conv = Conversation {
        provider: Some(crate::providers::Kind::Antigravity),
        model: "antigravity/gemini-3.8-flash".into(),
        ..Default::default()
    };
    prefix_only_grows(
        "prefix-antigravity",
        conv,
        "antigravity/gemini-3.1-pro",
        antigravity_sent,
    );
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
    assert!(
        err.starts_with(
            "Not saved: it would take your notes to about 101 tokens, past their limit of 100"
        ),
        "{err}"
    );
    assert_eq!((note_lens(&c), c.next_note_id), (vec![300, 53], 2));
    // An update that would make a note longer past the limit: refused too.
    let err = tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 2, "text": "b".repeat(54)}),
    )
    .unwrap_err();
    assert!(
        err.starts_with("Not updated: it would make note 2 longer"),
        "{err}"
    );
    assert_eq!(note_lens(&c), [300, 53]);
    // Room made, the same fit.
    tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 1, "text": "a".repeat(299)}),
    )
    .unwrap();
    tool(&mut c, "NOTE_NEW", json!({"text": "c"})).unwrap();
    assert_eq!(note_lens(&c), [299, 53, 1]);
    // Growing under the limit, and up to it, is fine.
    tool(&mut c, "NOTE_DELETE", json!({"id": 3})).unwrap();
    tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 2, "text": "b".repeat(54)}),
    )
    .unwrap();
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
    assert_eq!(
        (note_lens(&c), notes_tokens(&c.notes)),
        (vec![200, 200, 200], 171)
    );
    assert!(render_memory(&c).contains("over their limit of 100"));
    // No new note, however small; no update that makes a note longer.
    let err = tool(&mut c, "NOTE_NEW", json!({"text": "x"})).unwrap_err();
    assert!(
        err.starts_with("Not saved: your notes take about 171 tokens, over their limit of 100."),
        "{err}"
    );
    assert!(
        tool(
            &mut c,
            "NOTE_UPDATE",
            json!({"id": 1, "text": "n".repeat(201)})
        )
        .is_err()
    );
    assert_eq!((note_lens(&c), c.next_note_id), (vec![200, 200, 200], 3));
    // Rewording at the same length, shrinking and deleting all go through,
    // each saying what is left to do while still over.
    let same = tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 1, "text": "m".repeat(200)}),
    )
    .unwrap();
    assert!(same.contains("over their limit of 100"), "{same}");
    let shrunk = tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 2, "text": "n".repeat(100)}),
    )
    .unwrap();
    assert!(
        shrunk.contains("about 142 tokens, over their limit"),
        "{shrunk}"
    );
    let deleted = tool(&mut c, "NOTE_DELETE", json!({"id": 3})).unwrap();
    assert_eq!(deleted, "Deleted note 3 (your notes take about 85 of 100 tokens in all).");
    assert_eq!(
        (note_lens(&c), notes_tokens(&c.notes)),
        (vec![200, 100], 85)
    );
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
    assert!(
        !render_memory(&c).contains("over their limit"),
        "{}",
        render_memory(&c)
    );
    assert_eq!(
        tool(
            &mut c,
            "NOTE_UPDATE",
            json!({"id": 1, "text": "m".repeat(200)})
        )
        .unwrap(),
        "Updated note 1 (note 1 takes about 57 tokens; your notes about 171 of 100 tokens in all)."
    );
    assert_eq!(
        tool(&mut c, "NOTE_DELETE", json!({"id": 3})).unwrap(),
        "Deleted note 3 (your notes take about 114 of 100 tokens in all)."
    );
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
    let down = tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 1, "text": "a".repeat(300)}),
    )
    .unwrap();
    assert!(!down.contains("over their limit"), "{down}");
    assert!(
        tool(
            &mut c,
            "NOTE_UPDATE",
            json!({"id": 1, "text": "a".repeat(400)})
        )
        .is_err()
    );
    tool(
        &mut c,
        "NOTE_UPDATE",
        json!({"id": 1, "text": "a".repeat(353)}),
    )
    .unwrap();
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
    let back: crate::store::Settings =
        serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
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
    let conv = Conversation {
        model: "claude-opus-5-5".into(),
        messages: vec![json!({"role": "user", "content": [oak::text("start")]})].into(),
        ..Default::default()
    };
    let (w, s) = idle_worker("notes-limit", conv);
    let req = |w: &Worker| claude_sent(&w.request_body().0);
    let flat = |x: &Sent| serde_json::to_string(&(&x.0, &x.2)).unwrap();
    let mut prev = req(&w);
    let system = serde_json::to_string(&prev.0).unwrap();
    assert!(
        system.contains("Your notes may take at most about 2,000 tokens in all"),
        "{system}"
    );
    let reply = |text: &str| {
        let mut c = s.conv.lock().unwrap();
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages
            .push(json!({"role": "user", "content": [oak::text(text)]}));
    };
    memory_tool(
        &mut s.conv.lock().unwrap(),
        "NOTE_NEW",
        &json!({"text": "n".repeat(3_500)}),
        0,
    )
    .unwrap();
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
    assert!(
        serde_json::to_string(&next.0)
            .unwrap()
            .contains("about 2,000 tokens in all")
    );
    prev = next;
    assert!(
        memory_tool(
            &mut s.conv.lock().unwrap(),
            "NOTE_NEW",
            &json!({"text": "more"}),
            0
        )
        .is_err()
    );
    memory_tool(
        &mut s.conv.lock().unwrap(),
        "NOTE_UPDATE",
        &json!({"id": 1, "text": "n".repeat(1_000)}),
        0,
    )
    .unwrap();
    // Told once.
    reply("and on");
    let next = req(&w);
    assert_grows(&prev, &next, "the same limit");
    assert_eq!(
        flat(&next)
            .matches("The limit on your notes is now")
            .count(),
        1
    );
    prev = next;
    // Saved and loaded: the same request.
    let saved = serde_json::to_string(&*s.conv.lock().unwrap()).unwrap();
    let (w2, _s2) = idle_worker(
        "notes-limit-reloaded",
        serde_json::from_str(&saved).unwrap(),
    );
    assert_eq!(req(&w2), prev, "reloaded");
    // A summary: the system prompt names the limit now, and no message does.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages
            .push(json!({"role": "assistant", "content": [oak::text("ok")]}));
        c.messages
            .push(json!({"role": "user", "content": [oak::text("more")]}));
        let cut = c.messages.len() - 1;
        install_summary(
            &mut c,
            crate::rolling::Summary {
                cut,
                text: "Noted things.".into(),
                made_at: 1,
                clear: cut,
            },
            0,
            Default::default(),
        );
    }
    let summarized = req(&w);
    let system = serde_json::to_string(&summarized.0).unwrap();
    assert!(
        system.contains("Your notes may take at most about 500 tokens in all"),
        "{system}"
    );
    assert!(!flat(&summarized).contains("The limit on your notes is now"));
    crate::agent::memory::set_test_notes_limit(None);
}

/// AGENT_PAUSE and AGENT_RESUME by model pick the agents on it by id,
/// family word or provider, and nothing that merely shares a version.
#[test]
fn group_pause_picks_by_model() {
    use super::subagents::on_model;
    use crate::providers::Kind;
    for (word, model, kind) in [
        ("opus", "claude-opus-4-7", Kind::Claude),
        ("Opus", "anthropic/claude-opus-4", Kind::OpenRouter),
        ("claude-opus-4-7", "claude-opus-4-7", Kind::Claude),
        ("sol", "gpt-6.1-sol", Kind::Codex),
        ("gpt", "gpt-6.1-sol", Kind::Codex),
        ("codex", "gpt-6.1-sol", Kind::Codex),
        (
            "antigravity",
            "antigravity/gemini-3.8-flash",
            Kind::Antigravity,
        ),
        ("gemini", "antigravity/gemini-3.8-flash", Kind::Antigravity),
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

#[test]
fn agent_list_is_a_markdown_table() {
    use crate::store::AgentRef;
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let (parent, session) = idle_worker("agent-list", Conversation::default());
    assert_eq!(parent.agent_list(), "No agents yet: AGENT action=new starts one.");
    let now = api::now_ms();
    let (child, agent) = idle_worker("agent-list-child", Conversation {
        model: "smart".into(),
        effort: "high".into(),
        last_message_at: now - 60_000,
        progress_at: now - 30_000,
        progress: "EXECUTE returned".into(),
        ..Default::default()
    });
    child.set(|l| {
        l.busy = true;
        l.status = "Running execute".into();
    });
    session.conv.lock().unwrap().agents = vec![
        AgentRef { id: 1, conv: agent.conv.lock().unwrap().id.clone(), name: "A\\B | C\r\nD\tE".into() },
        AgentRef { id: 2, conv: "missing-agent-list-session".into(), name: "Offline".into() },
    ];

    let output = parent.agent_list();
    let mut cells = Vec::new();
    let mut cell = String::new();
    for event in Parser::new_ext(&output, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::TableCell) => cell.clear(),
            Event::Text(text) => cell.push_str(&text),
            Event::End(TagEnd::TableCell) => cells.push(std::mem::take(&mut cell)),
            _ => {}
        }
    }
    assert_eq!(cells.len(), 18, "{output}");
    assert_eq!(&cells[..6], ["id", "state", "last progress", "last change", "model", "name"]);
    assert_eq!(cells[6], "1");
    assert!(cells[7].starts_with("working (running execute) for "), "{output}");
    assert!(cells[8].ends_with(" ago: EXECUTE returned"), "{output}");
    assert!(cells[9].ends_with(" ago"), "{output}");
    assert_eq!(cells[10], "smart (high)");
    assert_eq!(cells[11], "agent 1 (A\\B | C D E)");
    assert_eq!(&cells[12..], ["2", "not running", "-", "-", "-", "agent 2 (Offline)"]);
}

/// A session's pulse: its last progress (a reply, a tool's result by its
/// tool's name), kept with the conversation; a wait its status line names
/// is what blocks it, and so is the error its last turn ended on.
#[test]
fn the_pulse_says_progress_and_blockers() {
    let (w, s) = idle_worker("pulse", Conversation::default());
    let p = super::pulse::Pulse::of(&s);
    assert_eq!(
        (p.phase, p.progress_at, p.blocker.as_str()),
        (super::pulse::IDLE, 0, "")
    );
    w.set(|l| {
        l.busy = true;
        l.status = "Waiting for the model".into();
    });
    let p = super::pulse::Pulse::of(&s);
    assert_eq!(p.phase, super::pulse::MODEL);
    assert!(
        p.since > 0 && p.detail(api::now_ms()).ends_with("no progress yet"),
        "{p:?}"
    );

    w.push("assistant", vec![json!({"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "echo hush", "summary": "Say"}})]);
    assert_eq!(
        s.conv.lock().unwrap().progress,
        "Its model replied with 1 tool call"
    );
    w.set(|l| l.status = "Running Say".into());
    w.push(
        "user",
        vec![oak::tool_result(&json!("e1"), json!("hush"), false)],
    );
    let p = super::pulse::Pulse::of(&s);
    assert_eq!(
        (p.phase, p.progress.as_str()),
        (super::pulse::TOOLS, "EXECUTE returned")
    );
    assert!(!format!("{p:?}").contains("echo hush"));

    w.set(|l| l.status = "Retry 1/5 in 3s after: HTTP 529".into());
    let p = super::pulse::Pulse::of(&s);
    assert_eq!(
        (p.phase, p.blocker.as_str()),
        (super::pulse::WAITING, "Retry 1/5 in 3s after: HTTP 529")
    );

    w.set(|l| {
        l.busy = false;
        l.status.clear();
        l.error = Some("Anthropic answered HTTP 500:\n overloaded".into());
    });
    let p = super::pulse::Pulse::of(&s);
    assert_eq!(
        (p.phase, p.blocker.as_str()),
        (
            super::pulse::BLOCKED,
            "Its last turn ended on an error: Anthropic answered HTTP 500: overloaded"
        )
    );
    // The last progress is saved with the conversation.
    let saved = serde_json::to_value(&*s.conv.lock().unwrap()).unwrap();
    assert_eq!(saved["progress"], "EXECUTE returned");
}

/// Every `SENT-<WORD>-<n>` sentinel in `v`, in order, once each.
fn sentinels_in(v: &Value) -> Vec<String> {
    let s = v.to_string();
    let mut out: Vec<String> = vec![];
    for (i, _) in s.match_indices("SENT-") {
        let t: String = s[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let t = t.trim_end_matches('-').to_string();
        if t.matches('-').count() == 2 && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// A summary request carries only what it replaces, as the model saw it:
/// the summary in use and the messages before the cut (with the prefix's
/// messages there), then the instruction. Never the part kept verbatim, nor
/// what the model never saw of the transcript (cleared tool output, another
/// provider's reasoning, Oak's own keys, the part an earlier summary
/// covered). On each provider's way of sending the prefix, and Freebuff's
/// (none).
#[test]
fn a_summary_request_carries_only_what_it_replaces() {
    let user = |t: &str| json!({"role": "user", "content": [oak::text(t)]});
    let said = |t: &str| json!({"role": "assistant", "content": [oak::text(t)]});
    let big = format!("SENT-HIDDEN-04 {}", "y".repeat(2_000));
    let messages = vec![
        user("SENT-OLD-00"),
        said("SENT-OLD-01"),
        user("SENT-REP-02"),
        json!({"role": "assistant", "content": [{"type": "tool_call", "id": "e1", "name": "EXECUTE", "input": {"command": "SENT-REP-03"}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "call_id": "e1", "content": big}]}),
        json!({"role": "assistant", "content": [
            {"type": "reasoning", "by": "elsewhere", "text": "SENT-HIDDEN-05"},
            {"type": "text", "text": "SENT-REP-05", "_oak_note": "SENT-HIDDEN-50"}]}),
        user("SENT-REP-06"),
        said("SENT-REP-07"),
        user("SENT-KEPT-08"),
        said("SENT-KEPT-09"),
        user("SENT-KEPT-10"),
    ];
    let (n, cut) = (messages.len(), 8);
    let sys = |at: usize| crate::prefix::SysMsg {
        at,
        kind: "date".into(),
        key: format!("{at}"),
        text: format!("SENT-SYS-{at:02}"),
        ..Default::default()
    };
    for (provider, model) in [
        (crate::providers::Kind::Claude, "claude-opus-4-5"),
        (crate::providers::Kind::OpenRouter, "openai/gpt-5"),
        (crate::providers::Kind::Codex, "gpt-5.6-sol"),
        (crate::providers::Kind::Freebuff, "freebuff"),
    ] {
        let mut c = Conversation {
            provider: Some(provider),
            model: model.into(),
            messages: messages.clone().into(),
            ..Default::default()
        };
        c.rolling.summary = Some(crate::rolling::Summary {
            cut: 2,
            text: "SENT-SUMMARY-1".into(),
            made_at: 1,
            clear: 5,
        });
        c.prefix.sys = vec![sys(6), sys(8), sys(10)];
        let what = format!("{provider:?}");
        let sent = summary_messages(&c, provider, n, cut);
        let mut expect = vec![
            "SENT-SUMMARY-1",
            "SENT-REP-02",
            "SENT-REP-03",
            "SENT-REP-05",
            "SENT-REP-06",
            "SENT-REP-07",
        ];
        let mode = crate::prefix::mode(provider, model);
        if mode.is_some() {
            expect.push("SENT-SYS-06");
        }
        let mut got = sentinels_in(&json!(sent));
        got.sort();
        expect.sort();
        assert_eq!(got, expect, "{what}: {sent:?}");
        assert!(
            json!(sent).to_string().contains(crate::rolling::CLEARED),
            "{what}: cleared output goes as cleared"
        );
        // Exactly what the chat request sent before the cut.
        let mut chat = request_messages(&c, &c.messages);
        if let Some(mode) = mode {
            c.prefix.fold(&mut chat, 2, n);
            chat = c.prefix.inject(mode, chat, 2, n);
        }
        assert_eq!(sent[..], chat[..sent.len()], "{what}: as the model saw it");
        assert!(
            sentinels_in(&chat[sent.len()])
                .iter()
                .any(|s| s.contains("-08")),
            "{what}: up to the cut"
        );
        // The request: the chat request's system prompt and tools, these
        // messages, then the instruction (its own five-minute breakpoint).
        let body = oak::Request {
            provider,
            model: model.into(),
            system: "SYSTEM".into(),
            tools: vec![
                json!({"name": "T", "description": "d", "input_schema": {"type": "object"}}),
            ],
            messages: chat.clone(),
            ttl: oak::Ttl::OneHour,
            anchor: Some(chat.len() - 1),
            ..Default::default()
        };
        let r = summary_request_for(&c, &body, n, cut);
        assert_eq!(
            (r.system.as_str(), r.tools.len(), r.ttl, r.anchor),
            ("SYSTEM", 1, oak::Ttl::FiveMinutes, None),
            "{what}"
        );
        assert_eq!(r.messages, sent, "{what}");
        assert!(
            r.instruction
                .as_deref()
                .is_some_and(crate::rolling::is_housekeeping),
            "{what}"
        );
    }
}

/// The result text of call `id` among `c`'s messages.
fn result_of(c: &Conversation, id: &str) -> (String, bool) {
    let r = c
        .messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result" && b["call_id"] == id)
        .unwrap_or_else(|| panic!("no result for {id}"));
    let text = match &r["content"] {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    (text, r["is_error"] == true)
}

/// Code Mode: REPL is the one tool; its script calls the others through
/// their usual checks (the command guard too), and its variables outlast
/// the host (as across a restart of Oak). Skipped without Node.js.
#[test]
fn code_mode_runs_tools_from_a_persistent_repl() {
    if crate::code_mode::node().is_err() {
        eprintln!("skipped: no Node.js");
        return;
    }
    let dir = std::env::temp_dir().join(format!("oak-code-mode-test-{}", api::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
    let (w, s) = idle_worker(
        "code-mode",
        Conversation {
            code_mode: true,
            folder: dir.clone(),
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Default::default()
        },
    );
    let id = s.conv.lock().unwrap().id.clone();
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r, ..) = w.request_body();
    let names: Vec<&str> = r.tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, ["REPL"]);
    assert!(
        r.system.contains("This conversation is in Code Mode")
            && r.system.contains("- READ: ")
            && r.system.contains("Working folder")
    );
    let call = |cid: &str, name: &str, input: Value| json!({"type": "tool_call", "id": cid, "name": name, "input": input});
    let repl = |cid: &str, code: &str| call(cid, "REPL", json!({"code": code}));
    let run = |calls: Vec<Value>| {
        s.conv
            .lock()
            .unwrap()
            .messages
            .push(json!({"role": "assistant", "content": calls}));
        w.run_calls(&calls, oak::stop::TOOL_CALLS);
    };
    run(vec![
        repl(
            "a",
            "const t = await file('a.txt').text(); let n = t.length; const out = await $`echo hi`;\nawait edit('a.txt', 'world', 'there');\nconst m = new Map([['k', [1, 2n]]]);\nlog('logged'); [n, out.stdout.trim()]",
        ),
        call(
            "b",
            "EXECUTE",
            json!({"command": "touch made.txt", "timeout": 5, "summary": "x"}),
        ),
        repl("c", "await $`rm -rf /`"),
        repl(
            "d",
            "const r = await READ({path: 'a.txt'}); r.includes('hello there')",
        ),
        repl("e", "const x = 1; const x = 2"),
    ]);
    {
        let c = s.conv.lock().unwrap();
        let (a, err) = result_of(&c, "a");
        assert!(
            !err && a.contains("<repl-output eval=\"1\">")
                && a.contains("logged")
                && a.contains("12")
                && a.contains("hi"),
            "{a}"
        );
        let (b, err) = result_of(&c, "b");
        assert!(err && b.contains("your one tool is REPL"), "{b}");
        let (cc, err) = result_of(&c, "c");
        assert!(err && cc.contains("<error>"), "{cc}");
        let (d, err) = result_of(&c, "d");
        assert!(!err && d.contains("true"), "{d}");
        let (e, err) = result_of(&c, "e");
        assert!(err && e.contains("already been declared"), "{e}");
        // The script's calls are listed with its result.
        let a = c
            .messages
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .find(|b| b["call_id"] == "a")
            .unwrap();
        let calls: Vec<&str> = a[repl::CALLS_KEY]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["name"].as_str())
            .collect();
        assert_eq!(calls, ["READ", "$", "EDIT"]);
    }
    assert!(!dir.join("made.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "hello there\n"
    );
    // The host goes (idle, or as when Oak closes): the next one has the variables.
    assert!(crate::code_mode::snapshot_path(&id).exists());
    crate::code_mode::stop_idle(Duration::ZERO);
    run(vec![repl(
        "f",
        "[n, t.trim(), m.get('k')[1] + 1n, typeof $]",
    )]);
    {
        let c = s.conv.lock().unwrap();
        let (f, err) = result_of(&c, "f");
        assert!(
            !err && f.contains("12")
                && f.contains("hello world")
                && f.contains("3n")
                && f.contains("function"),
            "{f}"
        );
    }
    // Deleted: its snapshot goes.
    drop(w);
    drop(s);
    assert!(!crate::code_mode::snapshot_path(&id).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without Node.js, Code Mode is unavailable and says why.
#[test]
fn code_mode_needs_node() {
    let (w, s) = idle_worker(
        "code-mode-no-node",
        Conversation {
            code_mode: true,
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            ..Default::default()
        },
    );
    let calls =
        vec![json!({"type": "tool_call", "id": "a", "name": "REPL", "input": {"code": "1"}})];
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "user", "content": [oak::text("go")]}));
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": calls}));
    let id = s.conv.lock().unwrap().id.clone();
    crate::code_mode::without_node(&id, || w.run_calls(&calls, oak::stop::TOOL_CALLS));
    let (text, err) = result_of(&s.conv.lock().unwrap(), "a");
    assert!(err && text.contains("Code Mode needs Node.js"), "{text}");
    // REPL outside Code Mode, and Freebuff in it, are refused.
    s.conv.lock().unwrap().code_mode = false;
    let calls =
        vec![json!({"type": "tool_call", "id": "b", "name": "REPL", "input": {"code": "1"}})];
    s.conv
        .lock()
        .unwrap()
        .messages
        .push(json!({"role": "assistant", "content": calls}));
    w.run_calls(&calls, oak::stop::TOOL_CALLS);
    let (text, err) = result_of(&s.conv.lock().unwrap(), "b");
    assert!(err && text.contains("isn't in Code Mode"), "{text}");
    assert!(
        code_mode_refuses_provider(true, api::Provider::Freebuff).is_some()
            && code_mode_refuses_provider(false, api::Provider::Freebuff).is_none()
    );
    assert!(
        code_mode_refuses_provider(true, api::Provider::Plugin).is_some()
            && code_mode_refuses_provider(false, api::Provider::Plugin).is_none()
    );
}

/// Each Code Mode conversation has a REPL context of its own: one's
/// variables (live or restored) are never another's. Skipped without Node.js.
#[test]
fn code_mode_conversations_keep_their_own_variables() {
    if crate::code_mode::node().is_err() {
        eprintln!("skipped: no Node.js");
        return;
    }
    let conv = || Conversation {
        code_mode: true,
        folder: std::env::temp_dir(),
        model: "claude-opus-5-5".into(),
        effort: "high".into(),
        ..Default::default()
    };
    let (wa, sa) = idle_worker("code-mode-a", conv());
    let (wb, sb) = idle_worker("code-mode-b", conv());
    let run = |w: &Worker, s: &Deleting, cid: &str, code: &str| {
        let calls =
            vec![json!({"type": "tool_call", "id": cid, "name": "REPL", "input": {"code": code}})];
        s.conv
            .lock()
            .unwrap()
            .messages
            .push(json!({"role": "assistant", "content": calls}));
        w.run_calls(&calls, oak::stop::TOOL_CALLS);
        result_of(&s.conv.lock().unwrap(), cid)
    };
    for s in [&sa, &sb] {
        s.conv
            .lock()
            .unwrap()
            .messages
            .push(json!({"role": "user", "content": [oak::text("go")]}));
    }
    assert!(
        !run(
            &wa,
            &sa,
            "a1",
            "const secret = 'from a'; var shared = 1; secret"
        )
        .1
    );
    let (b, err) = run(
        &wb,
        &sb,
        "b1",
        "[typeof secret, typeof shared, typeof globalThis.shared]",
    );
    assert!(
        !err && b.contains("'undefined', 'undefined', 'undefined'"),
        "{b}"
    );
    assert!(!run(&wb, &sb, "b2", "const secret = 'from b'; secret").1);
    // Restarted from their snapshots, each has its own again.
    crate::code_mode::stop_idle(Duration::ZERO);
    let (a, _) = run(&wa, &sa, "a2", "[secret, shared]");
    let (b, _) = run(&wb, &sb, "b3", "[secret, typeof shared]");
    assert!(
        a.contains("'from a', 1") && b.contains("'from b', 'undefined'"),
        "{a}\n{b}"
    );
}

/// /swarm makes a conversation the coordinator (the kit in its first
/// request, its instructions in the system prompt); members get the worker
/// instructions and the SWARM tool instead of AGENT; posts reach the
/// members they name; SWARM wait parks a member; kill is for good; the
/// monitor posts once when everyone goes quiet; members' turns end
/// without a report to the coordinator.
#[test]
fn swarms_talk_on_their_board() {
    let (cw, cs) = idle_worker("swarm-coord", Conversation { model: "claude-opus-5-5".into(), ..Default::default() });
    let coord = cs.conv.lock().unwrap().id.clone();
    // A chat (or a subagent) can't run one; a conversation can.
    assert!(swarm::refuses(&Conversation { chat: true, ..Default::default() }).is_some());
    assert!(swarm::refuses(&Conversation { parent: Some("x".into()), ..Default::default() }).is_some());
    let (block, mark) = apply_command(&mut cs.conv.lock().unwrap(), Command::Swarm("rewrite the parser".into()));
    assert!(block.unwrap().to_string().contains("rewrite the parser") && mark.unwrap().0 == MarkKind::Swarm);
    {
        let c = cs.conv.lock().unwrap();
        assert_eq!(c.swarm.as_ref().unwrap().name, "coordinator");
        assert_eq!(c.prefix.kits, ["SWARM"]);
    }
    cs.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r, ..) = cw.request_body();
    assert!(r.system.contains("# You are the swarm coordinator") && !r.system.contains("one agent in a swarm"));
    assert!(r.tools.iter().any(|t| t["name"] == "SWARM"));

    // Two members, as SWARM add makes them (without starting them).
    let member = |name: &str| {
        let (w, s) = idle_worker(
            &format!("swarm-{name}"),
            Conversation {
                model: "claude-opus-5-5".into(),
                title: format!("@{name}"),
                parent: Some(coord.clone()),
                agent_id: 1,
                swarm: Some(crate::store::Seat { swarm: coord.clone(), name: name.into(), killed: false }),
                prefix: crate::prefix::State { kits: vec!["SWARM".into()], ..Default::default() },
                ..Default::default()
            },
        );
        let id = s.conv.lock().unwrap().id.clone();
        let sw = crate::swarm::get(&coord);
        let mut b = sw.lock().unwrap();
        b.roster.members.push(crate::swarm::Member { name: name.into(), conv: id, added_by: "coordinator".into(), ..Default::default() });
        b.save();
        drop(b);
        (w, s)
    };
    let (pw, ps) = member("parser");
    let (_vw, vs) = member("verifier");
    ps.conv.lock().unwrap().messages.push(json!({"role": "user", "content": [oak::text("go")]}));
    let (r, ..) = pw.request_body();
    assert!(r.system.contains("You are @parser, one agent in a swarm") && !r.system.contains("You are a subagent"));
    assert!(r.tools.iter().any(|t| t["name"] == "SWARM") && !r.tools.iter().any(|t| t["name"] == "AGENT"));

    // A post reaches whom it names, as a board post.
    let out = cw.tool("SWARM_POST", &json!({"text": "@parser take src/parse.rs"})).0.unwrap();
    assert!(out.contains("went to @parser"), "{out}");
    let inbox = ps.conv.lock().unwrap().inbox.clone();
    assert_eq!(inbox.len(), 1);
    assert!(inbox[0].contains("from=\"@coordinator\" type=\"board post\" seq=\"1\""));
    assert!(vs.conv.lock().unwrap().inbox.is_empty());
    ps.conv.lock().unwrap().inbox.clear();
    // Reading; waiting with nothing new parks the member.
    assert!(pw.tool("SWARM_READ", &json!({})).0.unwrap().contains("#1 @coordinator: @parser take src/parse.rs"));
    assert!(pw.tool("SWARM_WAIT", &json!({"since": 0})).0.unwrap().starts_with("Not waiting"));
    assert!(pw.tool("SWARM_WAIT", &json!({"since": 1})).0.unwrap().starts_with("Asleep"));
    assert!(pw.parked() && !pw.parked());
    // Its turn ends without a final reply to the coordinator.
    pw.report_to_parent();
    assert!(cs.conv.lock().unwrap().inbox.is_empty());
    // Status lists everyone; names are checked; the title is the coordinator's.
    let status = cw.tool("SWARM_STATUS", &json!({})).0.unwrap();
    assert!(status.contains("@coordinator (you)") && status.contains("@parser: waiting") && status.contains("@verifier: waiting"), "{status}");
    assert!(pw.tool("SWARM_TITLE", &json!({"title": "x"})).0.is_err());
    assert!(cw.tool("SWARM_TITLE", &json!({"title": "Parser rewrite"})).0.is_ok());
    assert!(cw.tool("SWARM_ADD", &json!({"name": "parser", "brief": "again"})).0.unwrap_err().contains("taken"));
    assert!(cw.tool("SWARM_ADD", &json!({"name": "all", "brief": "x"})).0.unwrap_err().contains("reserved"));
    // A member can't kill one it didn't add.
    assert!(pw.tool("SWARM_KILL", &json!({"name": "verifier"})).0.unwrap_err().contains("ask @coordinator"));

    // Everyone quiet, the job not done: the monitor posts once.
    let seq = swarm::check_stall(&coord).expect("a stall post");
    assert!(cs.conv.lock().unwrap().inbox.last().unwrap().contains("[stall]"));
    assert_eq!(swarm::check_stall(&coord), None);
    // Done: it stays quiet.
    cw.tool("SWARM_POST", &json!({"text": "[done] rewritten"})).0.unwrap();
    assert_eq!(swarm::check_stall(&coord), None);
    assert!(seq > 1);

    // Kill: for good; what comes for it is dropped, and the user can't send to it.
    assert!(cw.tool("SWARM_KILL", &json!({"name": "verifier"})).0.unwrap().contains("Killed @verifier"));
    assert!(vs.killed_member());
    cw.tool("SWARM_POST", &json!({"text": "@all wrap up"})).0.unwrap();
    assert!(vs.conv.lock().unwrap().inbox.is_empty());
    assert!(cw.tool("SWARM_STATUS", &json!({})).0.unwrap().contains("@verifier: killed"));
    assert!(cw.tool("SWARM_ADD", &json!({"name": "verifier", "brief": "x"})).0.unwrap_err().contains("taken"));
    crate::swarm::remove(&coord);
}
