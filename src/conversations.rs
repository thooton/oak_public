//! Conversations as the window handles them: deleting one, reverting it,
//! switching drafts between tabs, and editing its queued messages.

use super::*;

/// A queued message being edited in place (it is held in the queue meanwhile).
pub(crate) struct Editing {
    pub(crate) conv: String,
    pub(crate) id: u64,
    pub(crate) text: String,
    pub(crate) focus: bool,
}

pub(crate) enum QueueAction {
    Remove(u64),
    Edit(u64, String),
    Save,
    Cancel,
}

impl App {
    /// Deletes conversation `id` ("Delete conversation" on Home): it stops,
    /// its background tasks are killed, and its transcript file is removed.
    /// A conversation's subagents belong to it, so they go too (and their
    /// files). Their tabs close.
    pub(crate) fn delete_conversation(&mut self, id: &str) {
        let children: Vec<String> = match self.sessions.get(id) {
            Some(s) => s.conv.lock().unwrap().agents.iter().map(|a| a.conv.clone()).collect(),
            None => vec![],
        };
        let sides: Vec<String> = self.sessions.iter().filter(|(_, s)|
            s.conv.lock().unwrap().side_chat_of.as_deref() == Some(id)).map(|(key, _)| key.clone()).collect();
        for child in children.into_iter().chain(sides) {
            self.delete_conversation(&child);
        }
        self.delete_transcript(id);
    }

    /// Stops and forgets one conversation, removing its file (and its
    /// browser's profile).
    pub(crate) fn delete_transcript(&mut self, id: &str) {
        browser::forget(id);
        match self.sessions.get(id) {
            Some(s) => s.delete(),
            None => store::remove_conversation(id),
        }
        self.forget(id);
    }

    /// Drops a conversation that was deleted (AGENT_DELETE): its tab
    /// closes, and when it was shown, the session that started it is
    /// shown instead (or Home). What was being written to it goes too.
    pub(crate) fn forget(&mut self, id: &str) {
        let Some(s) = self.sessions.remove(id) else { return };
        let parent = s.conv.lock().unwrap().parent.clone();
        if self.side_chat.as_deref() == Some(id) {
            self.close_side_chat();
            self.side_hint = None;
            self.side_editing = None;
        }
        self.lists.remove(id);
        self.drafts.remove(id);
        self.last_sent.remove(id);
        if self.editing.as_ref().is_some_and(|e| e.conv == id) {
            self.editing = None;
        }
        if self.goal_edit.as_ref().is_some_and(|g| g.0 == id) {
            self.goal_edit = None;
        }
        if self.settings.tabs.iter().any(|t| t == id) {
            self.settings.tabs.retain(|t| t != id);
            store::save_settings(&self.settings);
        }
        if matches!(&self.screen, Screen::Chat(shown) if shown == id) {
            // Home or Settings stays up, but the screen must not point at
            // the conversation that is gone.
            self.screen = Screen::New;
            match parent.filter(|p| self.page == Page::Chats && self.sessions.contains_key(p)) {
                Some(p) => self.open(p),
                None if self.page == Page::Chats => self.page = Page::Home,
                None => {}
            }
        }
        if self.draft_key == id {
            self.input.clear();
            self.attachments.clear();
            self.draft_key = String::new();
        }
    }

    /// Reverts conversation `id` to before the user's message starting at
    /// block `block` of message `msg` (`first` is that block): the message
    /// goes back in the message box, its attachments with it, ahead of
    /// anything being written there. Files aren't touched.
    pub(crate) fn revert(&mut self, ctx: &egui::Context, id: &str, msg: usize, block: usize, first: Value) {
        let Some(s) = self.sessions.get(id) else { return };
        let (text, attachments) = {
            let c = s.conv.lock().unwrap();
            let content = c.messages.get(msg).and_then(|m| m["content"].as_array()).map(Vec::as_slice).unwrap_or_default();
            if content.get(block) != Some(&first) {
                return; // the conversation changed meanwhile
            }
            let range = user_messages(content).into_iter().find(|r| r.start == block).unwrap_or(block..block + 1);
            let (mut text, mut attachments) = (String::new(), vec![]);
            for b in &content[range] {
                match b["text"].as_str() {
                    Some(t) if b["type"] == "text" && b.get("_oak_file").is_none() => text = t.to_string(),
                    _ => attachments.push(b.clone()),
                }
            }
            (text, attachments)
        };
        s.revert(msg, block, first);
        if self.draft_key == id {
            if !text.is_empty() {
                self.input = if self.input.trim().is_empty() { text } else { format!("{text}\n\n{}", self.input) };
            }
            self.attachments.splice(0..0, attachments);
            let composer = if s.conv.lock().unwrap().side_chat_of.is_some() { Id::unique(("side-composer", id)) } else { Id::unique("composer") };
            ctx.memory_mut(|m| m.request_focus(composer));
        }
        self.editing = None;
    }
    /// Each tab keeps its own draft: when the tab shown changes, what was
    /// being written is put away and the new tab's comes back.
    pub(crate) fn switch_draft(&mut self) {
        let key = match &self.screen {
            Screen::Chat(id) => id.clone(),
            Screen::New => String::new(),
        };
        if key == self.draft_key {
            return;
        }
        let old = (std::mem::take(&mut self.input), std::mem::take(&mut self.attachments));
        let prev = std::mem::replace(&mut self.draft_key, key.clone());
        if !old.0.is_empty() || !old.1.is_empty() {
            self.drafts.insert(prev, old);
        }
        (self.input, self.attachments) = self.drafts.remove(&key).unwrap_or_default();
        self.hint = None;
        self.lists.values_mut().for_each(vlist::List::leave);
        if self.goal_edit.as_ref().is_some_and(|g| g.0 != key) {
            self.goal_edit = None;
        }
    }
}

impl App {
    pub(crate) fn apply_queue_action(&mut self, conv: &str, action: QueueAction) {
        let s = &self.sessions[conv];
        match action {
            QueueAction::Remove(id) => {
                s.unqueue(id);
                if self.editing.as_ref().is_some_and(|e| e.conv == conv && e.id == id) {
                    self.editing = None;
                }
            }
            QueueAction::Edit(id, text) => {
                // One edit at a time: starting another keeps what was typed.
                if let Some(prev) = self.editing.take()
                    && let Some(ps) = self.sessions.get(&prev.conv)
                {
                    ps.release(prev.id, Some(prev.text.trim().to_string()));
                }
                if s.hold(id) {
                    self.editing = Some(Editing { conv: conv.to_string(), id, text, focus: true });
                }
            }
            QueueAction::Save | QueueAction::Cancel => {
                if let Some(e) = self.editing.take() {
                    let text = matches!(action, QueueAction::Save).then(|| e.text.trim().to_string());
                    s.release(e.id, text);
                }
            }
        }
    }
}
