//! Retained side chats are ordinary Sessions with a `side_chat_of` link, not
//! subagents: they never report back or inherit running work. The bubble
//! reopens the same fork in the right-hand pane, sharing its space with the
//! browser. It draws the normal transcript and composer controls, without
//! the session box.
//! Drafts, queued edits, reverts, tasks, shutdown and files use the same paths
//! as other conversations. Closing the pane only hides it.

use super::*;

pub(crate) const FORK_MESSAGE: &str = "The session was forked to be placed in a \"side chat\". This is only a chat for asking questions or requesting small changes, so do not continue work as you were before it was forked. Only make changes to the project if the user explicitly requests so.";

/// A snapshot of the durable context, not the source's worker, task ownership,
/// goal, timers, inbox, usage totals or account/server thread. Complete any
/// in-flight tool calls in the copy before adding the fork instruction.
pub(crate) fn fork(source: &Conversation, id: String, now: i64) -> Conversation {
    let mut c = Conversation {
        id,
        title: format!("Side chat: {}", source.title),
        titled: true,
        folder: source.folder.clone(),
        provider: source.provider,
        model: source.model.clone(),
        effort: source.effort.clone(),
        role: source.role.clone(),
        messages: source.messages.clone(),
        format: source.format,
        rolling: source.rolling.clone(),
        prefix: source.prefix.clone(),
        notes: source.notes.clone(),
        next_note_id: source.next_note_id,
        marks: source.marks.clone(),
        last_message_at: now,
        side_chat_of: Some(source.id.clone()),
        ..Default::default()
    };
    agent::answer_lost_calls(&mut c);
    c.messages.push(serde_json::json!({"role": "user", "content": [oak::text(FORK_MESSAGE)]}));
    c
}

impl App {
    fn side_chat_for(&self, source: &str) -> Option<String> {
        self.sessions.iter().find_map(|(id, s)| (s.conv.lock().unwrap().side_chat_of.as_deref() == Some(source)).then(|| id.clone()))
    }

    fn open_side_chat(&mut self, source: &str, ctx: &egui::Context) {
        let id = self.side_chat_for(source).unwrap_or_else(|| {
            let now = api::now_ms();
            let id = format!("side-{now}-{:016x}", rand::random::<u64>());
            let c = fork(&self.sessions[source].conv.lock().unwrap(), id.clone(), now);
            c.save();
            self.sessions.insert(id.clone(), Session::start(c, ctx.clone()));
            id
        });
        if self.side_chat.as_deref() != Some(&id) {
            self.side_hint = None;
            self.side_editing = None;
        }
        self.side_chat = Some(id);
        self.side_deleting = false;
        self.side_focus = true;
        self.side_active = true;
        self.side_browser = false;
    }

