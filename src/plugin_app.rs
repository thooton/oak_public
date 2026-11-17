//! The window as plugins reach it (`plugins::api`'s `app_` functions): what
//! a plugin's tab, form or hook can do to the tabs, the message box and
//! the New screen while it draws.

use super::*;
use crate::App;
use oak_sdk::chat::Chat;
use oak_sdk::host::NewConversation;

impl App {
    /// Closes the tabs `gone` says to, and forgets them as tabs to reopen.
    pub(crate) fn plugin_close_tabs(&mut self, gone: &dyn Fn(&str) -> bool) {
        while let Some(i) = self.settings.tabs.iter().rposition(|id| gone(id)) {
            App::close_tabs(self, i, tabs::TabClose::One);
        }
        // What is gone can't be reopened either.
        self.closed_tabs.retain(|(_, id)| !gone(id));
        self.plugin_chats.retain(|id, _| !gone(id));
    }

    /// The key of the tab shown, when one is.
    pub(crate) fn plugin_shown(&self) -> Option<String> {
        match (&self.screen, self.page) {
            (Screen::Chat(id), Page::Chats) => Some(id.clone()),
            _ => None,
        }
    }

    /// Shows the page of the Home tab `id`, when it has one.
    pub(crate) fn plugin_go_page(&mut self, id: &str) {
        if crate::plugins::parts().home_tabs.iter().any(|tab| tab.page() && tab.id() == id) {
            self.plugin_page = id.to_string();
            self.page = Page::Plugin;
        }
    }

    /// Shows Settings; its X comes back to what was shown.
    pub(crate) fn plugin_go_settings(&mut self) {
        if self.page != Page::Settings {
            self.settings_from = Some(self.page);
            self.page = Page::Settings;
        }
    }

    pub(crate) fn plugin_go_home(&mut self, tab: &str) {
        self.page = Page::Home;
        self.home_menu = true;
        self.home_list = tab.to_string();
    }

    /// Shows the New screen with `kind` picked.
    pub(crate) fn plugin_go_new(&mut self, kind: &str) {
        self.screen = Screen::New;
        self.page = Page::Chats;
        self.hint = None;
        use crate::new_screen::Here;
        // (Oak's own choices first: a plugin's of the same id can't hide one.)
        let here = match kind {
            "" | home::LOCAL_TAB => Some(Here::Folder),
            crate::new_screen::CHAT_KIND => Some(Here::Chat),
            crate::new_screen::SUPER_KIND => Some(Here::Super),
            _ => None,
        };
        if let Some(here) = here {
            (self.new_cloud, self.new_kind, self.new_here) = (false, None, here);
        } else if crate::plugins::parts().new_kinds.iter().any(|k| k.id() == kind) {
            (self.new_cloud, self.new_kind) = (false, Some(kind.to_string()));
        } else if let Some(provider) = cloud::Provider::from_id(kind) {
            (self.new_cloud, self.new_kind, self.new_provider) = (true, None, provider);
        }
    }

