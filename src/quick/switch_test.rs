//! Switching a conversation into AWS Quick, out of it and back, and between
//! Quick's modes: what each request carries over to Quick's thread, and
//! what the thread keeps.

use super::*;

fn request(conv: &str, messages: Vec<Value>) -> Request {
    Request {
        provider: crate::providers::Kind::Quick,
        model: "smart".into(),
        conv: Some(conv.into()),
        messages,
        ..Default::default()
    }
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

fn text_of(v: &Value) -> String {
    v["payload"]["text"].as_str().unwrap_or("").to_string()
}

/// A conversation keeps its effort across a switch: Claude's `xhigh` or
/// thinking off, or Smart's `max` taken to Fast. Each goes to the mode's
/// nearest level below, never above (off went as the mode's highest, and
/// `xhigh` as `max`).
#[test]
fn an_effort_carried_across_a_switch_fits_the_mode() {
    let smart = model("smart").unwrap();
    let fast = model("fast").unwrap();
    let mut r = request("switch-effort", vec![]);
    let at = |r: &mut Request, e: Option<&str>, m: &Model| {
        r.effort = e.map(String::from);
        effort(r, m)
    };
    assert_eq!(at(&mut r, Some("xhigh"), smart), "xhigh");
    assert_eq!(at(&mut r, Some("max"), smart), "max");
    assert_eq!(at(&mut r, Some("ultra"), smart), "max");
    assert_eq!(at(&mut r, Some("high"), smart), "high");
    assert_eq!(at(&mut r, Some("off"), smart), "low");
    assert_eq!(at(&mut r, Some("none"), smart), "low");
    assert_eq!(at(&mut r, Some("something new"), smart), "medium");
    assert_eq!(at(&mut r, None, smart), "medium");
    assert_eq!(at(&mut r, Some("max"), fast), "high");
    assert_eq!(at(&mut r, Some("xhigh"), fast), "high");
    assert_eq!(at(&mut r, Some("off"), fast), "low");
    assert_eq!(at(&mut r, Some("low"), fast), "low");
    r.purpose = oak::Purpose::Title;
    assert_eq!(at(&mut r, Some("max"), fast), "off");
}

/// Switching between Quick's modes is the same thread: the next message
/// names the new mode and carries no briefing and nothing of the
/// conversation again.
#[test]
fn a_mode_switch_stays_on_the_thread_and_sends_nothing_again() {
    let thread = "switch-mode-thread";
    let mut r = request(thread, vec![user("first"), assistant("done"), user("now faster")]);
    let first = user_message(&r, thread, model("smart").unwrap(), None);
    assert!(text_of(&first).contains("<tools_loaded>"), "a thread's first message is briefed");
    r.model = "fast".into();
    let next = user_message(&r, thread, model("fast").unwrap(), None);
    assert_eq!(next["payload"]["agent_mode"], "fast");
    assert_eq!(next["payload"]["thread_id"], thread);
    assert_eq!(text_of(&next), "now faster");
}

/// A conversation Claude worked on, moved to Quick in the middle of
/// Claude's turn (its newest user turn only a result): Quick is sent all of
/// it, the user's message once, and told to carry on from there, not that
/// its connection was cut.
#[test]
fn a_switch_into_quick_mid_turn_carries_the_conversation_once() {
    let thread = "switch-into-quick";
    let messages = vec![
        user("fix the flaky test"),
        json!({"role": "assistant", "content": [
            {"type": "reasoning", "text": "Claude's thinking", "signature": "sig", "by": "anthropic"},
            {"type": "text", "text": "Reading it."},
            {"type": "tool_call", "id": "toolu_1", "name": "READ", "input": {"path": "tests/a.rs"}}
        ]}),
        json!({"role": "user", "content": [{"type": "tool_result", "call_id": "toolu_1", "content": "fn flaky() {}"}]}),
    ];
    let mut r = request(thread, messages);
    r.unseen = all_unseen(&r.messages);
    let text = text_of(&user_message(&r, thread, model("smart").unwrap(), None));
    let so_far = text.split("<conversation_so_far>").nth(1).expect("the conversation goes");
    assert_eq!(text.matches("fix the flaky test").count(), 1, "{text}");
    assert!(so_far.contains("User: fix the flaky test"));
    assert!(so_far.contains("Assistant: Reading it."));
    assert!(so_far.contains("Assistant called READ: {\"path\":\"tests/a.rs\"}"));
    assert!(so_far.contains("Result of READ: fn flaky() {}"));
    assert!(!so_far.contains("Claude's thinking"), "another provider's reasoning stays out");
    assert!(text.trim_end().ends_with(CARRY_ON_SO_FAR));
    assert!(!text.contains(CARRY_ON));
}

/// Quick asked with a decision card, the user answered, and another model
/// went on from the answers before the conversation came back to Quick:
/// Quick's thread never had them, so they go as what the user said. An
/// attached text file's words go too; the newest turn's answers and files
/// go with the message itself, not twice.
#[test]
fn the_conversation_so_far_carries_card_answers_and_text_files() {
    let answers = "1. Which approach?\nAnswer: The small fix";
    let unseen = vec![
        json!({"role": "user", "content": [{"type": "tool_result", "call_id": format!("{ASK_CALL}m1"), "content": answers}]}),
        assistant("Doing the small fix."),
        json!({"role": "user", "content": [
            {"type": "document", "media_type": "text/plain", "title": "notes.txt", "data": "keep the API"}
        ]}),
        assistant("Noted."),
        json!({"role": "user", "content": [
            {"type": "tool_result", "call_id": format!("{ASK_CALL}m2"), "content": "1. Again?\nAnswer: newest"},
            {"type": "document", "media_type": "text/plain", "title": "new.txt", "data": "NEWEST FILE"}
        ]}),
    ];
    let (body, cut) = so_far_body(&unseen, SO_FAR_PART, SO_FAR_CHARS).unwrap();
    assert!(!cut);
    assert!(
        body.contains(&format!("User: {}", markup::decision_response(answers))),
        "{body}"
    );
    assert!(body.contains("Which approach?: The small fix"));
    assert!(!body.contains("Result of the call"));
    assert!(body.contains("[a document: notes.txt]\nkeep the API"));
    assert!(!body.contains("newest") && !body.contains("NEWEST FILE"), "{body}");
}

/// A thread moved to another login has none of the conversation, nor
/// Oak's briefing: each message is briefed and sent it all until one gets
/// there (the move is seen once; a first send that failed sent the next
/// request's newest turn alone, unbriefed).
#[test]
fn a_login_move_is_briefed_until_a_message_gets_there() {
    let conv = "switch-moved-conv";
    let r = request(
        conv,
        vec![
            user("start"),
            json!({"role": "assistant", "content": [
                {"type": "tool_call", "id": "c1", "name": "run_python_outpost_local", "input": {"code": "READ(path=\"a\")"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "call_id": "c1", "content": "a"}]}),
            assistant("read it"),
            user("go on"),
        ],
    );
    assert!(oak_preface(&r, conv).is_none(), "Quick worked on this thread: briefed already");
    moved_out(conv);
    assert!(moving(conv));
    assert!(oak_preface(&r, conv).is_some());
    assert!(oak_preface(&r, conv).is_some(), "again, while no message got there");
    // A fresh thread of its own (`recovery`) is briefed once, as before.
    let mut fresh = r.clone();
    fresh.quick_session = Some(recovery::Session { account: "a".into(), thread: "switch-moved-fresh".into() });
    assert!(oak_preface(&fresh, "switch-moved-fresh").is_some());
    assert!(oak_preface(&fresh, "switch-moved-fresh").is_none());
    moved_in(conv);
    assert!(!moving(conv));
    assert!(oak_preface(&r, conv).is_none());
}

/// A fork of a conversation Quick worked on goes on in a new thread: after
/// Oak restarts (the move it made is forgotten), its first message is still
/// briefed, since the snippets it holds are all in what the new thread
/// lacks. One Quick ran on this thread since keeps it unbriefed.
#[test]
fn a_fork_after_a_restart_is_briefed() {
    let snippet = |id: &str| {
        json!({"role": "assistant", "content": [
            {"type": "tool_call", "id": id, "name": "run_python_outpost_local", "input": {"code": "READ(path=\"a\")"}}
        ]})
    };
    let result = |id: &str| json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": "a"}]});
    let mut r = request(
        "switch-fork-restarted",
        vec![user("start"), snippet("s1"), result("s1"), assistant("read it"), user("in the fork")],
    );
    r.unseen = all_unseen(&r.messages);
    assert!(!quick_worked_here(&r));
    assert!(oak_preface(&r, "switch-fork-restarted").is_some());
    // Quick then worked on the fork's own thread; Claude went on after.
    r.messages.extend([snippet("s2"), result("s2"), assistant("Claude's turn"), user("back")]);
    r.unseen = vec![assistant("Claude's turn")];
    assert!(quick_worked_here(&r));
    // Quick's own conversation, no switch: worked here.
    r.unseen.clear();
    assert!(quick_worked_here(&r));
}

/// A turn parked after Quick took in a message is read on only while no
/// other model worked on the conversation since: after a switch away and
/// back it is stale, and its thread lacks what the others did.
#[test]
fn a_parked_turn_is_not_read_on_after_another_model_worked() {
    let mut r = request("switch-parked", vec![user("hi")]);
    assert!(turn::reads_on_parked(&r));
    r.unseen = vec![assistant("Claude's reply")];
    assert!(!turn::reads_on_parked(&r));
}

/// Files the user attached as the conversation moved to Quick reach it:
/// Quick's message takes text only, so a document went nowhere before.
/// One that can't go to Quick's workspace goes in the message, when text.
#[test]
fn attached_files_reach_quick() {
    let r = request(
        "switch-files",
        vec![
            user("old"),
            json!({"role": "user", "content": [
                {"type": "document", "media_type": "text/plain", "title": "old.txt", "data": "OLD"}
            ]}),
            assistant("reply"),
            json!({"role": "user", "content": [
                {"type": "text", "text": "look at these"},
                {"type": "document", "media_type": "text/plain", "title": "a.txt", "data": "hello there"},
                {"type": "document", "media_type": "application/pdf", "title": "b.pdf", "data": "JVBERi0="}
            ]}),
        ],
    );
    // (No such thread: the upload is refused before it goes anywhere.)
    let lines = turn::attached_images(&r, "no thread!");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("a.txt") && lines[0].contains("hello there"), "{}", lines[0]);
    assert!(lines[1].contains("b.pdf") && lines[1].contains("couldn't put it"), "{}", lines[1]);
    assert!(lines.iter().all(|l| !l.contains("OLD")));
    assert_eq!(document_text(&r.messages[3]["content"][1]).as_deref(), Some("hello there"));
    assert_eq!(document_text(&r.messages[3]["content"][2]), None);
}
