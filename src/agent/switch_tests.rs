//! Switching a conversation between providers mid-conversation, above all
//! AWS Quick and Claude, back and forth: what each request carries, what
//! state survives the switch and what gives way to it.

use super::tests::idle_worker;
use super::*;

const CLAUDE: &str = "claude-opus-5-5";
const QUICK: &str = "smart";

fn user(t: &str) -> Value {
    json!({"role": "user", "content": [oak::text(t)]})
}

/// A reply as `by`'s model wrote it.
fn reply(by: api::Provider, blocks: Vec<Value>) -> Value {
    let mut blocks = blocks;
    mark_by(&mut blocks, Some(by));
    json!({"role": "assistant", "content": blocks})
}

fn all_text(v: &[Value]) -> String {
    serde_json::to_string(v).unwrap()
}

fn switch(s: &Session, p: api::Provider, m: &str) {
    let mut c = s.conv.lock().unwrap();
    apply_command(&mut c, Command::Model(p, m.into()));
}

/// Claude -> Quick -> Claude -> Quick -> Claude: each request goes to the
/// model picked, with what that provider takes. Quick hears what Claude did
/// since its own last reply (and nothing twice), on the same thread every
/// time; Claude never gets Quick's own snippet calls or its recovery
/// prompts; the effort and Quick's thread outlive every switch.
#[test]
fn alternating_quick_and_claude_keeps_every_request_whole() {
    use crate::quick::{QUIET_KEY, recovery};
    let thread = recovery::Session { account: "switch-account".into(), thread: "switch-thread".into(), ..Default::default() };
    let (w, s) = idle_worker("switch-alternate", Conversation {
        provider: Some(api::Provider::Claude),
        model: CLAUDE.into(),
        effort: "high".into(),
        messages: vec![user("Build the parser")].into(),
        ..Default::default()
    });
    let claude_turn = |s: &Session, n: usize| {
        let mut c = s.conv.lock().unwrap();
        let id = format!("toolu_c{n}");
        c.messages.push(reply(api::Provider::Claude, vec![
            oak::text(&format!("Claude step {n}")),
            json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": "a.rs"}}),
        ]));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": format!("claude result {n}")}]}));
        c.messages.push(reply(api::Provider::Claude, vec![oak::text(&format!("Claude done {n}"))]));
    };
    let quick_turn = |s: &Session, n: usize| {
        let mut c = s.conv.lock().unwrap();
        let (snip, inner) = (format!("snip{n}"), format!("q{n}"));
        c.messages.push(reply(api::Provider::Quick, vec![
            oak::text(&format!("Quick step {n}")),
            json!({"type": "tool_call", "id": snip, "name": "run_python_on_device", QUIET_KEY: true, "input": {"code": "READ(path=\"b.rs\")"}}),
            json!({"type": "tool_call", "id": inner, "name": "READ", "input": {"path": "b.rs"}}),
        ]));
        c.messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "call_id": snip, "content": "snippet ran"},
            {"type": "tool_result", "call_id": inner, "content": format!("quick result {n}")},
        ]}));
        c.quick_session = Some(thread.clone());
    };
    let ask = |s: &Session, t: &str| s.conv.lock().unwrap().messages.push(user(t));

    claude_turn(&s, 1);
    for round in 1..=3 {
        // To Quick: it hears Claude's turn since its own last reply.
        ask(&s, &format!("quick round {round}"));
        switch(&s, api::Provider::Quick, QUICK);
        let (req, ..) = w.request_body();
        assert_eq!((req.provider, req.model.as_str()), (api::Provider::Quick, QUICK));
        assert_eq!(crate::quick::new_user_text(&req).as_deref(), Some(format!("quick round {round}").as_str()));
        let unseen = all_text(&req.unseen);
        assert!(unseen.contains(&format!("Claude done {round}")) && unseen.contains(&format!("claude result {round}")), "round {round}: {unseen}");
        if round > 1 {
            assert!(!unseen.contains(&format!("Quick step {}", round - 1)), "Quick's own reply isn't sent back to it: {unseen}");
            assert!(!unseen.contains(&format!("Claude done {}", round - 1)), "nor what it heard before: {unseen}");
            assert_eq!(req.quick_session.as_ref(), Some(&thread), "the same thread every time");
        }
        quick_turn(&s, round);

        // Back to Claude: the whole conversation, Quick's snippets out.
        ask(&s, &format!("claude round {round}"));
        switch(&s, api::Provider::Claude, CLAUDE);
        let (req, ..) = w.request_body();
        assert_eq!((req.provider, req.model.as_str()), (api::Provider::Claude, CLAUDE));
        assert!(req.quick_session.is_none() && req.unseen.is_empty());
        assert_eq!(req.effort.as_deref(), Some("high"), "the effort outlives the switches");
        let sent = all_text(&req.messages);
        assert!(!sent.contains("run_python_on_device") && !sent.contains("snippet ran"), "round {round}: {sent}");
        for n in 1..=round {
            assert!(sent.contains(&format!("quick result {n}")) && sent.contains(&format!("Quick step {n}")), "Quick's work reaches Claude: {sent}");
        }
        // Every call has its result, right after it.
        for (k, m) in req.messages.iter().enumerate().filter(|(_, m)| m["role"] == "assistant") {
            for call in m["content"].as_array().unwrap().iter().filter(|b| oak::is_call(b)) {
                let next = &req.messages[k + 1];
                assert!(next["content"].as_array().unwrap().iter().any(|b| b["call_id"] == call["id"]), "{call} unanswered");
            }
        }
        assert_eq!(req.messages.last().unwrap()["role"], "user");
        claude_turn(&s, round + 1);
    }
    assert_eq!(s.conv.lock().unwrap().quick_session, Some(thread));
}

