//! A super agent on this computer (a conversation with the super agent's
//! role, `agent::prompt::SUPER_ROLE`) looks as the command center's do
//! (`super_view`): its tab is a text conversation of what was said, with
//! its work out of sight, its corner box says what it is doing and lists
//! its subagents, and a subagent's tab has an agent's box. It still runs
//! here, as any conversation does: its subagents are conversations of
//! their own, in their own tabs.

use crate::agent::{self, Live};
use crate::session_box::{AgentState, BoxActions, model_label};
use crate::store::{Conversation, MarkKind};
use crate::super_view::{self, AgentBox, AgentRow, BoxAction, Chat, Icon, Model, Msg, Pic, SuperBox};
use crate::transcript::Item;
use crate::{App, Theme};
use eframe::egui::{self, Rect, Ui};

/// Whether conversation `c` is a super agent of this computer's own (not
/// one of its subagents, which have its role too).
pub(crate) fn is_super(c: &Conversation) -> bool {
    c.role == agent::prompt::SUPER_ROLE && c.parent.is_none() && !c.chat
}

/// Whether conversation `c` is a subagent of a super agent on this computer.
pub(crate) fn is_super_agent(c: &Conversation) -> bool {
    c.role == agent::prompt::SUPER_ROLE && c.parent.is_some()
}

/// What a super agent is doing, from its session: "Working" style words
/// as its transcript's status line has them; empty when idle.
fn activity(conv: &Conversation, live: &Live) -> (bool, String) {
    if live.pause_settled(conv.paused) {
        return (false, if live.ultra { "Stopped (ultrapause)".into() } else { "Paused".into() });
    }
    if !(live.busy || conv.paused || live.pause_queued) {
        return (false, String::new());
    }
    let status = if live.status.is_empty() || live.status == "Paused" { if live.busy { "Working" } else { "Pausing" } } else { live.status.as_str() };
    (true, status.to_string())
}

/// What was said in a super agent's conversation: what the user wrote
/// (with its images) and what the super agent said, in order; its tool
/// calls, thinking and subagents' messages stay out of it (their tabs
/// have them).
fn said(conv: &Conversation) -> Vec<Msg> {
    let (items, msgs, _) = crate::transcript::conv_items(conv);
    let mut messages: Vec<Msg> = vec![];
    // The user message being gathered: its message index.
    let mut user_at = None;
    for (i, item) in items.iter().enumerate() {
        let at = msgs.get(i).copied().unwrap_or(usize::MAX);
        let user = |messages: &mut Vec<Msg>, user_at: &mut Option<usize>| -> usize {
            if *user_at != Some(at) {
                *user_at = Some(at);
                messages.push(Msg { kind: "user".into(), time: -1, key: crate::hash_of(&(at, i)), ..Default::default() });
            }
            messages.len() - 1
        };
        match item {
            Item::UserText(text, end, time, _) => {
                let m = user(&mut messages, &mut user_at);
                let m = &mut messages[m];
                if !m.text.is_empty() {
                    m.text.push_str("\n\n");
                }
                m.text.push_str(text);
                if let Some(t) = time {
                    m.time = *t;
                }
                // Several messages sent while the agent works can share
                // one API turn. Keep their bubbles, times and attachments
                // separate, using the transcript's message boundaries.
                if end.is_some() {
                    user_at = None;
                }
            }
            Item::Image(b, end) => {
                let m = user(&mut messages, &mut user_at);
                messages[m].images.push(Pic { name: b["_oak_file"].as_str().unwrap_or_default().to_string(), block: Some((*b).clone()), failed: None });
                if end.is_some() {
                    user_at = None;
                }
            }
            Item::File(b, end) => {
                let m = user(&mut messages, &mut user_at);
                let name = b["_oak_file"].as_str().or(b["name"].as_str()).unwrap_or("file");
                let m = &mut messages[m];
                if !m.text.is_empty() {
                    m.text.push('\n');
                }
                m.text.push_str(&format!("[{name}]"));
                if end.is_some() {
                    user_at = None;
                }
            }
            Item::Text(text, key, at, ..) => {
                user_at = None;
                messages.push(Msg { kind: "oak".into(), time: at.unwrap_or(-1), text: text.to_string(), key: crate::hash_of(&key), ..Default::default() });
            }
            Item::Mark(m) if m.kind == MarkKind::Failed || (m.kind == MarkKind::Limit && m.done == 0) => {
                user_at = None;
                let text = if m.kind == MarkKind::Limit { "Usage limit reached; waiting for it to reset".to_string() } else { m.text.clone() };
                messages.push(Msg { kind: "error".into(), time: if m.time > 0 { m.time } else { -1 }, text, key: i as u64, ..Default::default() });
            }
            _ => {}
        }
    }
    // A user message whose text and images all were hidden (an agent's
    // message, a notification) isn't one.
    messages.retain(|m| m.kind != "user" || !m.text.trim().is_empty() || !m.images.is_empty());
    messages
}

