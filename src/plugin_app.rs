//! The window as plugins see it (`oak_sdk::App`): what a plugin's tab, form
//! or hook can do to the tabs, the message box and the New screen while it
//! draws.

use super::*;
use oak_sdk::chat::Chat;
use oak_sdk::host::NewConversation;

impl oak_sdk::App for App {
    fn theme(&self) -> Theme {
        theme::theme(&self.settings.theme)
    }

    fn open(&mut self, key: &str) {
        App::open(self, key.to_string());
    }

    fn close_tabs(&mut self, gone: &dyn Fn(&str) -> bool) {
        while let Some(i) = self.settings.tabs.iter().rposition(|id| gone(id)) {
            App::close_tabs(self, i, tabs::TabClose::One);
        }
        // What is gone can't be reopened either.
        self.closed_tabs.retain(|(_, id)| !gone(id));
        self.plugin_chats.retain(|id, _| !gone(id));
    }

    fn shown(&self) -> Option<String> {
        match (&self.screen, self.page) {
            (Screen::Chat(id), Page::Chats) => Some(id.clone()),
            _ => None,
        }
    }

    fn go_home(&mut self, tab: &str) {
        self.page = Page::Home;
        self.home_list = tab.to_string();
    }

    fn go_new(&mut self, kind: &str) {
        self.screen = Screen::New;
        self.page = Page::Chats;
        self.hint = None;
        if kind.is_empty() || kind == home::LOCAL_TAB {
            (self.new_cloud, self.new_kind) = (false, None);
        } else if crate::plugins::parts().new_kinds.iter().any(|k| k.id() == kind) {
            (self.new_cloud, self.new_kind) = (false, Some(kind.to_string()));
        } else if let Some(provider) = cloud::Provider::from_id(kind) {
            (self.new_cloud, self.new_kind, self.new_provider) = (true, None, provider);
        }
    }

    fn hint(&mut self, text: Option<String>) {
        self.hint = text;
    }

    fn input(&mut self) -> &mut String {
        &mut self.input
    }

    fn attachments(&mut self) -> &mut Vec<Value> {
        &mut self.attachments
    }

    fn conversations(&self) -> Vec<oak_sdk::host::Conversation> {
        let mut list: Vec<oak_sdk::host::Conversation> = self
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                let c = s.conv.lock().unwrap();
                if c.side_chat_of.is_some() || c.parent.is_some() {
                    return None;
                }
                Some(oak_sdk::host::Conversation {
                    id: id.clone(),
                    title: c.title.clone(),
                    folder: c.folder.display().to_string(),
                    model: c.model.clone(),
                    role: c.role.clone(),
                    at: c.last_message_at,
                    busy: s.live.lock().unwrap().busy,
                })
            })
            .collect();
        list.sort_by_key(|c| std::cmp::Reverse(c.at));
        list
    }

    fn start_conversation(&mut self, new: NewConversation) -> Result<String, String> {
        let Some(ctx) = crate::plugins::host().context() else { return Err("Oak's window isn't up yet.".into()) };
        self.hint = None;
        let role = if new.role.is_empty() { self.settings.role.clone() } else { new.role };
        match self.start_conversation_as(new.text, new.files, None, Some((role, new.title)), &ctx) {
            Some(id) => Ok(id),
            None => Err(self.hint.clone().unwrap_or_else(|| "The conversation couldn't start.".into())),
        }
    }

    fn local_form(&mut self, ui: &mut Ui) {
        let t = theme::theme(&self.settings.theme);
        self.local_rows(ui, &t);
    }

    fn transcript(&mut self, ui: &mut Ui, key: &str, version: u64, make: &mut dyn FnMut() -> Chat, tail_sig: u64, tail: &mut dyn FnMut(&mut Ui)) {
        let t = theme::theme(&self.settings.theme);
        if self.plugin_chats.get(key).is_none_or(|(v, ..)| *v != version) {
            let (conv, live) = as_conversation(key, make());
            self.plugin_chats.insert(key.to_string(), (version, conv, live));
        }
        let (_, conv, live) = &self.plugin_chats[key];
        let list = self.lists.entry(key.to_string()).or_default();
        chat_view::read_only_view(ui, &t, conv, live, list, tail_sig, |ui| tail(ui));
    }

    fn cloud_list(&mut self, ui: &mut Ui, backend: &str) {
        let Some(provider) = cloud::Provider::from_id(backend) else { return };
        let t = theme::theme(&self.settings.theme);
        if let Some(key) = cloud_list_view(ui, &t, provider) {
            App::open(self, key);
        }
    }
}

/// A plugin's transcript as the conversation Oak's transcript code draws.
fn as_conversation(key: &str, chat: Chat) -> (Conversation, Live) {
    let marks = chat
        .marks
        .into_iter()
        // (A plain line is drawn as a stopped heartbeat's is: its text.)
        .map(|m| store::Mark::new(m.at, if m.failed { store::MarkKind::Failed } else { store::MarkKind::Heartbeat }, m.text, m.time))
        .collect();
    let conv = Conversation { id: key.to_string(), messages: chat.messages.into(), marks, ..Default::default() };
    let mut live = Live::default();
    (live.busy, live.status) = (chat.busy, chat.status);
    (conv, live)
}