/// What Oak told Quick to get its own thread going again (look for the
/// device, go on after an error, a fresh thread) reaches another provider
/// as one plain carry-on line; Quick still reads its own.
#[test]
fn quick_recovery_prompts_read_as_a_carry_on_elsewhere() {
    use crate::quick::recovery;
    let mut fresh = context_block(recovery::LIMIT_FRESH);
    fresh[recovery::FRESH_KEY] = json!("t2");
    let messages = vec![
        user("Fix it"),
        reply(api::Provider::Quick, vec![oak::text("Partial")]),
        json!({"role": "user", "content": [context_block(recovery::DEVICE_RETRY)]}),
        reply(api::Provider::Quick, vec![oak::text("Partial again")]),
        json!({"role": "user", "content": [context_block(recovery::ERROR_RETRY), fresh.clone(), oak::text("and also this")]}),
    ];
    let claude = for_provider(&messages, api::Provider::Claude).into_owned();
    let sent = all_text(&claude);
    for t in [recovery::DEVICE_RETRY, recovery::ERROR_RETRY, recovery::LIMIT_FRESH] {
        assert!(!sent.contains(t), "{t} reached Claude");
    }
    assert!(!sent.contains("list_outposts"), "{sent}");
    assert_eq!(claude[2]["content"], json!([oak::text(CARRY_ON)]));
    assert_eq!(claude[4]["content"], json!([oak::text(CARRY_ON), oak::text("and also this")]), "one line per turn, the user's text kept");
    assert_eq!(claude.len(), messages.len(), "every turn stays");
    // Quick reads them as they were.
    assert!(matches!(for_provider(&messages, api::Provider::Quick), std::borrow::Cow::Borrowed(_)));
    // A conversation with none goes as it is.
    let plain = vec![user("a"), reply(api::Provider::Claude, vec![oak::text("b")])];
    assert!(matches!(for_provider(&plain, api::Provider::Claude), std::borrow::Cow::Borrowed(_)));
}

/// A message steered into a Quick turn is left out for Quick (its thread
/// has it), unless a fresh thread opened since: that one never saw it.
#[test]
fn a_fresh_quick_thread_gets_messages_steered_into_the_old_one() {
    use crate::quick::{STEERED_KEY, recovery};
    let steered = |t: &str| {
        let mut b = oak::text(t);
        b[STEERED_KEY] = json!(true);
        b
    };
    let mut fresh = context_block(recovery::LIMIT_FRESH);
    fresh[recovery::FRESH_KEY] = json!("t2");
    let messages = vec![
        user("Start"),
        reply(api::Provider::Quick, vec![oak::text("on it")]),
        json!({"role": "user", "content": [steered("old thread steer"), fresh]}),
        reply(api::Provider::Quick, vec![oak::text("fresh reply")]),
        json!({"role": "user", "content": [steered("new thread steer")]}),
    ];
    let quick = for_provider(&messages, api::Provider::Quick).into_owned();
    let sent = all_text(&quick);
    assert!(sent.contains("old thread steer"), "the fresh thread never saw it: {sent}");
    assert!(!sent.contains("new thread steer"), "the fresh thread has it: {sent}");
    // With no fresh thread, every steered message stays out.
    let one = vec![user("Start"), reply(api::Provider::Quick, vec![oak::text("on it")]), json!({"role": "user", "content": [steered("s"), oak::text("t")]})];
    assert_eq!(for_provider(&one, api::Provider::Quick)[2]["content"], json!([oak::text("t")]));
    // Other providers read them all, as typed.
    let claude = all_text(&for_provider(&messages, api::Provider::Claude));
    assert!(claude.contains("old thread steer") && claude.contains("new thread steer"));
}

/// The user switches to Claude while a Quick turn fails (a service error,
/// the device out of reach, the account at its limit): no retry on Quick,
/// no move to another Quick account; the turn goes on on Claude at once,
/// whose request ends with a user turn (a plain carry-on after the reply
/// Quick cut short), and never with Quick's recovery prompts.
#[test]
fn a_switch_during_quick_recovery_goes_on_on_the_new_model() {
    use crate::quick::recovery;
    for reason in [recovery::ERROR, recovery::DEVICE, recovery::LIMIT, recovery::RATE] {
        let (w, s) = idle_worker(&format!("switch-recovery-{reason}"), Conversation {
            provider: Some(api::Provider::Quick),
            model: QUICK.into(),
            messages: vec![user("Fix the build")].into(),
            ..Default::default()
        });
        let mut request = 0;
        let mut retries = 0;
        std::thread::scope(|scope| w.turn_loop_with(scope, |_| {
            let (req, ..) = w.request_body();
            request += 1;
            match request {
                1 => {
                    assert_eq!(req.provider, api::Provider::Quick);
                    // Picked while Quick's turn runs.
                    s.command(Command::Model(api::Provider::Claude, CLAUDE.into()));
                    let partial = json!({"type": "text", "text": "Half done", BY_KEY: api::Provider::Quick.id()});
                    Reply::Done(vec![partial], format!("{reason}\nboom"), Usage::default())
                }
                2 => {
                    assert_eq!((req.provider, req.model.as_str()), (api::Provider::Claude, CLAUDE), "{reason}");
                    let last = req.messages.last().unwrap();
                    assert_eq!(last["role"], "user", "{reason}: the request ends with a user turn");
                    assert_eq!(last["content"], json!([oak::text(CARRY_ON)]), "{reason}");
                    let sent = all_text(&req.messages);
                    assert!(sent.contains("Half done"), "{reason}: {sent}");
                    for t in [recovery::DEVICE_RETRY, recovery::ERROR_RETRY, recovery::LIMIT_FRESH, recovery::RATE_FRESH, recovery::DEVICE_FRESH] {
                        assert!(!sent.contains(t), "{reason}: {sent}");
                    }
                    Reply::Done(vec![oak::text("Fixed")], oak::stop::END.into(), Usage::default())
                }
                _ => panic!("{reason}: one request on each model"),
            }
        }, |attempts, _| {
            *attempts += 1;
            retries += 1;
            true
        }));
        assert_eq!((request, retries), (2, 0), "{reason}");
        let c = s.conv.lock().unwrap();
        assert!(c.quick_session.is_none(), "{reason}: no fresh Quick thread");
        assert!(!c.marks.iter().any(|m| m.kind == MarkKind::QuickHandoff), "{reason}");
        assert_eq!(reply_text(&c).as_deref(), Some("Fixed"));
    }
}

/// Quick tiers are one provider: a switch between them mid-recovery
/// doesn't stop Quick getting its thread going again.
#[test]
fn a_quick_tier_switch_keeps_quick_recovering() {
    use crate::quick::recovery;
    let (w, s) = idle_worker("switch-quick-tier", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: vec![user("Go")].into(),
        ..Default::default()
    });
    let mut request = 0;
    std::thread::scope(|scope| w.turn_loop_with(scope, |_| {
        let (req, ..) = w.request_body();
        request += 1;
        if request == 1 {
            s.command(Command::Model(api::Provider::Quick, "fast".into()));
            return Reply::Done(vec![oak::text("Partial")], recovery::ERROR.into(), Usage::default());
        }
        assert_eq!((req.provider, req.model.as_str()), (api::Provider::Quick, "fast"));
        assert_eq!(crate::quick::new_user_text(&req).as_deref(), Some(recovery::ERROR_RETRY));
        Reply::Done(vec![oak::text("Done")], oak::stop::COMPLETE.into(), Usage::default())
    }, |attempts, _| {
        *attempts += 1;
        true
    }));
    assert_eq!(request, 2);
}