/// A super agent's chat: what was said (`said`), then what waits to be
/// sent, and whether it is at work.
#[cfg(test)]
fn chat_of(id: &str, conv: &Conversation, live: &Live) -> Chat {
    let mut chat = Chat { messages: said(conv), ..Default::default() };
    update(&mut chat, id, conv, live);
    chat
}

/// What changes in a super agent's chat without its conversation changing:
/// what waits to be sent (after what was said, the first `said` messages),
/// whether it is at work, its trouble.
fn update(chat: &mut Chat, id: &str, conv: &Conversation, live: &Live) {
    for q in &live.queued {
        let images = q.images.iter().map(|b| Pic { name: b["_oak_file"].as_str().unwrap_or_default().to_string(), block: Some(b.clone()), failed: None }).collect();
        chat.messages.push(Msg { kind: "user".into(), time: 0, text: q.text.clone(), key: q.id, images, ..Default::default() });
    }
    (chat.working, chat.activity) = activity(conv, live);
    chat.salt = format!("super-chat-{id}");
    chat.intro = format!("Runs its agents on this computer, in {}.", conv.folder.display());
    chat.loaded = true;
    chat.trouble = live.error.clone();
}

thread_local! {
    /// Each super agent's tab's chat, kept while its conversation's
    /// revision is the one it was made at (with how many messages were
    /// said then): a long conversation isn't walked every frame.
    static CHATS: std::cell::RefCell<std::collections::HashMap<String, (u64, usize, Chat)>> = Default::default();
}

/// A local subagent's state in words, as the command center's agents'.
fn state_word(s: AgentState) -> &'static str {
    match s {
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Paused => "paused",
        AgentState::Pausing => "pausing",
    }
}

impl App {
    /// A super agent's tab on this computer: its chat (`super_view::chat`),
    /// then its questions for the user, if it asked any.
    pub(crate) fn local_super_view(&mut self, ui: &mut Ui, t: &Theme, id: &str) {
        let Some(s) = self.sessions.get(id) else { return };
        let conv = s.conv.lock().unwrap();
        let mut live = s.live.lock().unwrap();
        let rev = conv.rev();
        let (_, n, mut chat) = CHATS.with(|m| m.borrow_mut().remove(id)).filter(|c| c.0 == rev).unwrap_or_else(|| {
            let messages = said(&conv);
            (rev, messages.len(), Chat { messages, ..Default::default() })
        });
        chat.messages.truncate(n);
        update(&mut chat, id, &conv, &live);
        let mut ask = live.ask.take();
        let mut ask_reply = None;
        crate::markdown::set_link_base(ui.ctx(), Some(conv.folder.as_path()));
        super_view::chat(ui, t, &chat, |ui| {
            if let Some(a) = ask.as_mut()
                && crate::ask_view(ui, t, a)
            {
                a.sent = true;
            }
            if !conv.ask_request.is_empty() {
                ask_reply = crate::ask_request_view(ui, t, &conv.ask_request);
            }
        });
        crate::markdown::set_link_base(ui.ctx(), None);
        CHATS.with(|m| m.borrow_mut().insert(id.to_string(), (rev, n, chat)));
        live.ask = ask;
        drop(live);
        drop(conv);
        if let Some(accept) = ask_reply {
            s.answer_ask_request(accept);
        }
    }