    /// This machine's conversations (not side chats, not subagents).
    pub(crate) fn plugin_conversations(&self) -> Vec<oak_sdk::host::Conversation> {
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

    /// What Oak and its conversations are doing (`oak_sdk::App::activity`).
    pub(crate) fn plugin_activity(&self, ctx: &egui::Context) -> oak_sdk::host::Activity {
        let shown = match &self.screen {
            Screen::Chat(id) if self.page == Page::Chats => Some(id.as_str()),
            _ => None,
        };
        let mut tasks = 0;
        let mut conversations: Vec<oak_sdk::host::Doing> = self
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                let c = s.conv.lock().unwrap();
                if c.side_chat_of.is_some() {
                    return None;
                }
                tasks += s.tasks.running();
                let l = s.live.lock().unwrap();
                let (phase, tool) = l.plugin_phase();
                Some(oak_sdk::host::Doing {
                    id: id.clone(),
                    title: c.title.clone(),
                    busy: l.busy,
                    phase: phase.into(),
                    status: if l.busy { l.status.clone() } else { String::new() },
                    tool,
                    asking: l.ask.is_some(),
                    failed: !l.busy && l.error.is_some(),
                    paused: c.paused,
                    shown: shown == Some(id.as_str()),
                    at: c.last_message_at,
                })
            })
            .collect();
        conversations.sort_by_key(|c| std::cmp::Reverse(c.at));
        let rect = |r: Option<egui::Rect>| r.map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]);
        let (maximized, fullscreen, focused) = ctx.input(|i| {
            let v = i.viewport();
            (v.maximized.unwrap_or(false), v.fullscreen.unwrap_or(false), v.focused.unwrap_or(true))
        });
        let home = self.page == Page::Home;
        oak_sdk::host::Activity {
            page: match self.page {
                Page::Home => "home",
                Page::Chats => "chats",
                Page::Settings => "settings",
                Page::Usage => "usage",
                Page::Plugin => "plugin",
            }
            .into(),
            menu: home && self.home_menu,
            menu_rect: if home && self.home_menu { rect(self.home_menu_rect) } else { None },
            buttons_rect: if home { rect(self.home_buttons_rect) } else { None },
            maximized,
            fullscreen,
            focused,
            tasks,
            conversations,
        }
    }

    pub(crate) fn plugin_start_conversation(&mut self, new: NewConversation) -> Result<String, String> {
        let Some(ctx) = crate::plugins::context() else { return Err("Oak's window isn't up yet.".into()) };
        // A plugin that names its own folder, model, effort or role starts
        // a conversation of those, whatever the New screen's rows were set
        // to; otherwise it starts what they show, as the New screen would.
        let custom = !new.folder.trim().is_empty() || !new.model.trim().is_empty() || !new.provider.trim().is_empty() || !new.effort.trim().is_empty() || new.chat;
        if !custom {
            self.hint = None;
            let role = if new.role.is_empty() { self.settings.role.clone() } else { new.role };
            return match self.start_conversation_as(new.text, new.files, None, Some((role, new.title)), &ctx) {
                Some(id) => Ok(id),
                None => Err(self.hint.clone().unwrap_or_else(|| "The conversation couldn't start.".into())),
            };
        }
        let role = if new.role.is_empty() { self.settings.role.clone() } else { new.role.clone() };
        match self.start_conversation_with(new, role, &ctx) {
            Ok(id) => Ok(id),
            Err(why) => {
                self.hint = Some(why.clone());
                Err(why)
            }
        }
    }

    /// Starts a conversation a plugin names the folder, model, effort and
    /// role of (`NewConversation`), opens its tab, and sends its message.
    /// Ok is its id.
    fn start_conversation_with(&mut self, new: NewConversation, role: String, ctx: &egui::Context) -> Result<String, String> {
        let folder = if new.chat {
            std::path::PathBuf::new()
        } else {
            let typed = if new.folder.trim().is_empty() { self.settings.folder.trim() } else { new.folder.trim() };
            if typed.is_empty() {
                return Err("Folder required".into());
            }
            let folder = instructions::expand_home(typed, &instructions::user_home());
            if !folder.is_dir() {
                return Err(format!("Folder not found: {}", folder.display()));
            }
            folder
        };
        let (provider, model) = if new.model.trim().is_empty() {
            let provider = self.settings.provider.unwrap_or_else(|| api::Provider::of(&self.settings.model));
            (provider, self.settings.model.clone())
        } else {
            let provider = if new.provider.trim().is_empty() { api::Provider::of(&new.model) } else { providers::Kind::from_id(new.provider.trim()).ok_or_else(|| format!("Oak doesn't know the provider {}.", new.provider.trim()))? };
            (provider, new.model.trim().to_string())
        };
        if model.is_empty() {
            return Err("Model required".into());
        }
        if let Some(why) = agent::chat_refuses_provider(new.chat, provider) {
            return Err(why);
        }
        let effort = if new.effort.trim().is_empty() { self.settings.effort.clone() } else { new.effort.trim().to_string() };
        let now = api::now_ms();
        let mut conv = Conversation {
            id: now.to_string(),
            title: String::new(),
            folder,
            provider: Some(provider),
            model: model.clone(),
            effort: agent::effort_for(&model, &effort),
            role: if new.chat { String::new() } else { agent::prompt::role(&role).id.to_string() },
            chat: new.chat,
            messages: Default::default(),
            last_message_at: now,
            next_task_id: 0,
            rolling: rolling::State { off: !self.settings.rolling, ..Default::default() },
            ..Default::default()
        };
        if !new.title.is_empty() {
            conv.title = new.title.head(60);
            conv.titled = true;
        }
        let id = conv.id.clone();
        let s = Session::start(conv, ctx.clone());
        if !new.text.is_empty() || !new.files.is_empty() {
            s.send(new.text, new.files);
        }
        self.sessions.insert(id.clone(), s);
        self.open(id.clone());
        self.hint = None;
        Ok(id)
    }

    /// Draws a transcript a plugin has, as a conversation's is drawn.
    pub(crate) fn plugin_transcript(&mut self, ui: &mut Ui, key: &str, version: u64, make: &mut dyn FnMut() -> Chat, tail_sig: u64, tail: &mut dyn FnMut(&mut Ui)) {
        let t = theme::theme(&self.settings.theme);
        if self.plugin_chats.get(key).is_none_or(|(v, ..)| *v != version) {
            let (conv, live) = as_conversation(key, make());
            self.plugin_chats.insert(key.to_string(), (version, conv, live));
        }
        let (_, conv, live) = &self.plugin_chats[key];
        let list = self.lists.entry(key.to_string()).or_default();
        chat_view::read_only_view(ui, &t, conv, live, list, tail_sig, |ui| tail(ui));
    }

    pub(crate) fn plugin_cloud_list(&mut self, ui: &mut Ui, backend: &str) {
        let Some(provider) = cloud::Provider::from_id(backend) else { return };
        let t = theme::theme(&self.settings.theme);
        if let Some(key) = cloud_list_view(ui, &t, provider) {
            App::open(self, key);
        }
    }

    /// A conversation of this machine's, as a plugin sees it: what it is,
    /// what it is doing and how its last turn went. None: no session has
    /// `key`.
    pub(crate) fn plugin_session(&self, key: &str) -> Option<oak_sdk::host::Session> {
        let s = self.sessions.get(key)?;
        let c = s.conv.lock().unwrap();
        let live = s.live.lock().unwrap();
        Some(oak_sdk::host::Session {
            id: key.to_string(),
            title: c.title.clone(),
            folder: c.folder.display().to_string(),
            model: c.model.clone(),
            provider: c.provider().id().to_string(),
            effort: c.effort.clone(),
            role: c.role.clone(),
            chat: c.chat,
            busy: live.busy,
            status: live.status.clone(),
            error: live.error.clone(),
            reply: crate::agent::reply_text(&c),
        })
    }

    /// Switches a conversation's model, as its picker does (a `Command`,
    /// which the worker takes next), but without asking.
    pub(crate) fn plugin_set_model(&mut self, key: &str, provider: &str, model: &str) -> Result<(), String> {
        let Some(s) = self.sessions.get(key) else { return Err(format!("There is no conversation {key}.")) };
        let provider = providers::Kind::from_id(provider).ok_or_else(|| format!("Oak doesn't know the provider {provider}."))?;
        if model.trim().is_empty() {
            return Err("No model was named.".into());
        }
        s.command(Command::Model(provider, model.to_string()));
        Ok(())
    }

    /// Sends a conversation a message, as its message box does.
    pub(crate) fn plugin_send(&mut self, key: &str, text: &str) -> Result<(), String> {
        let Some(s) = self.sessions.get(key) else { return Err(format!("There is no conversation {key}.")) };
        s.send(text.to_string(), vec![]);
        Ok(())
    }

    /// Stops what a conversation is doing, as its interrupt does.
    pub(crate) fn plugin_interrupt(&mut self, key: &str) -> Result<(), String> {
        let Some(s) = self.sessions.get(key) else { return Err(format!("There is no conversation {key}.")) };
        s.interrupt();
        Ok(())
    }

    /// Asks a conversation to answer its last turn, as resuming a paused
    /// one does.
    pub(crate) fn plugin_resume(&mut self, key: &str) -> Result<(), String> {
        let Some(s) = self.sessions.get(key) else { return Err(format!("There is no conversation {key}.")) };
        s.resume_next();
        Ok(())
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
