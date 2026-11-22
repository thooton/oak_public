//! Forks: a conversation copied into a new one of its own, with its own tab
//! and Home row. It carries the durable context with it (the transcript, the
//! summary in force, notes, the prefix as sent and the session memory), but
//! no running work: no tasks, subagents, goal, heartbeats, meters, inbox,
//! seat or usage totals. Forking from a message copies the conversation up
//! to just before it (what Revert to before this goes back to), to take it
//! another way; the source keeps its own transcript, and the fork waits for
//! the user.
//!
//! The fork has a scratchpad of its own, and its prefix's pin is let go, so
//! its first request renders the system prompt for it (naming its own
//! scratchpad) rather than sending the one the source sent. Files in the
//! working folder are shared, as a side chat's are; the pictures in its
//! transcript are the same blobs, which are never deleted. Nothing links a
//! fork to its source: deleting either leaves the other.

use super::*;

/// A copy of `source` as conversation `id`, forking from just before `at`
/// (`None`: at its end). `None` when `at` is no longer there: the source
/// changed since the user asked.
pub(crate) fn fork(source: &Conversation, id: String, now: i64, at: Option<agent::RevertTo>) -> Option<Conversation> {
    let mut c = Conversation {
        id,
        title: fork_title(source),
        titled: true,
        folder: source.folder.clone(),
        provider: source.provider,
        model: source.model.clone(),
        effort: source.effort.clone(),
        role: source.role.clone(),
        chat: source.chat,
        code_mode: source.code_mode,
        messages: source.messages.clone(),
        format: source.format,
        rolling: source.rolling.clone(),
        prefix: source.prefix.clone(),
        memory: source.memory.clone(),
        earlier: source.earlier.clone(),
        notes: source.notes.clone(),
        next_note_id: source.next_note_id,
        marks: source.marks.clone(),
        last_message_at: now,
        ..Default::default()
    };
    // (Its system prompt is rendered again, with its own scratchpad.)
    c.prefix.pin = None;
    if let Some(to) = at {
        let cut = match &to {
            agent::RevertTo::Block(msg, block, was) => agent::revert_to(&mut c, *msg, *block, was),
            agent::RevertTo::End(n) => agent::revert_to_end(&mut c, *n),
        };
        if !cut {
            return None;
        }
    }
    // A source stopped mid-turn leaves calls without results, which the API
    // refuses.
    agent::answer_lost_calls(&mut c);
    Some(c)
}

/// "Fork: <the source's title>", one Fork for a fork of a fork.
fn fork_title(source: &Conversation) -> String {
    let base = source.title.trim();
    let base = if base.is_empty() { "Untitled" } else { base };
    if base.starts_with("Fork: ") { base.to_string() } else { format!("Fork: {base}") }
}