/// Quick's token count is of its own thread: it sizes the context, but a
/// summary made after a switch to Claude doesn't place its cut by it. And a
/// reply from another provider ends keeping the Claude request warm.
#[test]
fn quick_usage_is_not_a_measure_of_what_claude_is_sent() {
    let (w, s) = idle_worker("switch-measures", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: vec![user("a"), reply(api::Provider::Quick, vec![oak::text("b")]), user("c")].into(),
        ..Default::default()
    });
    let usage = |prompt| Usage { prompt, output: 100, ..Default::default() };
    let (body, n, base) = w.request_body();
    *w.warm.lock().unwrap() = Some((body.clone(), false));
    *w.last_request.lock().unwrap() = Some((body, n, base, api::now_ms()));
    w.after_reply(usage(90_000));
    {
        let c = s.conv.lock().unwrap();
        assert!(c.rolling.measured.is_empty(), "{:?}", c.rolling.measured);
        assert_eq!(c.rolling.context, 90_100);
    }
    assert!(w.warm.lock().unwrap().is_none(), "nothing of Claude's to keep warm after Quick's reply");
    // On Claude, the count is the request's own.
    switch(&s, api::Provider::Claude, CLAUDE);
    let (body, n, base) = w.request_body();
    *w.last_request.lock().unwrap() = Some((body, n, base, api::now_ms()));
    w.after_reply(usage(5_000));
    let c = s.conv.lock().unwrap();
    assert_eq!(c.rolling.measured.len(), 1);
    assert_eq!((c.rolling.measured[0].n, c.rolling.measured[0].prompt), (n, 5_000));
}

/// A conversation past what one summary request on the new model can hold
/// (Quick has no summaries; another model's window may be bigger) is
/// summarized in parts: each cut at the latest boundary that fits, the
/// next summary taking on from it, till the kept part is what the
/// settings keep.
#[test]
fn a_conversation_past_the_new_window_is_summarized_in_parts() {
    // 40 turns of about 10k tokens each.
    let big = "x".repeat(40_000);
    let mut m = vec![];
    for i in 0..40 {
        m.push(user(&format!("turn {i} {big}")));
        m.push(json!({"role": "assistant", "content": [oak::text(&format!("reply {i}"))]}));
    }
    m.push(user("now"));
    let total = rolling::estimate(m.iter().map(rolling::chars).sum());
    let keep = 40_000;
    let whole = rolling::plan_within(&m, 0, 0, total, &[], keep, 10_000, false, u64::MAX).unwrap();
    assert!(whole.before > 300_000, "{}", whole.before);
    let cap = 150_000;
    let first = rolling::plan_within(&m, 0, 0, total, &[], keep, 10_000, false, cap).unwrap();
    assert!(first.before <= cap && first.before > cap - 20_000, "as much as fits: {}", first.before);
    assert!(first.cut < whole.cut && (m[first.cut]["role"] == "user" || m[first.cut]["role"] == "assistant"));
    // Within the cap the plan is the same as without one.
    assert_eq!(rolling::plan_within(&m, 0, 0, total, &[], keep, 10_000, false, whole.before), Some(whole));
    // Nothing that fits: no plan.
    assert_eq!(rolling::plan_within(&m, 0, 0, total, &[], keep, 10_000, false, 1_000), None);
    // The cap is the model's window's three quarters.
    let w = api::context_window(CLAUDE).unwrap();
    assert_eq!(rolling::summary_cap(CLAUDE), w * 3 / 4);
}

/// A fork of a conversation Quick worked on goes on in a Quick thread of its
/// own, a new one, which has none of it: its first Quick request sends the
/// whole conversation, Quick's own replies on the source's thread too, with
/// Oak's briefing. Claude reads the fork as it read the source.
#[test]
fn a_fork_sends_its_new_quick_thread_the_whole_conversation() {
    let source = Conversation {
        id: "switch-fork-source".into(),
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: vec![
            user("Start the port"),
            reply(api::Provider::Quick, vec![oak::text("Ported the lexer")]),
            user("Now the parser"),
            reply(api::Provider::Claude, vec![oak::text("Parser ported")]),
            user("And the tests"),
            reply(api::Provider::Quick, vec![oak::text("Tests ported")]),
        ]
        .into(),
        quick_session: Some(crate::quick::recovery::Session { account: "a".into(), thread: "source-thread".into(), ..Default::default() }),
        ..Default::default()
    };
    let mut c = crate::fork::fork(&source, "switch-fork-copy".into(), 1, None).unwrap();
    assert!(c.quick_session.is_none(), "not the source's thread");
    assert!(crate::quick::moving(&c.id), "Quick gets the briefing and all of it");
    c.messages.push(user("Carry on"));
    let (w, _s) = idle_worker("switch-fork", c);
    let (req, ..) = w.request_body();
    let unseen = all_text(&req.unseen);
    for t in ["Start the port", "Ported the lexer", "Parser ported", "Tests ported"] {
        assert!(unseen.contains(t), "{t} missing: {unseen}");
    }
    // The source's own thread still has it all: nothing changes there.
    let mut src = source.clone();
    src.messages.push(user("Carry on"));
    let (w, _s) = idle_worker("switch-fork-src", src);
    assert!(w.request_body().0.unseen.is_empty());
    // On Claude, the fork reads as the source does.
    let (w, s) = idle_worker("switch-fork-claude", crate::fork::fork(&source, "switch-fork-claude-copy".into(), 1, None).unwrap());
    switch(&s, api::Provider::Claude, CLAUDE);
    s.conv.lock().unwrap().messages.push(user("Carry on"));
    let text = |m: &[Value]| m.iter().map(|m| m["content"].clone()).collect::<Vec<_>>();
    let forked = text(&w.request_body().0.messages);
    let (w, s) = idle_worker("switch-fork-claude-src", source.clone());
    switch(&s, api::Provider::Claude, CLAUDE);
    s.conv.lock().unwrap().messages.push(user("Carry on"));
    assert_eq!(forked, text(&w.request_body().0.messages));
}