    /// A super agent's corner box on this computer: its name and folder,
    /// what it is doing (Stop), its model and effort, and its subagents,
    /// each opening its tab.
    pub(crate) fn local_super_box(&mut self, ctx: &egui::Context, t: &Theme, area: Rect, key: &str) {
        let Some(s) = self.sessions.get(key) else { return };
        let models = match &*self.models.lock().unwrap() {
            Some(Ok(list)) => list.clone(),
            _ => vec![],
        };
        let (b, provider, agents) = {
            let conv = s.conv.lock().unwrap();
            let live = s.live.lock().unwrap();
            let (working, activity) = activity(&conv, &live);
            // (A model or effort switch waiting to go out shows as made.)
            let (mut model, mut effort, mut provider) = (conv.model.clone(), conv.effort.clone(), conv.provider());
            for cmd in &live.commands {
                match cmd {
                    agent::Command::Effort(e) => effort = e.clone(),
                    agent::Command::Model(p, m) => (provider, model) = (*p, m.clone()),
                    _ => {}
                }
            }
            let effort = agent::effort_for(&model, &effort);
            let mut list: Vec<Model> = models.iter().filter(|m| m.provider == provider).map(|m| Model { id: m.id.clone(), name: model_label(m), efforts: agent::efforts(&m.id).into_iter().map(String::from).collect() }).collect();
            if !list.iter().any(|m| m.id == model) {
                list.insert(0, Model { id: model.clone(), name: self.model_name(&model), efforts: agent::efforts(&model).into_iter().map(String::from).collect() });
            }
            let agents: Vec<(i64, String, String)> = conv.agents.iter().map(|a| (a.id as i64, a.name.clone(), a.conv.clone())).collect();
            let name = if conv.title.is_empty() { "Super agent".to_string() } else { conv.title.clone() };
            let b = SuperBox {
                salt: key.to_string(),
                icon: Icon::Tree,
                caption: format!("On this computer, in {}", conv.folder.display()),
                working,
                activity,
                stop: if live.busy { format!("Stops {name}'s turn; its agents carry on") } else { String::new() },
                clear: String::new(),
                delete: String::new(),
                models: list,
                model,
                effort,
                model_tip: format!("{name} picks the model of each agent it starts."),
                agents: vec![],
                name,
            };
            (b, provider, agents)
        };
        let mut b = b;
        // The subagents at work first, then the rest, the latest first in each.
        let mut rows: Vec<(AgentRow, String)> = agents
            .into_iter()
            .map(|(n, title, conv)| {
                let state = self.sessions.get(&conv).map_or(AgentState::Idle, AgentState::of);
                let report = self.sessions.get(&conv).map(|s| last_reply(&s.conv.lock().unwrap())).unwrap_or_default();
                (AgentRow { n, title, state: state_word(state).into(), live: true, working: state == AgentState::Working, report }, conv)
            })
            .collect();
        rows.sort_by_key(|(r, _)| (!r.working, std::cmp::Reverse(r.n)));
        let convs: Vec<(i64, String)> = rows.iter().map(|(r, c)| (r.n, c.clone())).collect();
        b.agents = rows.into_iter().map(|(r, _)| r).collect();
        crate::unslop::start_notes(&s.conv, ctx);
        let mut info = crate::session_box::SessionInfo::new(Some(s), &self.settings, models, |m| self.model_name(m));
        for a in &mut info.agents {
            a.3 = self.sessions.get(&a.2).map_or(AgentState::Idle, AgentState::of);
        }
        if let Some((_, parent, title)) = &mut info.parent {
            *title = self.sessions.get(parent.as_str()).map(|s| s.conv.lock().unwrap().title.clone()).unwrap_or_default();
        }
        let tasks = s.tasks.clone();
        let bottom = area.bottom() - crate::look::GAP;
        let mut controls = BoxActions::default();
        let editing = self.goal_edit.as_mut().filter(|g| g.key == key);
        let act = super_view::super_box(ctx, t, area, &b, |ui, w| local_controls(ui, t, w, bottom, key, &info, &tasks, editing, &mut controls));
        let Some(s) = self.sessions.get(key) else { return };
        match act {
            Some(BoxAction::Stop) => s.interrupt(),
            Some(BoxAction::Model(m, e)) => {
                if e != b.effort {
                    s.command(agent::Command::Effort(e));
                }
                if m != b.model {
                    self.model_switch = Some((key.to_string(), provider, m));
                }
            }
            Some(BoxAction::Open(n)) => {
                if let Some((_, conv)) = convs.into_iter().find(|(m, _)| *m == n)
                    && self.sessions.contains_key(&conv)
                {
                    self.open(conv);
                }
            }
            _ => {}
        }
        self.box_actions(ctx, t, key, &info, controls);
    }