    /// Immediately left of the browser's globe. Right-click also offers
    /// deletion when the retained chat's pane is closed.
    pub(crate) fn side_chat_toggle(&mut self, ui: &mut Ui, t: &Theme) {
        let Some(source) = self.browser_conv() else { return };
        let r = ui.max_rect();
        let rect = Rect::from_min_size(egui::pos2(r.right() - 3.0 * 36.0 - 76.0, r.top()), vec2(36.0, r.height()));
        let resp = ui.interact(rect, Id::unique("side-chat-toggle"), Sense::click());
        let side = self.side_chat_for(&source);
        let open = side.is_some() && self.side_chat == side && !self.side_browser;
        if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, t.panel);
        }
        let color = if open { t.accent } else if resp.hovered() { t.ink } else { t.ink3 };
        let bubble = Rect::from_center_size(rect.center() - vec2(0.0, 1.0), vec2(16.0, 12.0));
        let p = ui.painter();
        p.rect_stroke(bubble, 3.0, Stroke::new(1.3, color), egui::StrokeKind::Inside);
        p.line_segment([bubble.left_bottom() + vec2(4.0, -1.0), bubble.left_bottom() + vec2(3.0, 4.0)], Stroke::new(1.3, color));
        p.line_segment([bubble.left_bottom() + vec2(3.0, 4.0), bubble.left_bottom() + vec2(8.0, -0.5)], Stroke::new(1.3, color));
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if open { "Close side chat" } else { "Open side chat" });
        if resp.clicked() {
            if open { self.close_side_chat(); } else { self.open_side_chat(&source, ui.ctx()); }
        }
        if let Some(id) = side {
            resp.context_menu(|ui| {
                if ui.button("Delete side chat").clicked() {
                    self.side_chat = Some(id);
                    self.side_deleting = true;
                    self.side_browser = false;
                    ui.close();
                }
            });
        }
    }

    /// Only the source's selected tab shows its side chat. Other tabs keep
    /// their ordinary layout and browser, without changing the retained fork.
    pub(crate) fn shown_side_chat(&self) -> Option<String> {
        let source = self.browser_conv()?;
        let id = self.side_chat.as_ref()?;
        (self.sessions.get(id)?.conv.lock().unwrap().side_chat_of.as_deref() == Some(&source)).then(|| id.clone())
    }

    pub(crate) fn side_chat_focused(&self) -> bool {
        self.side_active && !self.side_browser && self.shown_side_chat().is_some()
    }

    pub(crate) fn close_side_chat(&mut self) {
        self.side_chat = None;
        self.side_deleting = false;
        self.side_active = false;
    }

    /// One resizable right-hand pane. Switching its tabs leaves the browser
    /// running and the side chat's session and draft untouched.
    pub(crate) fn conversation_pane(&mut self, ui: &mut Ui, t: &Theme) {
        let side = self.shown_side_chat();
        let inst = self.browser_conv().and_then(|c| browser::get(&c));
        if side.is_none() && inst.is_none() { return; }
        if inst.is_none() { self.side_browser = false; }
        let max = (ui.max_rect().width() - 380.0).max(320.0);
        egui::Panel::right("browser-pane")
            .resizable(true).default_size(browser_view::PANE_DEFAULT_W.min(max)).size_range(320.0..=max)
            .frame(egui::Frame::new().fill(t.bg))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.expand_to_include_rect(r);
                ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0, t.line));
                if ui.input(|i| i.pointer.any_pressed()) {
                    self.side_active = ui.rect_contains_pointer(r) && !self.side_browser;
                }
                let target = if self.side_active && !self.side_browser { side.clone() } else {
                    match &self.screen { Screen::Chat(id) => Some(id.clone()), Screen::New => None }
                };
                find::keyboard_target(ui.ctx(), target);
                if let Some(id) = side.as_deref() {
                    self.side_chat_bar(ui, t, id, inst.is_some());
                }
                if let Some(id) = side.as_deref().filter(|_| !self.side_browser) {
                    self.side_chat_contents(ui, t, id);
                } else if let Some(inst) = &inst {
                    browser_view::pane(ui, t, inst);
                }
            });
    }

    fn side_chat_bar(&mut self, ui: &mut Ui, t: &Theme, id: &str, browser: bool) {
        egui::Panel::top("side-chat-bar").exact_size(34.0)
            .frame(egui::Frame::new().fill(t.panel)).show(ui, |ui| {
                let r = ui.max_rect();
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    if ui.selectable_label(!self.side_browser, "Side chat").clicked() {
                        self.side_browser = false;
                        self.side_active = true;
                        self.side_focus = true;
                    }
                    if browser && ui.selectable_label(self.side_browser, "Browser").clicked() {
                        self.side_browser = true;
                        self.side_active = false;
                    }
                });
                if !self.side_browser {
                    let close = Rect::from_min_size(egui::pos2(r.right() - 34.0, r.top() + 2.0), vec2(30.0, 30.0));
                    let delete = close.translate(vec2(-30.0, 0.0));
                    if widgets::icon_button(ui, t, delete, Id::unique(("side-delete", id)), "Delete chat", paint_delete) {
                        self.side_deleting = true;
                    }
                    if widgets::icon_button(ui, t, close, Id::unique(("side-close", id)), "Close side chat", widgets::draw_x) {
                        self.close_side_chat();
                    }
                }
            });
    }

    fn side_chat_contents(&mut self, ui: &mut Ui, t: &Theme, id: &str) {
        // Reuse the ordinary composer/actions with the fork's draft, hint
        // and queued edit. Restore the main conversation before drawing it.
        let main_screen = std::mem::replace(&mut self.screen, Screen::Chat(id.into()));
        let main_key = std::mem::replace(&mut self.draft_key, id.into());
        let (text, files) = self.drafts.remove(id).unwrap_or_default();
        let main_input = std::mem::replace(&mut self.input, text);
        let main_files = std::mem::replace(&mut self.attachments, files);
        std::mem::swap(&mut self.hint, &mut self.side_hint);
        std::mem::swap(&mut self.editing, &mut self.side_editing);
        let plain = egui::Frame::new().fill(t.bg);
        let mut delete = false;
        if self.side_deleting {
            let body = |ui: &mut Ui| {
                ui.label(RichText::new("Its transcript goes; the conversation it came from stays. This can't be undone.").color(t.ink2));
            };
            let (act, close) = widgets::confirm_dialog(ui.ctx(), t, "side-chat-delete", "Delete side chat?", body, None, "Delete", false);
            delete = act;
            if act || close { self.side_deleting = false; }
        }
        egui::Panel::bottom("side-composer").frame(plain).show(ui, |ui| {
            if self.side_active { composer::take_drops(ui.ctx(), &mut self.attachments, &mut self.hint); }
            if let Some(hint) = &self.hint { ui.colored_label(t.ink2, hint); }
            if !self.attachments.is_empty() {
                composer::attachment_strip(ui, t, &mut self.attachments, "side-attachments", true);
            }
            let rect = ui.available_rect_before_wrap();
            let top = rect.top() + composer::GAP;
            let input_id = Id::unique(("side-composer", id));
            self.recall(ui, input_id);
            let enter = widgets::enter_pressed(ui, input_id);
            let (response, box_rect) = self.message_box(ui, t, Rangef::new(rect.left() + composer::GAP + composer::BTN + 8.0, rect.right() - composer::GAP), top, input_id);
            if std::mem::take(&mut self.side_focus) { response.request_focus(); }
            if response.has_focus() { self.side_active = true; }
            let bottom = box_rect.bottom();
            let buttons = Rect::from_min_size(egui::pos2(rect.left() + composer::GAP, bottom - composer::COMPOSER_H), vec2(composer::BTN, composer::COMPOSER_H));
            if self.composer_buttons(ui, t, buttons) { self.pick_files(); }
            ui.expand_to_include_rect(box_rect.expand(composer::GAP));
            if enter && (!self.input.trim().is_empty() || !self.attachments.is_empty()) {
                self.send_composed(ui.ctx());
            }
        });
        let action = egui::CentralPanel::default().frame(plain).show(ui, |ui| self.screen_view(ui, t)).inner;
        self.chat_actions(ui.ctx(), id, action);
        let side_text = std::mem::replace(&mut self.input, main_input);
        let side_files = std::mem::replace(&mut self.attachments, main_files);
        self.drafts.insert(id.into(), (side_text, side_files));
        std::mem::swap(&mut self.hint, &mut self.side_hint);
        std::mem::swap(&mut self.editing, &mut self.side_editing);
        self.draft_key = main_key;
        self.screen = main_screen;
        if delete { self.delete_conversation(id); }
    }
}