/// Claude -> Quick -> Claude: what Claude cached lives on at Anthropic, so
/// back on the same model the cache state comes back as it was (its hour's
/// entry anchored and read, not written again). Another Claude model, or
/// Quick itself, starts cold; Quick's requests leave Claude's state alone.
#[test]
fn claudes_cache_outlives_a_detour_to_quick() {
    use crate::cache_ttl::Choice;
    let mut c = Conversation {
        id: "switch-cache".into(),
        provider: Some(api::Provider::Claude),
        model: CLAUDE.into(),
        ..Default::default()
    };
    let now = api::now_ms();
    c.cache.record(now, Choice { ttl: oak::Ttl::OneHour, anchor: None }, 50_000, 10, 0);
    let claude = c.cache.clone();
    assert!(claude.hour.is_some());
    apply_command(&mut c, Command::Model(api::Provider::Quick, QUICK.into()));
    assert!(c.cache.hour.is_none() && c.cache.last.is_none(), "Quick has none of it");
    // A Quick reply is recorded in Quick's state.
    c.cache.record(now + 1_000, Choice { ttl: oak::Ttl::FiveMinutes, anchor: None }, 80_000, 12, 0);
    apply_command(&mut c, Command::Model(api::Provider::Quick, "fast".into()));
    apply_command(&mut c, Command::Model(api::Provider::Claude, CLAUDE.into()));
    assert_eq!(c.cache, claude, "back on the model that cached it");
    let choice = c.cache.choose(now + 120_000, 55_000, 0, 120_000);
    assert!(choice.ttl == oak::Ttl::OneHour || choice.anchor == Some(9), "the hour's entry is used: {choice:?}");
    // To another Claude model and back: cold there, its own state back here.
    apply_command(&mut c, Command::Model(api::Provider::Claude, "claude-sonnet-5-5".into()));
    assert!(c.cache.hour.is_none());
    apply_command(&mut c, Command::Model(api::Provider::Claude, CLAUDE.into()));
    assert_eq!(c.cache, claude);
    // An effort set while away is another cache for a model whose effort
    // can't change per message (claude-sonnet-5 here), and the same one
    // where it can.
    let old = "claude-sonnet-5";
    assert!(!crate::prefix::supports_effort(old));
    apply_command(&mut c, Command::Model(api::Provider::Claude, old.into()));
    c.cache = claude.clone();
    apply_command(&mut c, Command::Model(api::Provider::Quick, QUICK.into()));
    apply_command(&mut c, Command::Effort("low".into()));
    apply_command(&mut c, Command::Model(api::Provider::Claude, old.into()));
    assert!(c.cache.hour.is_none() && c.cache.cold, "another effort, another cache");
    apply_command(&mut c, Command::Model(api::Provider::Claude, CLAUDE.into()));
    c.cache = claude.clone();
    apply_command(&mut c, Command::Model(api::Provider::Quick, QUICK.into()));
    apply_command(&mut c, Command::Effort("max".into()));
    apply_command(&mut c, Command::Model(api::Provider::Claude, CLAUDE.into()));
    assert_eq!(c.cache, claude, "per-message effort: the same cache");
    // Quick, then another Claude model: cold; the first model's state stays for later.
    apply_command(&mut c, Command::Model(api::Provider::Quick, QUICK.into()));
    apply_command(&mut c, Command::Model(api::Provider::Claude, "claude-sonnet-5-5".into()));
    assert!(c.cache.hour.is_none() && c.cache.cold);
}

/// A side chat of a conversation Quick worked on is a new Quick thread too:
/// its first Quick request sends the whole conversation, with the side
/// chat's instruction as the message; on Claude it sends what the source
/// sent, then that instruction.
#[test]
fn a_side_chat_sends_its_new_quick_thread_the_whole_conversation() {
    let source = Conversation {
        id: "switch-side-source".into(),
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: vec![
            user("Port it"),
            reply(api::Provider::Quick, vec![oak::text("Ported the lexer")]),
            user("More"),
            reply(api::Provider::Quick, vec![oak::text("Ported the parser")]),
        ]
        .into(),
        ..Default::default()
    };
    let c = crate::side_chat::fork(&source, "switch-side-copy".into(), 1);
    let (w, _s) = idle_worker("switch-side", c);
    let (req, ..) = w.request_body();
    let unseen = all_text(&req.unseen);
    for t in ["Port it", "Ported the lexer", "Ported the parser"] {
        assert!(unseen.contains(t), "{t} missing: {unseen}");
    }
    assert_eq!(crate::quick::new_user_text(&req).as_deref(), Some(crate::side_chat::FORK_MESSAGE));
    // On Claude, the source's messages as they were, then the instruction.
    let mut side = crate::side_chat::fork(&source, "switch-side-claude".into(), 1);
    apply_command(&mut side, Command::Model(api::Provider::Claude, CLAUDE.into()));
    let mut src = source.clone();
    apply_command(&mut src, Command::Model(api::Provider::Claude, CLAUDE.into()));
    let content = |m: &[Value]| m.iter().map(|m| m["content"].clone()).collect::<Vec<_>>();
    let (sent, from) = (request_messages(&side, &side.messages), request_messages(&src, &src.messages));
    assert_eq!(content(&sent[..from.len()]), content(&from));
}

/// Where no summary request on the model can hold the oldest part left (a
/// message bigger than it, from a model with a bigger window), the turn
/// says so and what to do, rather than that nothing is old enough.
#[test]
fn a_part_too_big_for_one_summary_says_why() {
    let cap = rolling::summary_cap(CLAUDE);
    let huge = "y".repeat((cap as usize + 20_000) * 4);
    // (What follows it is more than the part kept: the cut comes after it.)
    let tail = "z".repeat(120_000);
    let mut c = Conversation {
        provider: Some(api::Provider::Claude),
        model: CLAUDE.into(),
        messages: vec![
            user(&huge),
            reply(api::Provider::Quick, vec![oak::text("read it")]),
            user(&tail),
            reply(api::Provider::Quick, vec![oak::text("on")]),
            user(&tail),
            reply(api::Provider::Quick, vec![oak::text("on again")]),
            user("and now"),
        ]
        .into(),
        ..Default::default()
    };
    let total = rolling::estimate(c.messages.iter().map(rolling::chars).sum());
    // (The sizes as given: other tests change the settings' meanwhile.)
    c.messages.load_all();
    let plan = |cap| rolling::plan_within(&c.messages, 0, 0, total, &[], 40_000, 10_000, true, cap);
    assert!(plan(cap).is_none(), "nothing fits one request");
    assert!(plan(u64::MAX).is_some(), "though a summary could be made");
    let why = too_big_to_summarize(CLAUDE);
    assert!(why.contains(&api::Provider::model_name(CLAUDE)) && why.contains("larger context window"), "{why}");
}

/// A failed retry wait (a refusal's, Quick's backoff) ends at once when the
/// user switches to another provider's model, which takes the retry.
#[test]
fn a_switch_ends_a_retry_wait() {
    let (w, s) = idle_worker("switch-retry-wait", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        ..Default::default()
    });
    assert!(!w.switching_away());
    s.command(Command::Model(api::Provider::Quick, "fast".into()));
    assert!(!w.switching_away(), "another Quick tier is the same provider");
    s.command(Command::Model(api::Provider::Claude, CLAUDE.into()));
    assert!(w.switching_away());
    let t = Instant::now();
    assert!(w.retry_sleep(Duration::from_secs(30)));
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