    /// A super agent's subagent's corner box on this computer: an agent's
    /// box (its title, whose it is, what it is doing with Stop, its model),
    /// then its notes and background tasks when it has any.
    pub(crate) fn local_agent_box(&mut self, ctx: &egui::Context, t: &Theme, area: Rect, key: &str) {
        let Some(s) = self.sessions.get(key) else { return };
        let state = AgentState::of(s);
        let b = {
            let conv = s.conv.lock().unwrap();
            let live = s.live.lock().unwrap();
            let (working, activity) = activity(&conv, &live);
            let parent = conv.parent.clone().unwrap_or_default();
            let boss = self.sessions.get(&parent).map(|p| p.conv.lock().unwrap().title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "its super agent".into());
            AgentBox {
                icon: Icon::Tree,
                title: if conv.title.is_empty() { format!("Agent {}", conv.agent_id) } else { conv.title.clone() },
                caption: format!("Agent {}, run by {boss}", conv.agent_id),
                working,
                state: if working && !activity.is_empty() { activity } else { state_word(state).into() },
                stop: if live.busy { "Stops its turn".into() } else { String::new() },
                model: format!("{} at {} effort", self.model_name(&conv.model), agent::effort_for(&conv.model, &conv.effort)),
            }
        };
        let models = match &*self.models.lock().unwrap() {
            Some(Ok(list)) => list.clone(),
            _ => vec![],
        };
        crate::unslop::start_notes(&s.conv, ctx);
        let mut info = crate::session_box::SessionInfo::new(Some(s), &self.settings, models, |m| self.model_name(m));
        for a in &mut info.agents {
            a.3 = self.sessions.get(&a.2).map_or(AgentState::Idle, AgentState::of);
        }
        if let Some((_, parent, title)) = &mut info.parent {
            *title = self.sessions.get(parent.as_str()).map(|s| s.conv.lock().unwrap().title.clone()).unwrap_or_default();
        }
        let tasks = s.tasks.clone();
        let bottom = area.bottom() - crate::look::GAP;
        let mut controls = BoxActions::default();
        let editing = self.goal_edit.as_mut().filter(|g| g.key == key);
        let stop = super_view::agent_box(ctx, t, area, &b, |ui, w| local_controls(ui, t, w, bottom, key, &info, &tasks, editing, &mut controls));
        if stop {
            s.interrupt();
        }
        self.box_actions(ctx, t, key, &info, controls);
    }
}

/// A local super agent's or subagent's notes and background tasks, under
/// its box, when it has any (the command center's have neither).
#[allow(clippy::too_many_arguments)]
fn local_controls(ui: &mut Ui, t: &Theme, w: f32, bottom: f32, key: &str, info: &crate::session_box::SessionInfo, tasks: &crate::tasks::Tasks, editing: Option<&mut crate::session_box::GoalEdit>, act: &mut BoxActions) {
    // Keep the common super-agent layout uncluttered, without taking away
    // a local conversation's provider switching, goals, meters, heartbeats,
    // folder actions or navigation back to its parent.
    crate::look::panel_box(ui, t, w, 8, 2.0, |ui| {
        let label = if info.goal.is_empty() { "Local controls" } else { "Local controls (Goal)" };
        egui::CollapsingHeader::new(label).id_salt(("local-super-controls", key))
            .open(editing.as_ref().map(|_| true)).show(ui, |ui| {
                crate::session_box::session_rows(ui, t, info, editing, act);
            });
    });
    if !info.notes.is_empty() || !tasks.infos().is_empty() {
        crate::session_box::side_boxes(ui, t, w, bottom, key, info, tasks);
    }
}