impl App {
    /// Forks conversation `source` and shows the fork: from just before
    /// `at`, or at its end. What the source is doing carries on there alone.
    /// Gives the fork's id, or None when the source changed since.
    pub(crate) fn fork_conversation(&mut self, source: &str, at: Option<agent::RevertTo>, ctx: &egui::Context) -> Option<String> {
        let now = api::now_ms();
        let id = format!("fork-{now}-{:016x}", rand::random::<u64>());
        let c = fork(&self.sessions.get(source)?.conv.lock().unwrap(), id.clone(), now, at)?;
        if c.code_mode {
            // Its REPL starts with the source's variables.
            crate::code_mode::copy_snapshot(source, &id);
        }
        c.save();
        self.sessions.insert(id.clone(), Session::start(c, ctx.clone()));
        self.open(id.clone());
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> Value {
        json!({"type": "tool_call", "id": id, "name": "READ", "input": {"path": "x"}})
    }

    fn source() -> Conversation {
        Conversation {
            id: "fork-source-conv".into(),
            title: "Original".into(),
            folder: "/tmp/project".into(),
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            role: "main".into(),
            messages: vec![
                json!({"role": "user", "content": [oak::text("Build the project")]}),
                json!({"role": "assistant", "content": [call("running")]}),
                json!({"role": "user", "content": [oak::text("Carry on")]}),
                json!({"role": "assistant", "content": [oak::text("Done.")]}),
            ].into(),
            notes: vec![store::Note { id: 7, text: "Keep this context".into(), rewrite: None }],
            next_note_id: 8,
            rolling: rolling::State { summary: Some(rolling::Summary { cut: 1, text: "Earlier context".into(), made_at: 123, clear: 1 }), ..Default::default() },
            prefix: prefix::State {
                kits: vec!["AGENT".into()],
                pin: Some(prefix::Pin { system: "you are Oak, pads at temp3".into(), ..Default::default() }),
                ..Default::default()
            },
            heartbeats: vec![store::Heartbeat { id: 1, message: "Continue".into(), every_min: 1, next_at: 0 }],
            agents: vec![store::AgentRef { id: 1, conv: "fork-source-agent".into(), name: "Worker".into() }],
            goal: "Keep working".into(),
            inbox: vec!["Agent report".into()],
            held: vec!["Pending change".into()],
            parent: Some("agent-parent".into()),
            spawner: true,
            scratch: "temp7".into(),
            next_task_id: 42,
            limit_wait: Some(123),
            paused: true,
            spent: 9.0,
            tokens: store::Tokens { requests: 3, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn a_fork_keeps_the_context_but_not_the_work() {
        let source = source();
        let before = serde_json::to_value(&source).unwrap();
        let mut c = fork(&source, "fork-test".into(), 456, None).expect("a fork");
        assert_eq!(serde_json::to_value(&source).unwrap(), before, "the source is untouched");
        assert_eq!(c.messages.len(), source.messages.len());
        assert_eq!(c.messages[0], source.messages[0]);
        assert_eq!(c.title, "Fork: Original");
        assert!(c.titled);
        assert_eq!((&c.folder, &c.model, &c.effort, &c.role), (&source.folder, &source.model, &source.effort, &source.role));
        assert_eq!(c.notes, source.notes);
        assert_eq!(c.next_note_id, 8);
        assert_eq!(c.marks, source.marks);
        assert_eq!(serde_json::to_value(&c.rolling).unwrap(), serde_json::to_value(&source.rolling).unwrap());
        assert_eq!(c.prefix.kits, source.prefix.kits, "the loaded kits stay loaded");
        assert!(c.parent.is_none() && !c.spawner && c.agents.is_empty());
        assert!(c.goal.is_empty() && c.heartbeats.is_empty() && c.meters.is_empty());
        assert!(c.inbox.is_empty() && c.held.is_empty() && c.limit_wait.is_none());
        assert!(!c.paused && c.next_task_id == 0);
        assert!(c.scratch.is_empty(), "a scratchpad of its own");
        assert!(c.prefix.pin.is_none(), "its prompt is rendered for it");
        assert!(c.side_chat_of.is_none(), "a conversation in its own right");
        assert_eq!(c.tokens, store::Tokens::default());
        assert_eq!(c.spent, 0.0);
        // A fork of a fork keeps one Fork.
        c.title = fork_title(&c);
        assert_eq!(c.title, "Fork: Original");
        // It saves and loads as a conversation of its own.
        let loaded: Conversation = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        assert_eq!(loaded.messages, c.messages);
        assert!(loaded.side_chat_of.is_none());
    }

    #[test]
    fn forking_from_an_item_keeps_what_came_before_it() {
        let source = source();
        // Before the reply's only block: the reply goes, as Revert to before
        // this leaves it.
        let c = fork(&source, "fork-at-call".into(), 1, Some(agent::RevertTo::Block(1, 0, call("running")))).expect("a fork");
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0], source.messages[0]);
        // Before a call of a reply with text and calls: the text stays, and
        // the calls up to the cut keep their results; the transcript stays
        // valid for the model.
        let mut with_text = source.clone();
        with_text.messages[1] = json!({"role": "assistant", "content": [oak::text("Looking."), call("running")]});
        let c = fork(&with_text, "fork-at-later-call".into(), 1, Some(agent::RevertTo::Block(1, 1, call("running")))).expect("a fork");
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[1]["content"][0]["text"], "Looking.");
        assert!(!c.messages[1]["content"].as_array().unwrap().iter().any(|b| b["type"] == "tool_call"));
        // Before the user's last message: it and the reply after it go. The
        // reply before it left a call without a result, which the fork
        // answers so its transcript is valid for the model.
        let c = fork(&source, "fork-at-message".into(), 1, Some(agent::RevertTo::Block(2, 0, source.messages[2]["content"][0].clone()))).expect("a fork");
        assert_eq!(c.messages.len(), 3);
        assert_eq!(c.messages[2]["content"][0]["type"], "tool_result");
        assert!(!c.messages.iter().any(|m| m["content"][0]["text"] == "Carry on"), "the message and what followed are gone");
        // Before a reply that is still streaming in: everything there is.
        let c = fork(&source, "fork-at-end".into(), 1, Some(agent::RevertTo::End(4))).expect("a fork");
        assert_eq!(c.messages.len(), 4);
        // Something the source no longer has: no fork at all.
        let other = json!({"type": "text", "text": "not that"});
        assert!(fork(&source, "fork-gone".into(), 1, Some(agent::RevertTo::Block(2, 0, other))).is_none());
    }

    #[test]
    fn forking_makes_a_new_conversation_of_its_own() {
        use crate::app_hover_test::test_ctx;
        let (ctx, _t) = test_ctx(false, true);
        let mut app = App { screen: Screen::Chat("fork-source-conv".into()), ..Default::default() };
        app.sessions.insert("fork-source-conv".into(), Session::start(source(), ctx.clone()));
        let id = app.fork_conversation("fork-source-conv", None, &ctx).expect("a fork");
        assert_ne!(id, "fork-source-conv");
        assert!(app.sessions.contains_key(&id));
        assert!(matches!(&app.screen, Screen::Chat(shown) if shown == &id), "the fork is shown");
        assert!(app.settings.tabs.contains(&id));
        assert!(!app.sessions[&id].active(), "it waits for the user");
        let (len, origin_len) = {
            let forked = app.sessions[&id].conv.lock().unwrap();
            let origin = app.sessions["fork-source-conv"].conv.lock().unwrap();
            assert_eq!(&forked.messages, &origin.messages);
            (forked.messages.len(), origin.messages.len())
        };
        assert_eq!((len, origin_len), (4, 4), "the source keeps its transcript");
        let saved = store::load_conversations().into_iter().any(|c| c.id == id);
        assert!(saved, "a fork is an ordinary saved conversation");
        app.delete_conversation(&id);
        assert!(app.sessions.contains_key("fork-source-conv"), "deleting the fork leaves the source");
        app.delete_conversation("fork-source-conv");
    }
}