fn result_of(id: &str, text: &str) -> Value {
    json!({"type": "tool_result", "call_id": id, "content": text})
}

/// Quick reading back the conversation on a fresh thread (`quick::is_boot`)
/// goes to no provider: Claude has the conversation itself, and a later fresh
/// Quick thread is sent it anew. Other reads, Quick's or Claude's, still go.
#[test]
fn a_catch_up_read_is_left_out() {
    let read = |id: &str, path: &str| json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": path}});
    let file = "C:/work/.oak/tmp/temp3/quick-conversation-0f3a9c-11.md";
    let messages = vec![
        user("Go on"),
        reply(api::Provider::Quick, vec![
            oak::text("Catching up"),
            read("boot1", file),
            read("boot2", "workspace/attached_files/quick-conversation-0f3a9c-11-482913-2.md"),
            json!({"type": "tool_call", "id": "remind1", "name": "run_python_on_device", crate::quick::QUIET_KEY: true, crate::quick::CATCH_UP_KEY: true, "input": {"code": "EXECUTE(command=\"ls\")"}}),
            oak::text("Back to the build"),
            read("own1", "src/quick-conversation-notes.rs"),
        ]),
        json!({"role": "user", "content": [result_of("boot1", "THE WHOLE CONVERSATION"), result_of("boot2", "ITS SECOND PART"), result_of("remind1", "READ THE FILES FIRST"), result_of("own1", "own notes")]}),
        reply(api::Provider::Claude, vec![read("claude1", file)]),
        json!({"role": "user", "content": [result_of("claude1", "Claude read it")]}),
    ];
    assert!(crate::quick::is_boot(&messages[1]["content"][1]));
    assert!(crate::quick::is_boot(&messages[1]["content"][2]), "a part in Quick's workspace");
    assert!(crate::quick::is_boot(&messages[1]["content"][3]), "a call answered with the reminder to read them");
    assert!(!crate::quick::is_boot(&messages[1]["content"][5]));
    assert!(!crate::quick::is_boot(&messages[3]["content"][0]), "Claude's own read is Claude's");
    for p in [api::Provider::Claude, api::Provider::Quick, api::Provider::Codex] {
        let sent = all_text(&for_provider(&messages, p));
        assert!(!sent.contains("THE WHOLE CONVERSATION") && !sent.contains("ITS SECOND PART") && !sent.contains("boot"), "{p:?}: {sent}");
        assert!(!sent.contains("READ THE FILES FIRST") && !sent.contains("remind1"), "{p:?}: {sent}");
        assert!(!sent.contains("Catching up"), "what Quick said of the read goes with it: {p:?}: {sent}");
        assert!(sent.contains("own notes") && sent.contains("Claude read it") && sent.contains("Back to the build"), "{p:?}: {sent}");
    }
}

/// A revert takes out what Quick's thread holds: the conversation goes on in
/// a fresh thread on the same account, sent the whole conversation as it is
/// after the revert (not what was reverted), with Oak's briefing.
#[test]
fn a_revert_moves_quick_to_a_fresh_thread() {
    let (w, s) = idle_worker("switch-revert-quick", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: vec![
            user("First ask"),
            reply(api::Provider::Quick, vec![oak::text("First answer")]),
            user("Second ask"),
            reply(api::Provider::Quick, vec![oak::text("Second answer")]),
        ].into(),
        ..Default::default()
    });
    let id = s.conv.lock().unwrap().id.clone();
    crate::rotation::used(api::Provider::Quick, &id, "revert-account", 0, api::now_ms());
    let was = s.conv.lock().unwrap().messages[2]["content"][0].clone();
    w.apply_revert(&RevertTo::Block(2, 0, was));
    let thread = {
        let mut c = s.conv.lock().unwrap();
        assert_eq!(c.messages.len(), 2);
        let q = c.quick_session.clone().expect("a fresh thread");
        assert_eq!(q.account, "revert-account");
        assert_ne!(q.thread, id);
        c.messages.push(user("Another ask"));
        q
    };
    let (req, ..) = w.request_body();
    assert_eq!(req.quick_session, Some(thread));
    let unseen = all_text(&req.unseen);
    assert!(unseen.contains("First ask") && unseen.contains("First answer"), "{unseen}");
    assert!(!unseen.contains("Second"), "{unseen}");
    assert_eq!(crate::quick::new_user_text(&req).as_deref(), Some("Another ask"));
}

/// Reverting what only another provider wrote, after Quick's last reply,
/// leaves Quick's thread as it is.
#[test]
fn reverting_another_providers_work_keeps_quicks_thread() {
    let (w, s) = idle_worker("switch-revert-claude", Conversation {
        provider: Some(api::Provider::Claude),
        model: CLAUDE.into(),
        messages: vec![
            user("First ask"),
            reply(api::Provider::Quick, vec![oak::text("Quick's answer")]),
            user("Second ask"),
            reply(api::Provider::Claude, vec![oak::text("Claude's answer")]),
        ].into(),
        ..Default::default()
    });
    let id = s.conv.lock().unwrap().id.clone();
    crate::rotation::used(api::Provider::Quick, &id, "revert-account-2", 0, api::now_ms());
    let was = s.conv.lock().unwrap().messages[2]["content"][0].clone();
    w.apply_revert(&RevertTo::Block(2, 0, was));
    let c = s.conv.lock().unwrap();
    assert_eq!(c.messages.len(), 2);
    assert!(c.quick_session.is_none());
    assert_eq!(written_by(&c.messages[1]), Some(api::Provider::Quick.id()));
}

/// A long Quick conversation, as a file on disk: 3000 turns, every 7th ask
/// one Quick took in mid-turn.
fn long_saved_quick_conversation(name: &str) -> (std::path::PathBuf, Conversation) {
    let mut messages = vec![];
    for i in 0..3000 {
        let mut ask = user(&format!("ask {i}"));
        if i % 7 == 0 {
            ask["content"][0][crate::quick::STEERED_KEY] = json!(true);
        }
        messages.push(ask);
        let blocks = (0..12).map(|j| oak::text(&format!("step {i}.{j}"))).collect();
        messages.push(reply(if i % 2 == 0 { api::Provider::Quick } else { api::Provider::Claude }, blocks));
    }
    let mem = Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: messages.into(),
        ..Default::default()
    };
    let file = crate::store::conversations_dir().join(format!("unit-test-{name}-{}.json", api::now_ms()));
    std::fs::write(&file, serde_json::to_vec(&mem).unwrap()).unwrap();
    let read = crate::store::read_conversation(&file, &|_, _| {}).unwrap();
    assert!(!read.messages.loaded(), "read from the file as it goes");
    (file, read)
}