/// A trash can beside the close X, using the toolbar's stroke weight.
fn paint_delete(p: &egui::Painter, r: Rect, color: Color32) {
    let c = r.center();
    let s = Stroke::new(1.3, color);
    p.line_segment([c + vec2(-7.0, -4.0), c + vec2(7.0, -4.0)], s);
    p.rect_stroke(Rect::from_center_size(c + vec2(0.0, -6.0), vec2(6.0, 4.0)), 1.0, s, egui::StrokeKind::Inside);
    p.add(egui::Shape::line(vec![c + vec2(-5.0, -3.0), c + vec2(-4.0, 7.0), c + vec2(4.0, 7.0), c + vec2(5.0, -3.0)], s));
    for x in [-1.5, 1.5] { p.line_segment([c + vec2(x, -1.0), c + vec2(x, 4.5)], s); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fork_keeps_context_but_not_work_or_ownership() {
        let source = Conversation {
            id: "source".into(),
            title: "Original".into(),
            folder: "/tmp/project".into(),
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            role: "main".into(),
            messages: vec![
                json!({"role": "user", "content": [oak::text("Build the project")]}),
                json!({"role": "assistant", "content": [{"type": "tool_call", "id": "running", "name": "EXECUTE", "input": {"command": "build"}}]}),
            ].into(),
            notes: vec![store::Note { id: 7, text: "Keep this context".into(), rewrite: None }],
            next_note_id: 8,
            rolling: rolling::State { summary: Some(rolling::Summary { cut: 1, text: "Earlier context".into(), made_at: 123, clear: 1 }), ..Default::default() },
            prefix: prefix::State { kits: vec!["AGENT".into()], ..Default::default() },
            heartbeats: vec![store::Heartbeat { id: 1, message: "Continue".into(), every_min: 1, next_at: 0 }],
            agents: vec![store::AgentRef { id: 1, conv: "source-agent".into(), name: "Worker".into() }],
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
        };
        let before = serde_json::to_value(&source).unwrap();
        let mut c = fork(&source, "side-test".into(), 456);
        assert_eq!(serde_json::to_value(&source).unwrap(), before);
        assert_eq!(c.messages[..2], source.messages);
        assert_eq!(c.messages[2]["content"][0]["call_id"], "running");
        assert_eq!(c.messages.last().unwrap()["content"][0]["text"], FORK_MESSAGE);
        assert_eq!((&c.folder, &c.model, &c.effort, &c.role), (&source.folder, &source.model, &source.effort, &source.role));
        assert_eq!(c.side_chat_of.as_deref(), Some("source"));
        assert_eq!(c.notes, source.notes);
        assert_eq!(c.next_note_id, 8);
        assert_eq!(c.prefix, source.prefix);
        assert_eq!(serde_json::to_value(&c.rolling).unwrap(), serde_json::to_value(&source.rolling).unwrap());
        assert!(c.parent.is_none() && !c.spawner && c.agents.is_empty());
        assert!(c.goal.is_empty() && c.heartbeats.is_empty() && c.meters.is_empty());
        assert!(c.inbox.is_empty() && c.held.is_empty() && c.limit_wait.is_none());
        assert!(!c.paused && c.scratch.is_empty() && c.next_task_id == 0);
        assert_eq!(c.tokens, store::Tokens::default());
        assert_eq!(c.spent, 0.0);
        c.messages.push(json!({"role": "assistant", "content": [oak::text("Independent")]}));
        let loaded: Conversation = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        assert_eq!(loaded.side_chat_of, c.side_chat_of);
        assert_eq!(loaded.messages, c.messages);
        assert_eq!(source.messages.len(), 2);
        let legacy: Conversation = serde_json::from_value(before).unwrap();
        assert!(legacy.side_chat_of.is_none());
    }

    #[test]
    fn docked_controls_keep_drafts_pause_only_the_fork_and_confirm_deletion() {
        use crate::app_hover_test::{click, key, painted, rect_of, run_frame, test_ctx};
        let (ctx, t) = test_ctx(false, true);
        let source = format!("unit-side-panel-{:016x}", rand::random::<u64>());
        let mut app = App { screen: Screen::Chat(source.clone()), page: Page::Chats, input: "Main draft".into(), draft_key: source.clone(), ..Default::default() };
        app.settings.tabs = vec![source.clone()];
        app.sessions.insert(source.clone(), Session::start(Conversation { id: source.clone(), title: "Original".into(), folder: std::env::temp_dir(), ..Default::default() }, ctx.clone()));
        app.open_side_chat(&source, &ctx);
        let side = app.side_chat.clone().unwrap();
        let mut time = 0.0;
        let mut frame = |app: &mut App, events| run_frame(&ctx, vec2(1400.0, 800.0), &mut time, 0.05, events, |ui| {
            egui::Panel::top("test-tabs").exact_size(TABS_H).show(ui, |ui| app.tab_bar(ui, &t));
            app.conversation_pane(ui, &t);
            egui::Panel::bottom("test-composer").show(ui, |ui| app.composer(ui, &t));
            egui::CentralPanel::default().show(ui, |ui| { app.screen_view(ui, &t); });
        });
        for _ in 0..3 { frame(&mut app, vec![]); }
        let out = frame(&mut app, vec![egui::Event::Text("Side draft".into())]);
        assert_eq!(out.viewport_output.len(), 1, "no native side window");
        assert_eq!(app.drafts[&side].0, "Side draft");
        assert_eq!(app.input, "Main draft");
        let input = ctx.read_response(Id::unique(("side-composer", &side))).unwrap().rect;
        let main = ctx.read_response(Id::unique("composer")).unwrap().rect;
        assert!(input.left() > main.right(), "side composer is docked on the right");
        let image = egui::ColorImage::filled([4, 4], Color32::WHITE);
        frame(&mut app, vec![egui::Event::PasteImage(Arc::new(image))]);
        assert_eq!(app.drafts[&side].1.len(), 1);
        assert!(app.attachments.is_empty(), "paste attaches only to the focused pane");
        for _ in 0..2 { frame(&mut app, vec![]); }
        let input = ctx.read_response(Id::unique(("side-composer", &side))).unwrap().rect;
        frame(&mut app, click(egui::pos2(input.left() - 23.0, input.bottom() - composer::COMPOSER_H + 48.0), egui::PointerButton::Primary));
        assert!(app.sessions[&side].pause_on());
        assert!(!app.sessions[&source].pause_on());
        frame(&mut app, vec![key(Key::F, Modifiers::COMMAND)]);
        assert!(ctx.data(|d| d.get_temp::<find::Find>(Id::unique(("find", &side)))).unwrap().open);
        assert!(!ctx.data(|d| d.get_temp::<find::Find>(Id::unique(("find", &source)))).unwrap().open);
        frame(&mut app, click(main.center(), egui::PointerButton::Primary));
        frame(&mut app, vec![key(Key::F, Modifiers::COMMAND)]);
        assert!(ctx.data(|d| d.get_temp::<find::Find>(Id::unique(("find", &source)))).unwrap().open);
        assert_ne!(ctx.read_response(Id::unique(("find-field", &source))).unwrap().rect, ctx.read_response(Id::unique(("find-field", &side))).unwrap().rect);
        let delete = ctx.read_response(Id::unique(("side-delete", &side))).unwrap().rect;
        let close = ctx.read_response(Id::unique(("side-close", &side))).unwrap().rect;
        assert_eq!(delete.right(), close.left(), "delete is immediately beside close");
        frame(&mut app, click(delete.center(), egui::PointerButton::Primary));
        assert!(app.side_deleting && app.sessions.contains_key(&side));
        let out = frame(&mut app, vec![]);
        let cancel = rect_of(&painted(&out), "Cancel");
        frame(&mut app, click(cancel.center(), egui::PointerButton::Primary));
        assert!(!app.side_deleting && app.sessions.contains_key(&side));
        // (The closed dialog holds the pointer for one more frame.)
        frame(&mut app, vec![]);
        frame(&mut app, click(close.center(), egui::PointerButton::Primary));
        assert!(app.side_chat.is_none() && app.sessions.contains_key(&side));
        app.open_side_chat(&source, &ctx);
        frame(&mut app, vec![key(Key::W, Modifiers::COMMAND)]);
        assert!(app.side_chat.is_none());
        assert_eq!(app.settings.tabs, vec![source.clone()], "Ctrl+W closes the focused side pane, not the source tab");
        assert_eq!(app.drafts[&side].0, "Side draft");
        app.open_side_chat(&source, &ctx);
        for _ in 0..2 { frame(&mut app, vec![]); }
        frame(&mut app, click(delete.center(), egui::PointerButton::Primary));
        let out = frame(&mut app, vec![]);
        let confirm = rect_of(&painted(&out), "Delete");
        frame(&mut app, click(confirm.center(), egui::PointerButton::Primary));
        assert!(!app.sessions.contains_key(&side) && app.sessions.contains_key(&source));
        assert_eq!(app.input, "Main draft");
        app.delete_conversation(&source);
    }

    #[test]
    fn reopening_reuses_session_and_drafts_and_deletion_cascades() {
        use crate::app_hover_test::{run_frame, test_ctx};
        let (ctx, t) = test_ctx(false, true);
        let source = format!("unit-side-source-{:016x}", rand::random::<u64>());
        let mut app = App { screen: Screen::Chat(source.clone()), page: Page::Chats, input: "Main draft".into(), draft_key: source.clone(), ..Default::default() };
        app.sessions.insert(source.clone(), Session::start(Conversation { id: source.clone(), title: "Original".into(), folder: std::env::temp_dir(), ..Default::default() }, ctx.clone()));
        app.open_side_chat(&source, &ctx);
        let side = app.side_chat.clone().unwrap();
        let original_messages = app.sessions[&side].conv.lock().unwrap().messages.clone();
        assert_eq!(original_messages.last().unwrap()["content"][0]["text"], FORK_MESSAGE);
        app.drafts.insert(side.clone(), ("Side draft".into(), vec![]));
        let mut time = 0.0;
        for _ in 0..3 {
            run_frame(&ctx, vec2(1000.0, 800.0), &mut time, 0.05, vec![], |ui| app.conversation_pane(ui, &t));
        }
        assert_eq!(app.input, "Main draft");
        assert_eq!(app.draft_key, source);
        assert_eq!(app.drafts[&side].0, "Side draft");
        assert!(matches!(&app.screen, Screen::Chat(id) if id == &source));
        app.side_chat = None;
        app.open_side_chat(&source, &ctx);
        assert_eq!(app.side_chat.as_deref(), Some(side.as_str()));
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[&side].conv.lock().unwrap().messages, original_messages);
        assert!(!app.sessions[&side].active(), "opening must not continue the source's work");
        let saved = store::load_conversations().into_iter().find(|c| c.id == side).expect("persisted fork");
        assert_eq!(saved.side_chat_of.as_deref(), Some(source.as_str()));
        app.delete_conversation(&side);
        assert!(app.sessions.contains_key(&source));
        assert!(app.side_chat.is_none() && !app.drafts.contains_key(&side));
        app.open_side_chat(&source, &ctx);
        let new_side = app.side_chat.clone().unwrap();
        assert_ne!(new_side, side);
        app.delete_conversation(&source);
        assert!(app.sessions.is_empty());
        assert!(!store::load_conversations().iter().any(|c| c.id == side || c.id == new_side || c.id == source));
    }
}