/// What a subagent said last, for its row's hover: its last reply's text.
fn last_reply(c: &Conversation) -> String {
    for m in c.messages.iter().rev() {
        if m["role"] != "assistant" {
            continue;
        }
        if let Some(text) = m["content"].as_array().and_then(|b| b.iter().rev().find_map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten())) {
            let text = text.trim();
            return text.chars().take(300).collect();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Queued;
    use serde_json::json;

    /// A super agent's chat has what the user and it said, not its work
    /// or its subagents' messages, then what waits to be sent.
    #[test]
    fn the_chat_is_what_was_said() {
        let mut c = Conversation { role: agent::prompt::SUPER_ROLE.into(), ..Default::default() };
        c.messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "Ship the release"}]}),
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "plan it"}, {"type": "text", "text": "On it."}, {"type": "tool_use", "id": "t1", "name": "EXECUTE", "input": {"command": "make"}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "built"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Agent 1 runs the build."}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "<agent-message from=\"agent 1 (Build)\" type=\"final reply\">\nIt passes.\n</agent-message>"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "The build passes."}]}),
        ]
        .into();
        assert!(is_super(&c) && !is_super_agent(&c));
        let mut live = Live::default();
        live.queued.push(Queued { id: 9, text: "Tag it".into(), images: vec![], held: false });
        let chat = chat_of("k", &c, &live);
        let said: Vec<(&str, &str)> = chat.messages.iter().map(|m| (m.kind.as_str(), m.text.as_str())).collect();
        assert_eq!(said, vec![("user", "Ship the release"), ("oak", "On it."), ("oak", "Agent 1 runs the build."), ("oak", "The build passes."), ("user", "Tag it")]);
        assert_eq!(chat.messages.last().unwrap().time, 0, "waiting to be sent");
        assert!(!chat.working && chat.loaded);

        let sub = Conversation { role: agent::prompt::SUPER_ROLE.into(), parent: Some("k".into()), agent_id: 1, ..Default::default() };
        assert!(!is_super(&sub) && is_super_agent(&sub));
        assert!(!is_super(&Conversation::default()));
    }

    #[test]
    fn saved_super_agents_keep_message_boundaries_and_attachments() {
        let mut c = Conversation { role: agent::prompt::SUPER_ROLE.into(), ..Default::default() };
        // Messages received during a turn share an API user message,
        // but each attachment still belongs to its own message.
        c.messages = vec![json!({"role": "user", "content": [
            {"type": "image", "_oak_file": "first.png"},
            {"type": "text", "text": "First request", "_oak_at": 1000},
            {"type": "text", "text": "Second request", "_oak_at": 2000000},
            {"type": "image", "_oak_file": "last.png"}
        ]})].into();
        // No migration or new persisted fields are needed for this UI.
        let saved = serde_json::to_vec(&c).unwrap();
        let restored: Conversation = serde_json::from_slice(&saved).unwrap();
        assert!(is_super(&restored));
        let chat = chat_of("restored", &restored, &Live::default());
        let msgs = &chat.messages;
        assert_eq!(msgs.len(), 3);
        assert_eq!((msgs[0].text.as_str(), msgs[0].time), ("First request", 1000));
        assert_eq!(msgs[0].images[0].name, "first.png");
        assert_eq!((msgs[1].text.as_str(), msgs[1].time), ("Second request", 2000000));
        assert!(msgs[1].images.is_empty());
        assert!(msgs[2].text.is_empty());
        assert_eq!(msgs[2].images[0].name, "last.png");
        assert_ne!(msgs[0].key, msgs[1].key);
        assert_ne!(msgs[1].key, msgs[2].key);

        c.parent = Some("boss".into());
        c.agent_id = 3;
        let sub: Conversation = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(is_super_agent(&sub) && !is_super(&sub));
    }

    #[test]
    fn local_controls_keep_goals_and_heartbeat_management() {
        use crate::app_hover_test::{painted, run_frame, test_ctx};
        let (ctx, t) = test_ctx(true, true);
        let mut info = crate::session_box::SessionInfo::new(None, &crate::store::Settings::default(), vec![], str::to_string);
        info.new = false;
        info.goal = "Ship the release".into();
        info.heartbeats = vec![crate::store::Heartbeat { id: 4, message: "Check the build".into(), every_min: 10, next_at: 0 }];
        info.parent = Some((1, "boss".into(), "Release train".into()));
        let tasks = crate::tasks::Tasks::default();
        let mut edit = crate::session_box::GoalEdit { key: "a".into(), text: info.goal.clone(), focus: false, endless: false };
        let mut time = 0.0;
        let mut shown = vec![];
        for _ in 0..3 {
            shown = painted(&run_frame(&ctx, egui::vec2(900.0, 1100.0), &mut time, 0.1, vec![], |ui| {
                let mut act = BoxActions::default();
                local_controls(ui, &t, 240.0, 1000.0, "a", &info, &tasks, Some(&mut edit), &mut act);
            }));
        }
        let text = shown.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join("|");
        for wanted in ["Local controls", "Claude", "Ship the release", "Release train", "Check the build"] {
            assert!(text.contains(wanted), "missing {wanted}: {text}");
        }
    }

}