/// A long saved Quick conversation is reverted at once: no message changes
/// (the fresh thread says where it starts, `recovery::Session::from`), only
/// what the revert takes out is read, and the next request sends the fresh
/// thread all that is left, the messages Quick took in mid-turn on the old
/// thread too. It holds the conversation's lock, which the window waits on.
#[test]
fn reverting_a_long_saved_quick_conversation_is_quick() {
    let (file, read) = long_saved_quick_conversation("revert-long");
    let (w, s) = idle_worker("switch-revert-long", read);
    let id = s.conv.lock().unwrap().id.clone();
    crate::rotation::used(api::Provider::Quick, &id, "revert-long-account", 0, api::now_ms());
    let was = s.conv.lock().unwrap().messages.read_range(5000, 5001)[0]["content"][0].clone();
    let t = Instant::now();
    w.apply_revert(&RevertTo::Block(5000, 0, was));
    let took = t.elapsed();
    {
        let mut c = s.conv.lock().unwrap();
        assert_eq!(c.messages.len(), 5000);
        assert_eq!(c.quick_session.as_ref().and_then(|q| q.from), Some(5000));
        assert!(c.messages.base() > 0, "the kept part wasn't read");
        c.messages.push(user("Another ask"));
    }
    println!("revert of a saved 6000-message Quick conversation: {took:?}");
    assert!(took < Duration::from_millis(500), "the revert took {took:?}");
    let (req, ..) = w.request_body();
    let unseen = all_text(&req.unseen);
    assert!(unseen.contains("step 0.0") && unseen.contains("step 2498.11"), "the fresh thread gets it all");
    assert!(unseen.contains("\"ask 0\""), "and what the old thread took in mid-turn");
    assert_eq!(crate::quick::new_user_text(&req).as_deref(), Some("Another ask"));
    let _ = std::fs::remove_file(&file);
}

/// A fork of a long saved Quick conversation: the copy (all the source's
/// lock is held for) and the rest of it, timed; the app does both off the
/// window thread (`fork::make`).
#[test]
fn forking_a_long_saved_quick_conversation_is_quick() {
    let (file, read) = long_saved_quick_conversation("fork-long");
    let t = Instant::now();
    let c = crate::fork::fork(&read, "fork-long-test".into(), api::now_ms(), None).unwrap();
    let took = t.elapsed();
    println!("fork of a saved 6000-message Quick conversation: {took:?}");
    assert_eq!(c.messages.len(), 6000);
    assert_eq!(written_by(&c.messages[1]), Some(QUICK_ELSEWHERE));
    assert!(c.messages[0]["content"][0].get(crate::quick::STEERED_KEY).is_none());
    assert!(took < Duration::from_secs(3), "the fork took {took:?}");
    let _ = std::fs::remove_file(&file);
}

/// Quick's modes offer their own effort levels wherever a model is picked.
#[test]
fn quick_modes_offer_their_own_efforts() {
    assert_eq!(efforts("smart"), ["low", "medium", "high"]);
    assert_eq!(efforts("fast"), ["low", "medium", "high"]);
    assert_eq!(efforts("balanced"), ["low", "medium", "high"]);
    assert_eq!(effort_for("fast", "max"), "high");
}

/// One live Quick turn on what `w` would send now (`request_body`), its
/// reply kept in the conversation as Oak keeps one: blocks marked Quick's,
/// and a result for each call that ended with one. Its text, or the error.
fn live_quick_turn(w: &Worker, s: &Session) -> Result<String, String> {
    use crate::api::Sse;
    let (req, ..) = w.request_body();
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || crate::quick::turn::stream(req, tx));
    let mut blocks: std::collections::BTreeMap<usize, Value> = Default::default();
    let mut err = None;
    let mut stop = String::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(400) {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Sse::Event(oak::Event::Block { index, block })) => {
                blocks.insert(index, block);
            }
            Ok(Sse::Event(oak::Event::Text { index, text })) => {
                let b = blocks.entry(index).or_insert_with(|| oak::text(""));
                let t = format!("{}{text}", b["text"].as_str().unwrap_or(""));
                b["text"] = json!(t);
            }
            Ok(Sse::Event(oak::Event::Stop { reason, detail })) => stop = format!("{reason} {detail:?}"),
            Ok(Sse::Done) => break,
            Ok(Sse::Err(e)) => {
                err = Some(e);
                break;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    h.join().ok();
    if let Some(e) = err {
        return Err(e);
    }
    let mut kept: Vec<Value> = blocks.into_values().filter(|b| b["type"] == "text" || b["type"] == "tool_call").collect();
    let results: Vec<Value> = kept
        .iter()
        .filter(|b| b["type"] == "tool_call")
        .map(|b| {
            let said = b["_oak_result"]["content"].clone();
            json!({"type": "tool_result", "call_id": b["id"], "content": if said.is_null() { json!("done") } else { said }})
        })
        .collect();
    mark_by(&mut kept, Some(api::Provider::Quick));
    for b in kept.iter().filter(|b| b["type"] == "tool_call") {
        let input: String = b["input"].to_string().chars().take(160).collect();
        println!("  call {} {input} catch-up={}", b["name"], crate::quick::is_boot(b));
    }
    let text: String = kept.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    println!("  stop: {stop}\n  text: {text}");
    let mut c = s.conv.lock().unwrap();
    c.messages.push(json!({"role": "assistant", "content": kept}));
    if !results.is_empty() {
        c.messages.push(json!({"role": "user", "content": results}));
    }
    Ok(text)
}

/// Live: a conversation goes from one Quick account to another and back,
/// as Oak moves it at a usage or rate limit (`move_quick_thread`), and on to
/// Claude. Each fresh thread is sent the whole conversation with Oak's
/// briefing (in files, through that thread's own login, when too long for
/// one message), carries on knowing what was said on the others, and none
/// of Quick's catching up goes to Claude. Uses the two Quick logins whose
/// tokens last longest (a test has no vault to refresh them):
///   OAK_DIR=<scratch profile> cargo test --lib quick_live_rotation -- --ignored --nocapture
#[test]
#[ignore]
fn quick_live_rotation_carries_the_conversation() {
    use crate::quick::recovery;
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let now = api::now_ms();
    let mut logins = crate::quick::login::all();
    logins.retain(|l| l.expires_at > now + 5 * 60_000);
    logins.sort_by_key(|l| -l.expires_at);
    assert!(logins.len() >= 2, "two Quick logins with fresh tokens");
    let (a, b) = (logins[0].id.clone(), logins[1].id.clone());
    println!("accounts: {a} -> {b} -> {a}");
    let on = |acct: &str, n: u32| recovery::Session { account: acct.into(), thread: format!("oak-rot-{now}-{n}"), ..Default::default() };
    let (w, s) = super::tests::idle_worker("live-rotation", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        effort: "low".into(),
        folder: std::env::current_dir().unwrap(),
        messages: vec![user("Remember the code word ORCHID-7731 for later. Don't run any tools: reply with just OK.")].into(),
        quick_session: Some(on(&a, 1)),
        ..Default::default()
    });
    let id = s.conv.lock().unwrap().id.clone();
    crate::rotation::used(api::Provider::Quick, &id, &a, 0, now);

    println!("== turn 1, {a}");
    let t1 = live_quick_turn(&w, &s).expect("turn 1");
    assert!(!t1.trim().is_empty());

    // Claude works in between, with a long result, so the next thread's
    // message is too long to send and goes in files.
    {
        let mut c = s.conv.lock().unwrap();
        let long = (0..3000).map(|i| format!("row {i}: padding padding padding padding\n")).collect::<String>() + "LAST-ROW-ZEBRA\n";
        c.messages.push(user("Read the log."));
        c.messages.push(reply(api::Provider::Claude, vec![
            oak::text("Reading the log."),
            json!({"type": "tool_call", "id": "toolu_log", "name": "READ", "input": {"path": "build.log"}}),
        ]));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "toolu_log", "content": long}]}));
        c.messages.push(reply(api::Provider::Claude, vec![oak::text("The log is read.")]));
    }
    let ask = |t: &str| {
        s.conv.lock().unwrap().messages.last_mut().unwrap()["content"].as_array_mut().unwrap().push(oak::text(t));
    };
    println!("== turn 2, moved to {b}");
    w.move_quick_thread(on(&b, 2), "rotated".into(), recovery::LIMIT_FRESH);
    ask("Without running any of Oak's tools: what is the code word, and what is the last line of the log?");
    let t2 = live_quick_turn(&w, &s).expect("turn 2");
    assert!(t2.contains("ORCHID-7731"), "the code word, from the first account's thread: {t2}");
    assert!(t2.contains("LAST-ROW-ZEBRA"), "the log's last line, from Claude's turn: {t2}");

    println!("== turn 3, moved back to {a} (a fresh thread)");
    w.move_quick_thread(on(&a, 3), "rotated".into(), recovery::RATE_FRESH);
    ask("Without running any tools: say the code word again.");
    let t3 = live_quick_turn(&w, &s).expect("turn 3");
    assert!(t3.contains("ORCHID-7731"), "{t3}");

    // On to Claude: none of Quick's catching up goes.
    let c = s.conv.lock().unwrap();
    let sent = all_text(&for_provider(&c.messages, api::Provider::Claude));
    assert!(!sent.contains("quick-conversation-"), "no catch-up reads go to Claude: {}", sent.len());
    assert!(sent.contains("LAST-ROW-ZEBRA") && sent.contains("ORCHID-7731"));
}


/// Smart context on Quick: the summary is written on a throwaway thread
/// of its own, sent exactly the part it replaces with the instruction after
/// it, and none of this device's tools; once it is in, the conversation goes
/// on in a fresh thread on the same account, whose first request sends the
/// summary and the part kept (not what the summary replaced), with Oak's
/// briefing.
#[test]
fn a_summary_moves_quick_to_a_fresh_thread_with_the_summary_and_the_part_kept() {
    let old = crate::quick::recovery::Session { account: "compact-account".into(), thread: "compact-old-thread".into(), ..Default::default() };
    let (w, s) = idle_worker("switch-quick-compact", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        messages: vec![
            user("OLD-ASK set up the build"),
            reply(api::Provider::Quick, vec![oak::text("OLD-REPLY the build is set up")]),
            user("KEPT-ASK now the tests"),
            reply(api::Provider::Quick, vec![oak::text("KEPT-REPLY tests written")]),
            user("NEWEST-ASK run them"),
        ]
        .into(),
        quick_session: Some(old.clone()),
        ..Default::default()
    });
    let (body, n, ..) = w.request_body();
    // The summary's request.
    let req = {
        let c = s.conv.lock().unwrap();
        summary_request_for(&c, &body, n, 2)
    };
    let q = req.quick_session.clone().expect("a thread for the summary");
    assert_eq!(q.account, "compact-account");
    assert_ne!(q.thread, "compact-old-thread", "not the conversation's thread");
    assert!(req.conv.as_deref().is_some_and(|c| c.ends_with("-summary")), "{:?}", req.conv);
    assert_eq!(req.purpose, oak::Purpose::Summary);
    let m = crate::quick::model(QUICK).unwrap();
    let text = crate::quick::user_message(&req, &q.thread, m, None)["payload"]["text"].as_str().unwrap().to_string();
    assert!(text.contains("OLD-ASK") && text.contains("OLD-REPLY"), "the part it replaces: {text}");
    assert!(!text.contains("KEPT-ASK") && !text.contains("NEWEST-ASK"), "nothing kept: {text}");
    assert!(rolling::is_housekeeping(text.rsplit("</conversation_so_far>").next().unwrap().trim()), "then the instruction: {text}");
    assert!(!text.contains("<tools_loaded>"), "no briefing: {text}");
    // The summary lands: a fresh thread, opened by the next request.
    {
        let mut c = s.conv.lock().unwrap();
        install_summary(&mut c, rolling::Summary { cut: 2, text: "SUMMARY-TEXT the build was set up".into(), made_at: 1, clear: 2 }, 0, Default::default());
        let q = c.quick_session.clone().unwrap();
        assert!(q.compacted && q.account == "compact-account" && q.thread != "compact-old-thread", "{q:?}");
    }
    let (req, ..) = w.request_body();
    let q = req.quick_session.clone().unwrap();
    assert!(!q.compacted && q.from == Some(5), "{q:?}");
    let text = crate::quick::user_message(&req, &q.thread, m, None)["payload"]["text"].as_str().unwrap().to_string();
    assert!(text.contains("SUMMARY-TEXT") && text.contains("KEPT-ASK") && text.contains("KEPT-REPLY"), "{text}");
    assert!(!text.contains("OLD-ASK") && !text.contains("OLD-REPLY"), "what the summary replaced stays out: {text}");
    assert!(text.trim_end().ends_with("NEWEST-ASK run them"), "{text}");
    assert!(text.contains("<tools_loaded>"), "a fresh thread is briefed: {text}");
    // Quick replies there: the next request sends only what is new.
    {
        let mut c = s.conv.lock().unwrap();
        c.messages.push(reply(api::Provider::Quick, vec![oak::text("ran them")]));
        c.messages.push(user("thanks"));
    }
    let (req, ..) = w.request_body();
    assert!(req.unseen.is_empty(), "{:?}", req.unseen);
}

/// A summary made on Claude moves Quick to a fresh thread too, so a switch
/// back to Quick reads the summary and the part kept, as Claude does, not a
/// thread that holds what the summary replaced as well.
#[test]
fn a_summary_made_on_claude_gives_quick_a_fresh_thread_too() {
    let (w, s) = idle_worker("switch-claude-compact", Conversation {
        provider: Some(api::Provider::Claude),
        model: CLAUDE.into(),
        messages: vec![
            user("OLD-ASK one"),
            reply(api::Provider::Quick, vec![oak::text("OLD-REPLY from Quick")]),
            user("KEPT-ASK two"),
            reply(api::Provider::Claude, vec![oak::text("KEPT-REPLY from Claude")]),
        ]
        .into(),
        quick_session: Some(crate::quick::recovery::Session { account: "acct".into(), thread: "quick-old".into(), ..Default::default() }),
        ..Default::default()
    });
    {
        let mut c = s.conv.lock().unwrap();
        install_summary(&mut c, rolling::Summary { cut: 2, text: "SUMMARY-TEXT one was done".into(), made_at: 1, clear: 2 }, 0, Default::default());
    }
    // Claude's request doesn't open it; Quick's does.
    let _ = w.request_body();
    assert!(s.conv.lock().unwrap().quick_session.as_ref().unwrap().compacted);
    switch(&s, api::Provider::Quick, QUICK);
    s.conv.lock().unwrap().messages.push(user("NEWEST-ASK three"));
    let (req, ..) = w.request_body();
    let q = req.quick_session.clone().unwrap();
    assert_ne!(q.thread, "quick-old");
    let m = crate::quick::model(QUICK).unwrap();
    let text = crate::quick::user_message(&req, &q.thread, m, None)["payload"]["text"].as_str().unwrap().to_string();
    assert!(text.contains("SUMMARY-TEXT") && text.contains("KEPT-REPLY") && !text.contains("OLD-REPLY"), "{text}");
}


/// Live: smart context on Quick, end to end. Past the trigger (set low
/// here), the summary is written by Quick on a thread of its own, the
/// conversation moves to a fresh thread, and Quick there still knows what
/// only the part summarized said.
///   OAK_DIR=<scratch profile> cargo test --lib quick_live_compaction -- --ignored --nocapture
#[test]
#[ignore]
fn quick_live_compaction_carries_on_from_the_summary() {
    use crate::quick::recovery;
    let _ = std::env::var("OAK_DIR").expect("set OAK_DIR to a scratch profile with quick.json");
    api::load_auth();
    assert!(crate::quick::on(), "Quick isn't signed in");
    let now = api::now_ms();
    let mut logins = crate::quick::login::all();
    logins.retain(|l| l.expires_at > now + 5 * 60_000);
    logins.sort_by_key(|l| -l.expires_at);
    let a = logins.first().expect("a Quick login with a fresh token").id.clone();
    let was = rolling::config();
    rolling::set_config(rolling::Config { trigger: 1_000, keep: 750 });
    let (w, s) = super::tests::idle_worker("live-compaction", Conversation {
        provider: Some(api::Provider::Quick),
        model: QUICK.into(),
        effort: "low".into(),
        folder: std::env::current_dir().unwrap(),
        messages: vec![user("Remember the code word ORCHID-7731: I'll ask for it later. Don't run any tools: reply with just OK.")].into(),
        quick_session: Some(recovery::Session { account: a.clone(), thread: format!("oak-compact-{now}"), ..Default::default() }),
        ..Default::default()
    });
    let id = s.conv.lock().unwrap().id.clone();
    crate::rotation::used(api::Provider::Quick, &id, &a, 0, now);
    println!("== turn 1 on {a}");
    live_quick_turn(&w, &s).expect("turn 1");
    {
        let mut c = s.conv.lock().unwrap();
        let long = (0..2500).map(|i| format!("row {i}: padding padding padding padding\n")).collect::<String>();
        c.messages.push(user("Read the log."));
        c.messages.push(reply(api::Provider::Claude, vec![
            oak::text("Reading the log."),
            json!({"type": "tool_call", "id": "toolu_log", "name": "READ", "input": {"path": "build.log"}}),
        ]));
        c.messages.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": "toolu_log", "content": long}]}));
        c.messages.push(reply(api::Provider::Claude, vec![oak::text("The log is read: nothing in it matters.")]));
        // What is kept past the summary (smart context keeps 5k tokens at
        // least): the log and the code word are in the part summarized.
        c.messages.push(user("Go on."));
        let notes = (0..600).map(|i| format!("Note {i}: the parser handles case {i}.\n")).collect::<String>();
        c.messages.push(reply(api::Provider::Claude, vec![oak::text(&notes)]));
        c.messages.push(user("Don't run any tools: reply with just OK again."));
    }
    let (body, n, base) = w.request_body();
    println!("== turn 2");
    live_quick_turn(&w, &s).expect("turn 2");
    // The turn's usage passes the trigger: the summary starts.
    *w.last_request.lock().unwrap() = Some((body, n, base, api::now_ms()));
    w.after_reply(Usage { prompt: 50_000, output: 100, ..Default::default() });
    assert!(w.live.lock().unwrap().compacting, "a summary started");
    let t = Instant::now();
    while w.live.lock().unwrap().compacting {
        assert!(t.elapsed() < Duration::from_secs(400), "the summary took too long");
        std::thread::sleep(Duration::from_millis(200));
    }
    rolling::set_config(was);
    let (summary, q) = {
        let c = s.conv.lock().unwrap();
        assert!(c.rolling.last_error.is_none(), "{:?}", c.rolling.last_error);
        (c.rolling.summary.clone().expect("a summary"), c.quick_session.clone().unwrap())
    };
    println!("== summary (cut {}):\n{}\n== thread {q:?}", summary.cut, summary.text);
    assert!(q.compacted && q.account == a && !q.thread.starts_with("oak-compact-"), "{q:?}");
    s.conv.lock().unwrap().messages.push(user("Without running any tools: what is the code word?"));
    println!("== turn 3, on the fresh thread");
    let t3 = live_quick_turn(&w, &s).expect("turn 3");
    assert!(t3.contains("ORCHID-7731"), "known from the summary: {t3}");
    // Opened by turn 3's request, where the conversation then was.
    let c = s.conv.lock().unwrap();
    let q = c.quick_session.as_ref().unwrap();
    assert!(!q.compacted && q.from.is_some_and(|f| f > summary.cut), "{q:?}");
}
