//! The composer row along a conversation's bottom: the message box and its
//! attachments, the buttons left of it (attach, pause, ultrapause), the
//! Settings link, the Unslop box between them, and the boxes right of it:
//! smart context and the account's usage.

use super::*;
pub(crate) use oak_sdk::ui::{GAP, chat_column, kv, meter, paint_spinner, transcript_x};
use crate::widgets::{chevron_v, clip_width, draw_x, enter_pressed, icon_button, paint_pause, sending_edit, switch};
use crate::theme::mix;

impl App {
    pub(crate) fn composer(&mut self, ui: &mut Ui, t: &Theme) {
        let area = ui.max_rect();
        let lay = ComposerLayout::new(area.x_range(), matches!(self.screen, Screen::Chat(_)));
        // Notices and attachments above the row, as wide as the message
        // box; without any, the row sits GAP below the panel's top line.
        let mut y = area.top() + GAP;
        let notices = |y: f32| Rect::from_x_y_ranges(lay.chat, y..=area.bottom());
        let session = chat_session(&self.screen, &self.sessions);
        let r = ui.scope_builder(UiBuilder::new().max_rect(notices(y)), |ui| composer_notices(ui, t, session, self.hint.as_deref()));
        if r.response.rect.height() > 0.5 {
            y = r.response.rect.bottom() + 6.0;
        }

        let id = Id::unique("composer");
        let enter = enter_pressed(ui, id);
        self.recall(ui, id);
        if !self.side_chat_focused() {
            take_drops(ui.ctx(), &mut self.attachments, &mut self.hint);
        }
        if !self.attachments.is_empty() {
            y = ui.scope_builder(UiBuilder::new().max_rect(notices(y)), |ui| {
                attachment_strip(ui, t, &mut self.attachments, "attachment-x", true);
                ui.add_space(4.0);
            }).response.rect.bottom() + 2.0;
        }

        // The row: the buttons left of the transcript's column, the message
        // box across it, and smart context and the account filling the
        // rest, flush with the window's edge. The message box grows with
        // its text, upwards (the panel is pinned to the window's bottom),
        // up to `composer_max_h`; past that it scrolls. The buttons and
        // boxes stay level with its bottom.
        let top = y;
        let (resp, box_rect) = self.message_box(ui, t, lay.chat, top, id);
        if resp.has_focus() { self.side_active = false; }
        let bottom = box_rect.bottom().max(top + COMPOSER_H);
        let row = |x: Rangef| Rect::from_x_y_ranges(x, bottom - COMPOSER_H..=bottom);
        let pick = self.composer_buttons(ui, t, row(Rangef::new(lay.buttons, lay.buttons + BTN)));
        self.corner_links(ui, t, area.left(), lay.unslop.map_or(lay.buttons, |u| u.min) - 8.0, bottom);
        // (A cloud session or a plugin's tab runs elsewhere: none of these are its.)
        let cloudy = self.cloud_screen() || self.plugin_screen();
        if !cloudy {
            self.side_boxes(ui, t, &lay, row);
        }
        ui.expand_to_include_rect(Rect::from_x_y_ranges(area.x_range(), area.top()..=bottom + GAP));
        if pick {
            self.pick_files();
        }
        // (Only when nothing else has the keyboard: the rolling sizes, the
        // folder or a key being typed keep it.)
        if matches!(self.screen, Screen::New) && ui.memory(|m| m.focused().is_none()) && self.input.is_empty() && self.goal_edit.is_none() {
            resp.request_focus();
        }
        if enter && (!self.input.trim().is_empty() || !self.attachments.is_empty()) {
            self.send_composed(ui.ctx());
        }
        drop_overlay(ui, t, Rect::from_x_y_ranges(area.x_range(), area.top()..=ui.min_rect().bottom()), "Drop files to attach them");
    }

    /// The message box (`id`) across `x` from `top`, growing with its text
    /// up to `composer_max_h` and scrolling past that. Gives its text box
    /// and where the box is.
    pub(crate) fn message_box(&mut self, ui: &mut Ui, t: &Theme, x: Rangef, top: f32, id: Id) -> (egui::Response, Rect) {
        let max_h = composer_max_h(ui.ctx());
        let focused = ui.memory(|m| m.has_focus(id));
        // Count what actually arrives in the composer: the CLI reports a
        // per-prompt input profile built from these numbers.
        if focused {
            ui.input(|i| {
                for e in &i.events {
                    match e {
                        egui::Event::Text(t) => crate::freebuff::note_typed(t.chars().count()),
                        egui::Event::Paste(t) => crate::freebuff::note_paste(t.chars().count()),
                        _ => {}
                    }
                }
            });
        }
        let r = ui.scope_builder(UiBuilder::new().max_rect(Rect::from_x_y_ranges(x, top..=top + max_h)), |ui| {
            let v = ui.visuals();
            let stroke = if focused { v.selection.stroke } else { v.widgets.inactive.bg_stroke };
            egui::Frame::new().fill(t.panel).stroke(stroke).show(ui, |ui| {
                let w = ui.available_width();
                ScrollArea::vertical()
                    .id_salt("composer-scroll")
                    .max_height(max_h - 2.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        crate::text_menu::add(
                            ui,
                            sending_edit(&mut self.input, id)
                                .frame(egui::Frame::NONE.inner_margin(Margin::same(10)))
                                .min_size(vec2(w, COMPOSER_H - 2.0))
                                .desired_width(w)
                                .hint_text("Type message"),
                        )
                    })
                    .inner
            })
        });
        (r.inner.inner, r.inner.response.rect)
    }

    /// The buttons left of the message box, in `rect`: attach, and for a
    /// conversation pause and ultrapause (each takes the session's
    /// subagents along; `pausing.rs`), which are done here. True when attach
    /// was clicked.
    pub(crate) fn composer_buttons(&mut self, ui: &mut Ui, t: &Theme, rect: Rect) -> bool {
        let session = chat_session(&self.screen, &self.sessions);
        let mut pick = false;
        let mut pause_action = None;
        let spinning = session.is_some_and(|s| s.pause_on() && (s.pausing() || self.agents_pausing(s)));
        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            // Three buttons fit the message box's height a little smaller.
            let small = session.is_some();
            pick = attach_button(ui, t, small);
            let Some(s) = session else { return };
            let paused = s.conv.lock().unwrap().paused;
            // One button, one thing: pause waits, ultrapause doesn't. A
            // pause spins until it (and its subagents' pauses) take
            // hold; a click meanwhile resumes.
            let on = s.pause_on();
            if pause_button(ui, t, on, spinning, small) {
                pause_action = Some(if on { PauseAction::Resume } else { PauseAction::Pause });
            }
            if ultrapause_button(ui, t, !paused, small) {
                pause_action = Some(PauseAction::Ultra);
            }
        });
        if let (Some(a), Screen::Chat(id)) = (pause_action, &self.screen) {
            let id = id.clone();
            match a {
                PauseAction::Pause => self.pause_group(&id),
                PauseAction::Resume => self.resume_group(&id),
                PauseAction::Ultra => self.ultrapause_group(&id),
            }
        }
        pick
    }

    /// Settings, in the bottom left corner from `left` to `link_end` (where
    /// the buttons, or the Unslop box beside them, begin); and right after a
    /// pause took a session's subagents along, all of them paused: a line
    /// right of the Settings link, its bottom on the ultrapause button's.
    fn corner_links(&mut self, ui: &mut Ui, t: &Theme, left: f32, link_end: f32, bottom: f32) {
        let flash = match &self.screen {
            Screen::Chat(id) => self.agents_paused_flash(ui.ctx(), id),
            Screen::New => None,
        };
        let mut links_right = left + GAP;
        let room = Rect::from_min_max(egui::pos2(left + GAP, bottom - 24.0), egui::pos2(link_end, bottom));
        if room.width() >= GEAR_W - 0.5 {
            let (link, clicked) = settings_link(ui, t, room);
            links_right = link.right();
            if clicked {
                self.settings_from = Some(self.page);
                self.page = Page::Settings;
            }
        }
        let Some(alpha) = flash else { return };
        let font = FontId::proportional(12.0);
        // (It grows left from the buttons, or from the Unslop box beside
        // them, and shows only when it clears the Settings link.)
        let right = link_end;
        let galley = ui.painter().layout_no_wrap("all subagents paused".into(), font, t.ink3.gamma_multiply(alpha));
        if right - galley.size().x >= links_right + 10.0 {
            // (+1: the descenders' last pixel row on the button's.)
            let at = egui::pos2(right - galley.size().x, bottom - galley.size().y + 1.0);
            ui.painter().galley(at, galley, t.ink3);
        }
    }

    /// The boxes along the row, each in `row` of its span: Unslop left of
    /// the buttons, and smart context and the account right of the message
    /// box.
    fn side_boxes(&mut self, ui: &mut Ui, t: &Theme, lay: &ComposerLayout, row: impl Fn(Rangef) -> Rect) {
        let session = chat_session(&self.screen, &self.sessions);
        if let Some(x) = lay.unslop {
            ui.scope_builder(UiBuilder::new().max_rect(row(x)), |ui| {
                unslop_panel(ui, t, &mut self.settings, x.span());
            });
        }
        if let Some(&x) = lay.boxes.first() {
            ui.scope_builder(UiBuilder::new().max_rect(row(x)), |ui| {
                rolling_panel(ui, t, session, &mut self.settings, &mut self.size_inputs, x.span());
            });
        }
        if let Some(&x) = lay.boxes.get(1) {
            ui.scope_builder(UiBuilder::new().max_rect(row(x)), |ui| account_box(ui, t, session, &self.settings, x.span()));
            if ui.ctx().data_mut(|d| d.remove_temp::<bool>(Id::unique(GO_HOME))).is_some() {
                self.page = Page::Home;
            }
        }
    }

    /// Sends what is in the message box to where it goes: a new
    /// conversation or cloud session (or what a plugin starts instead), a
    /// plugin's tab, a cloud session or this conversation.
    pub(crate) fn send_composed(&mut self, ctx: &egui::Context) {
        self.send_input(|app, text, images| {
            // The CLI seals this prompt's typing profile here; the
            // request that sends the text reports it once.
            crate::freebuff::seal_input_profile(&text);
            let shown = match &app.screen {
                Screen::New => None,
                Screen::Chat(id) => Some(id.clone()),
            };
            match shown {
                None => match app.new_kind() {
                    Some(kind) => {
                        if let Err(why) = kind.start(app, text, images) {
                            app.hint = Some(why);
                        }
                    }
                    None if app.new_cloud => app.start_cloud(text, images, ctx),
                    None => app.start_conversation(text, images, None, ctx),
                },
                Some(id) => match crate::plugins::tab_kind(&id) {
                    Some(kind) => {
                        if let Err(why) = kind.send(app, &id, text, images) {
                            app.hint = Some(why);
                        }
                    }
                    None if cloud::is_key(&id) => cloud::send(&id, text, images, ctx),
                    None => app.sessions[&id].send(text, images),
                },
            }
        });
    }

    /// Sends the message box's text (trimmed) and attachments with `send`;
    /// they are cleared unless it left a hint (it couldn't send).
    pub(crate) fn send_input(&mut self, send: impl FnOnce(&mut App, String, Vec<Value>)) {
        self.hint = None;
        let text = self.input.trim().to_string();
        let images = self.attachments.clone();
        let key = self.draft_key.clone();
        send(self, text.clone(), images);
        if self.hint.is_none() {
            self.input.clear();
            self.attachments.clear();
            if !text.is_empty() {
                self.last_sent.insert(key, text);
            }
        }
    }

    /// Up in the message box `id` while it has the keyboard and is empty
    /// (no text; attachments stay as they are) brings back the last message
    /// sent from this tab, the cursor at its end, to edit or send again. It
    /// sends nothing and changes no history; with text in the box, Up moves
    /// the cursor as usual.
    pub(crate) fn recall(&mut self, ui: &Ui, id: Id) {
        let up = |e: &egui::Event| matches!(e, egui::Event::Key { key: Key::ArrowUp, pressed: true, modifiers, .. } if modifiers.is_none());
        if !self.input.is_empty() || !ui.memory(|m| m.has_focus(id)) || !ui.input(|i| i.events.iter().any(up)) {
            return;
        }
        let Some(text) = self.last_sent.get(&self.draft_key).cloned().or_else(|| self.last_sent_in_conversation()) else { return };
        // (Taken, so the box doesn't see it too.)
        ui.input_mut(|i| i.events.retain(|e| !up(e)));
        let end = egui::text::CCursor::new(text.chars().count());
        self.input = text;
        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
        state.store(ui.ctx(), id);
    }

    /// The text of the user's last message in the open conversation (one of
    /// this Oak's, after a restart, when nothing was sent from its tab yet).
    fn last_sent_in_conversation(&self) -> Option<String> {
        let Screen::Chat(id) = &self.screen else { return None };
        let c = self.sessions.get(id)?.conv.lock().unwrap();
        last_user_text(&c.messages)
    }

    /// Asks for files to attach: any kind.
    pub(crate) fn pick_files(&mut self) {
        let Some(files) = rfd::FileDialog::new().pick_files() else { return };
        for f in files {
            attach_file(&mut self.attachments, &mut self.hint, &f);
        }
    }
}

/// Images pasted (Ctrl+V of a screenshot) or files dropped on the window,
/// added to `files` (`hint` says why one couldn't be).
pub(crate) fn take_drops(ctx: &egui::Context, files: &mut Vec<Value>, hint: &mut Option<String>) {
    let (pasted, dropped) = ctx.input(|i| {
        let pasted: Vec<_> = i.events.iter().filter_map(|e| if let egui::Event::PasteImage(img) = e { Some(img.clone()) } else { None }).collect();
        (pasted, i.raw.dropped_files.clone())
    });
    for img in pasted {
        match images::from_color_image(&img) {
            Ok(i) => files.push(i.block()),
            Err(e) => *hint = Some(format!("Couldn't attach the pasted image: {e}")),
        }
    }
    for f in dropped {
        match f.bytes() {
            Ok(b) => attach(files, hint, f.path(), Ok(b.to_vec())),
            Err(_) => attach_file(files, hint, f.path()),
        }
    }
}

/// Attaches the file at `path` (its bytes, or why they couldn't be read).
fn attach(files: &mut Vec<Value>, hint: &mut Option<String>, path: &std::path::Path, bytes: Result<Vec<u8>, String>) {
    match bytes.and_then(|b| file_block(path, &b)) {
        Ok(b) => files.push(b),
        Err(e) => *hint = Some(format!("Couldn't attach {}: {e}", path.display())),
    }
}

/// Reads the file at `path` and attaches it.
pub(crate) fn attach_file(files: &mut Vec<Value>, hint: &mut Option<String>, path: &std::path::Path) {
    attach(files, hint, path, std::fs::read(path).map_err(|e| e.to_string()));
}

/// The files attached so far, removable: images as thumbnails, other
/// files by name (`wrap`: over more lines when they don't fit one).
pub(crate) fn attachment_strip(ui: &mut Ui, t: &Theme, files: &mut Vec<Value>, salt: &'static str, wrap: bool) {
    let mut remove = None;
    let row = |ui: &mut Ui| {
        for (i, block) in files.iter().enumerate() {
            if let Some(r) = attachment_view(ui, t, block, 56.0)
                && ui.rect_contains_pointer(r.rect)
            {
                let x = Rect::from_min_size(egui::pos2(r.rect.right() - 18.0, r.rect.top() + 2.0), vec2(16.0, 16.0));
                if icon_button(ui, t, x, Id::unique((salt, i)), "Remove", draw_x) {
                    remove = Some(i);
                }
            }
        }
    };
    if wrap {
        ui.horizontal_wrapped(row);
    } else {
        ui.horizontal(row);
    }
    if let Some(i) = remove {
        files.remove(i);
    }
}

/// While files are dragged over the window: `r` (less a margin) shaded,
/// saying `text`.
pub(crate) fn drop_overlay(ui: &Ui, t: &Theme, r: Rect, text: &str) {
    if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
        let r = r.shrink(4.0);
        ui.painter().rect_filled(r, 0.0, t.bg.gamma_multiply(0.85));
        ui.painter().rect_stroke(r, 0.0, Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), t.accent);
    }
}

/// The text of the last message the user wrote in `messages` (not a tool
/// result, a note or anything else Oak added; `transcript::user_authored`).
pub(crate) fn last_user_text(messages: &[Value]) -> Option<String> {
    let blocks = messages.iter().rev().filter(|m| m["role"] == "user").filter_map(|m| m["content"].as_array());
    blocks.flat_map(|b| b.iter().rev()).find_map(|b| {
        let text = b["text"].as_str().filter(|t| !t.trim().is_empty())?;
        (b["type"] == "text" && b.get("_oak_file").is_none() && crate::transcript::user_authored(b)).then(|| text.to_string())
    })
}

/// The open conversation (None on a new one).
fn chat_session<'a>(screen: &Screen, sessions: &'a HashMap<String, Session>) -> Option<&'a Session> {
    match screen {
        Screen::Chat(id) => sessions.get(id),
        Screen::New => None,
    }
}

/// The composer row's height: the message box at four lines.
pub(crate) const COMPOSER_H: f32 = 96.0;
/// The tallest the message box grows (it scrolls past that): 300 points,
/// or less in a short window, so the transcript keeps most of it.
pub(crate) fn composer_max_h(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().height() * 0.4).clamp(COMPOSER_H, 300.0)
}
/// The message box gets at least this before side sections are dropped.
const MIN_INPUT_W: f32 = 220.0;
/// The narrowest the composer row's boxes get before one is dropped.
const MIN_BOX_W: f32 = 180.0;
/// The narrowest a box right of it (smart context, usage) gets; the
/// width account boxes are made at on their own.
pub(crate) const BOX_W: f32 = 200.0;
/// The buttons left of the message box.
pub(crate) const BTN: f32 = 30.0;

/// Over the message box: the messages queued (with Interrupt), the
/// session's error, and why the last message couldn't go.
fn composer_notices(ui: &mut Ui, t: &Theme, session: Option<&Session>, hint: Option<&str>) {
    if let Some(s) = session {
        let l = s.live.lock().unwrap();
        let n = l.ready_count();
        let asking = l.ask.is_some();
        if l.busy && n > 0 {
            egui::Frame::new().fill(t.panel2).stroke(Stroke::new(1.0, t.accent)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let next = if asking { "awaiting answers" } else { "next request" };
                    ui.label(format!("{n} message{} queued, {next}", crate::plural(n)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(Button::new(RichText::new("Interrupt").color(t.accent))).clicked() {
                            s.interrupt();
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }
        if let Some(e) = &l.error {
            ui.label(RichText::new(e).color(t.error));
        }
    }
    if let Some(h) = hint {
        ui.label(RichText::new(h).color(t.error));
    }
}

/// A chat drawn as widgets (a bot's, Oak's) in the conversation's column
/// of `ui`'s page, kept scrolled to the newest: `f` lays it out top down,
/// its rows `gap` apart, under a 16-point margin. A middle click scrolls it
/// (`autoscroll`).
pub(crate) fn chat_scroll(ui: &mut Ui, t: &Theme, salt: impl std::hash::Hash + std::fmt::Debug, gap: f32, f: impl FnOnce(&mut Ui)) {
    let area = ui.max_rect();
    let col = chat_column(area.x_range());
    let id = ui.make_persistent_id(&salt);
    // Middle-click scrolling measured last frame, set directly (an animated
    // scroll would leave it stuck to the bottom).
    let auto = ui.data_mut(|d| d.remove_temp::<f32>(id.with("autoscroll"))).filter(|dy| *dy != 0.0);
    let out = ScrollArea::vertical().id_salt(salt).stick_to_bottom(true).animated(auto.is_none()).auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.add_space((col.min - area.left()).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(col.span());
                ui.spacing_mut().item_spacing.y = gap;
                ui.add_space(16.0);
                f(ui);
            });
        });
        // After the scroll areas inside it took theirs.
        if let Some(dy) = auto {
            ui.scroll_with_delta(vec2(0.0, -dy));
        }
    });
    let scrollable = out.content_size.y > out.inner_rect.height() + 0.5;
    if let Some(dy) = crate::autoscroll::scroll(ui, t, id.value(), out.inner_rect, scrollable) {
        ui.data_mut(|d| d.insert_temp(id.with("autoscroll"), dy));
    }
}

/// The Settings gear's width, when there is no room for its label.
pub(crate) const GEAR_W: f32 = 18.0;

/// The Settings link's width with its label (about; `settings_link`
/// measures it).
const SETTINGS_W: f32 = 86.0;

/// Where the composer row's parts go across the panel.
pub(crate) struct ComposerLayout {
    /// The button column's left edge.
    pub(crate) buttons: f32,
    /// The message box: the transcript's column, unless the boxes need
    /// some of it.
    pub(crate) chat: Rangef,
    /// The Unslop box, left of the buttons (up to `BOX_W` wide, against
    /// them), when there is room.
    pub(crate) unslop: Option<Rangef>,
    /// Smart context, then the account (dropped in that order from the
    /// right when the window gets narrow).
    boxes: Vec<Rangef>,
}

impl ComposerLayout {
    /// `boxed`: a conversation, whose session box takes the room left of
    /// the transcript (`chat_column`).
    pub(crate) fn new(area: Rangef, boxed: bool) -> ComposerLayout {
        let col = if boxed { chat_column(area) } else { transcript_x(area) };
        let (left, right) = (area.min + GAP, area.max - GAP);
        // What goes when the window gets narrow, in order: Settings' label,
        // the account box, the Unslop box, smart context. However narrow,
        // the corner keeps room for the Settings gear: it is the only way
        // to Settings.
        let tries = [(true, SETTINGS_W, 2usize), (true, GEAR_W, 2), (true, GEAR_W, 1), (false, GEAR_W, 1)];
        for (unslop, link, n) in tries {
            let lead = left + link + 8.0 + if unslop { MIN_BOX_W + 8.0 } else { 0.0 } + BTN + 8.0;
            let chat_left = col.min.max(lead);
            let chat_right = col.max.min(right - n as f32 * (MIN_BOX_W + 8.0));
            if chat_right - chat_left < MIN_INPUT_W {
                continue;
            }
            let buttons = chat_left - 8.0 - BTN;
            let x0 = chat_right + 8.0;
            let w = (right - x0 - 8.0 * (n - 1) as f32) / n as f32;
            let boxes = (0..n).map(|i| Rangef::new(x0 + i as f32 * (w + 8.0), x0 + i as f32 * (w + 8.0) + w)).collect();
            let unslop = unslop.then(|| Rangef::new((buttons - 8.0 - BOX_W).max(left + link + 8.0), buttons - 8.0));
            return ComposerLayout { buttons, chat: Rangef::new(chat_left, chat_right), unslop, boxes };
        }
        let chat_left = col.min.max(left + GEAR_W + 8.0 + BTN + 8.0);
        let chat = Rangef::new(chat_left, col.max.min(right).max(chat_left + 40.0));
        ComposerLayout { buttons: chat_left - 8.0 - BTN, chat, unslop: None, boxes: vec![] }
    }
}

/// The square "+" left of the message box. True when clicked.
pub(crate) fn attach_button(ui: &mut Ui, t: &Theme, small: bool) -> bool {
    square_button(ui, t, small, "Attach files", |p, r, color| {
        let c = r.center();
        p.line_segment([c - vec2(6.0, 0.0), c + vec2(6.0, 0.0)], Stroke::new(1.5, color));
        p.line_segment([c - vec2(0.0, 6.0), c + vec2(0.0, 6.0)], Stroke::new(1.5, color));
    })
}

/// What the pause and ultrapause buttons were clicked for.
enum PauseAction {
    Pause,
    Resume,
    Ultra,
}

/// Pause (or, paused, resume) the session; under the attach button. While
/// the pause is taking hold (`spinning`), a spinner; a click resumes.
fn pause_button(ui: &mut Ui, t: &Theme, paused: bool, spinning: bool, small: bool) -> bool {
    if spinning {
        square_button(ui, t, small, "Resume", |p, r, _| paint_spinner(p, Rect::from_center_size(r.center(), vec2(14.0, 14.0)), t.accent))
    } else if paused {
        square_button(ui, t, small, "Resume", |p, r, _| {
            let c = r.center();
            p.add(egui::Shape::convex_polygon(
                vec![c + vec2(-4.0, -6.0), c + vec2(6.0, 0.0), c + vec2(-4.0, 6.0)],
                t.accent,
                Stroke::NONE,
            ));
        })
    } else {
        square_button(
            ui,
            t,
            small,
            "Pause after the current request and running commands finish. Keeps the reply.",
            |p, r, color| paint_pause(p, r.shrink(8.0), color),
        )
    }
}

/// Ultrapause: under the pause button, it stops everything at once. The
/// reply streaming in is dropped (and asked for again on resume), running
/// commands and background tasks are killed, and the session pauses.
/// Greyed out while paused; red under the pointer (it kills things).
fn ultrapause_button(ui: &mut Ui, t: &Theme, active: bool, small: bool) -> bool {
    let tip = if active {
        "Ultrapause: kills running commands and tasks, discards the current reply, and pauses. Resume retries the reply."
    } else {
        "Ultrapause unavailable while paused"
    };
    let look = if active { Look::Danger } else { Look::Off };
    square_button_as(ui, t, small, look, tip, |p, r, color| paint_ultrapause(p, r.shrink(8.0), color))
}

/// The ultrapause sign: three bars as tall as the pause sign's
/// (`paint_pause`), thinner, spanning the plus sign's 12 points.
fn paint_ultrapause(p: &egui::Painter, r: Rect, color: Color32) {
    let (w, gap, h) = (2.0, 3.0, r.height() * 0.7);
    let (left, top) = (r.center().x - (3.0 * w + 2.0 * gap) / 2.0, r.center().y - h / 2.0);
    for i in 0..3 {
        let x = left + i as f32 * (w + gap);
        p.rect_filled(Rect::from_min_size(egui::pos2(x, top), vec2(w, h)), 1.0, color);
    }
}

/// Nerd Font: a gear.
pub(crate) const GEAR_ICON: &str = "\u{f013}";

/// The gear and "Settings" in the composer's bottom left corner, in the
/// style of the old terminal link (a plain link: icon, label and a "^",
/// ink3, brighter under the pointer). `room` is the space it may take,
/// its bottom left corner where it goes; with too little room for the
/// label, just the gear. Returns the link's rect when drawn and whether it
/// was clicked.
pub(crate) fn settings_link(ui: &mut Ui, t: &Theme, room: Rect) -> (Rect, bool) {
    let font = FontId::proportional(12.5);
    let icons = markdown::icon_font(13.0);
    let full = crate::widgets::text_width(ui, "Settings", &font) + 34.0;
    let label = room.width() >= full;
    let w = if label { full } else { GEAR_W };
    let rect = Rect::from_min_size(egui::pos2(room.left(), room.bottom() - 24.0), vec2(w, 24.0));
    let resp = ui.interact(rect, Id::unique("settings-link"), Sense::click());
    let color = if resp.hovered() { t.ink } else { t.ink3 };
    let p = ui.painter();
    p.text(egui::pos2(rect.left() + 7.0, rect.center().y), Align2::CENTER_CENTER, GEAR_ICON, icons, color);
    if label {
        let g = p.text(egui::pos2(rect.left() + 18.0, rect.center().y), Align2::LEFT_CENTER, "Settings", font, color);
        chevron_v(p, egui::pos2(g.right() + 9.0, rect.center().y), true, color);
    }
    (rect, resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Settings").clicked())
}

/// Unslop, in a box like smart context's, left of the buttons: its
/// switch, and under it the switches that work with it on (greyed out with
/// it off). They are for every conversation.
pub(crate) fn unslop_panel(ui: &mut Ui, t: &Theme, settings: &mut Settings, w: f32) -> egui::Response {
    side_box(ui, t, w, |ui| {
        let on = settings.unslop;
        let mut changed = false;
        let tip = "Shows the model's messages rewritten by Gemma 4 31B through OpenRouter, paid from its credits. The model's context is unchanged. Not for subagents.";
        if switch_row(ui, t, "Unslop", t.ink2, tip, on) {
            settings.unslop = !on;
            changed = true;
        }
        ui.add_space(2.0);
        let rows: [(&str, &str, &mut bool); 3] = [
            ("No drafts", "Hides the model's drafts: only the rewrites show", &mut settings.unslop_no_drafts),
            ("Rewrite thinking", "Rewrites thinking summaries as they stream in", &mut settings.unslop_thinking),
            ("Rewrite notes", "Shows the notes rewritten; the model's notes are unchanged", &mut settings.unslop_notes),
        ];
        for (label, tip, value) in rows {
            ui.add_enabled_ui(on, |ui| {
                if switch_row(ui, t, label, t.ink3, tip, *value) {
                    *value = !*value;
                    changed = true;
                }
            });
        }
        if changed {
            unslop::set_all(settings);
            store::save_settings(settings);
        }
    })
    .response
}

/// A box's row: `label` (in `color`, `tip` under the pointer) and a
/// switch at the right. True when the switch is clicked.
fn switch_row(ui: &mut Ui, t: &Theme, label: &str, color: Color32, tip: &str, on: bool) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).small().color(color)).on_hover_text(tip);
        // A little smaller than smart context's (egui fades it in a disabled row).
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| switch(ui, t, on, vec2(26.0, 13.0)).clicked()).inner
    })
    .inner
}

/// A square button (30 px, or 28 `small`) with a painted icon, beside the
/// message box.
pub(crate) fn square_button(ui: &mut Ui, t: &Theme, small: bool, tip: &str, draw: impl FnOnce(&egui::Painter, Rect, Color32)) -> bool {
    square_button_as(ui, t, small, Look::Plain, tip, draw)
}

/// How a square button looks: plain, red under the pointer, or greyed out
/// (it can't be clicked).
#[derive(PartialEq)]
enum Look {
    Plain,
    Danger,
    Off,
}

fn square_button_as(ui: &mut Ui, t: &Theme, small: bool, look: Look, tip: &str, draw: impl FnOnce(&egui::Painter, Rect, Color32)) -> bool {
    let side = if small { 28.0 } else { BTN };
    let on = look != Look::Off;
    let (rect, resp) = ui.allocate_exact_size(vec2(BTN, side), if on { Sense::click() } else { Sense::hover() });
    let hovered = on && resp.hovered();
    // Its fill, border and icon.
    let (fill, border, ink) = match look {
        Look::Off => (t.bg, t.line, t.ink4),
        Look::Danger if hovered => (mix(t.panel, t.error, 0.18), t.error, t.ink),
        Look::Plain if hovered => (t.panel2, t.ink3, t.ink),
        _ => (t.panel, t.line, t.ink2),
    };
    let p = ui.painter();
    p.rect_filled(rect, 0.0, fill);
    p.rect_stroke(rect, 0.0, Stroke::new(1.0, border), egui::StrokeKind::Inside);
    draw(p, rect, ink);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip).clicked()
}

/// The account box, right of smart context: the account the model goes
/// through (the conversation's own, `crate::rotation`, else the one the
/// last request went through), shown as its provider has it. A model
/// through OpenRouter uses no Claude account: its box shows the
/// OpenRouter account, and what this session spent.
fn account_box(ui: &mut Ui, t: &Theme, session: Option<&Session>, settings: &Settings, w: f32) -> egui::Response {
    let conv_id = session.map(|s| s.conv.lock().unwrap().id.clone()).unwrap_or_default();
    let seat = |p: api::Provider| crate::rotation::seat(p, &conv_id);
    let (provider, spent, via) = match session {
        Some(s) => {
            let c = s.conv.lock().unwrap();
            (c.provider(), c.spent, c.served_by.clone())
        }
        None => (settings.provider.unwrap_or_else(|| api::Provider::of(&settings.model)), 0.0, String::new()),
    };
    match provider {
        api::Provider::OpenRouter => {
            let via = (!via.is_empty()).then(|| format!("via {via}"));
            let r = openrouter_panel(ui, t, openrouter::account_of_conv(&conv_id).as_ref(), ("This session", spent), via.as_deref(), w);
            r.on_hover_text("Lowest-cost fast, reliable provider; preserves warm caches. Excludes 4-bit models and prices above DeepSeek’s own.")
        }
        // A Codex model: the ChatGPT plan's Codex limits.
        api::Provider::Codex => {
            let all = codex::accounts();
            let mine = seat(api::Provider::Codex).and_then(|id| all.iter().find(|a| a.0 == id)).or(all.first()).map(|a| &a.1);
            usage_panel(ui, t, mine, None, false, w).on_hover_text("ChatGPT plan, Codex usage")
        }
        // A Freebuff model: the account it goes through.
        api::Provider::Freebuff => {
            let all = freebuff::accounts();
            let mine = seat(api::Provider::Freebuff).and_then(|id| all.iter().find(|a| a.id == id)).or(all.first());
            freebuff_panel(ui, t, mine, None, w).on_hover_text("Freebuff, runs its own agent and system prompt")
        }
        // The user's own server: the tokens used through it.
        api::Provider::Local => local_panel(ui, t, w).on_hover_text("llama.cpp server"),
        // NodeClub: its key, and the tokens used through it.
        api::Provider::NodeClub => nodeclub_panel(ui, t, crate::nodeclub::account_of_conv(&conv_id).as_ref(), None, w).on_hover_text("NodeClub, OpenAI-compatible API"),
        api::Provider::Claude => {
            let accounts = api::accounts();
            let account = seat(api::Provider::Claude).and_then(|id| accounts.iter().find(|a| a.id.to_string() == id)).or_else(|| accounts.iter().find(|a| a.current));
            usage_panel(ui, t, account, api::rotation_status(), false, w)
        }
    }
}

/// A box in the composer row, `w` wide and as tall as the message box.
pub(crate) fn side_box<R>(ui: &mut Ui, t: &Theme, w: f32, f: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    framed_box(ui, t, t.line, w, f)
}

/// `side_box` with its border in `stroke`.
pub(crate) fn framed_box<R>(ui: &mut Ui, t: &Theme, stroke: Color32, w: f32, f: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, stroke)).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
        // The box sits in the composer's row: lay its own rows out top to bottom.
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.set_width(w - 22.0);
            ui.set_height(COMPOSER_H - 16.0);
            // Rows as tall as their small text, not a button's 18px.
            ui.spacing_mut().interact_size.y = 14.0;
            ui.spacing_mut().item_spacing.y = 3.0;
            f(ui)
        })
        .inner
    })
}

/// Set (in the context's memory) by a link that opens Home.
const GO_HOME: &str = "oak-go-home";

/// An account and how much of its plan's limits is used. `status` (the
/// rotation note) sits grey at the right of its first row; `lit` outlines
/// it in the accent colour (the current account, on Home).
pub(crate) fn usage_panel(ui: &mut Ui, t: &Theme, a: Option<&api::Account>, status: Option<&str>, lit: bool, w: f32) -> egui::Response {
    let stroke = if lit { t.accent } else { t.line };
    framed_box(ui, t, stroke, w, |ui| {
        // No account at all (accounts load before the first frame, and
        // connecting fetches the vault's before it returns).
        let Some(acc) = a else {
            caption(ui, "Account", t.ink2);
            caption(ui, "No Claude account", t.ink3);
            if ui.link(RichText::new("+ Add account on Home").small().color(t.accent)).clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(Id::unique(GO_HOME), true));
            }
            return;
        };
        // "Claude, Max": the provider and the plan, when there is one.
        let name = acc.provider.name();
        if acc.api_key {
            head_row(ui, t, name, status);
            caption(ui, "API key, pay as you go, no plan limits", t.ink3);
            return;
        }
        if !acc.fetched && acc.email.is_empty() {
            head_row(ui, t, name, status);
            return loading(ui, t, "loading usage…");
        }
        let head = if acc.plan.is_empty() { name.to_string() } else { format!("{name}, {}", acc.plan) };
        // A failed refresh (often a 429: the usage endpoint answers few
        // requests) keeps the usage from before, marked as of then.
        let stale = acc.error.as_ref().filter(|_| !acc.limits.is_empty()).map(|e| (format!("as of {}", clock(acc.usage_at)), e));
        let r = ui.scope(|ui| head_row(ui, t, &head, status.or(stale.as_ref().map(|s| s.0.as_str())))).response;
        if let Some((_, e)) = &stale {
            r.on_hover_text(format!("Usage refresh failed; retrying shortly.\n{e}"));
        }
        key_line(ui, &acc.email, t.ink3);
        if let Some(e) = acc.error.as_ref().filter(|_| acc.limits.is_empty()) {
            ui.label(RichText::new("Usage unavailable").small().color(t.error)).on_hover_text(e);
            return;
        }
        if !acc.fetched {
            return loading(ui, t, "loading usage…");
        }
        let w = ui.available_width();
        for l in acc.limits.iter().take(2) {
            let reset = l.resets_at.map(until).unwrap_or_default();
            let color = if l.percent >= 90.0 { t.error } else { t.ink };
            let r = ui
                .scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    kv(ui, t, l.label, &format!("{}%, {reset}", l.percent.floor()), color);
                    meter(ui, t, w, l.percent / 100.0, if l.percent >= 90.0 { t.error } else { t.accent });
                })
                .response;
            r.on_hover_text(format!("{}% of the {} limit used; it resets in {reset}.", l.percent.floor(), l.label));
        }
    })
    .response
}

/// The OpenRouter account, in the box a Claude account goes in: the
/// credits left and what was spent, `spent` (what the line says, and the
/// dollars); `note`, grey at the right of its first row (the provider
/// OpenRouter picked for the last reply, or where the key is kept).
pub(crate) fn openrouter_panel(ui: &mut Ui, t: &Theme, a: Option<&openrouter::Account>, spent: (&str, f64), note: Option<&str>, w: f32) -> egui::Response {
    side_box(ui, t, w, |ui| {
        head_row(ui, t, "OpenRouter", note.filter(|v| !v.is_empty()));
        let Some(a) = a else {
            caption(ui, "No API key", t.ink3);
            return;
        };
        let key = if a.label.is_empty() {
            "API key".to_string()
        } else if a.label.ends_with(" keys") {
            a.label.clone() // all of them together
        } else {
            format!("key {}", a.label)
        };
        key_line(ui, &key, t.ink3);
        if let Some(e) = &a.error {
            let what = if a.rejected { "Key rejected: remove it" } else { "Credits unavailable" };
            ui.label(RichText::new(what).small().color(t.error)).on_hover_text(e);
        } else if let Some(left) = a.left {
            // Just the number: credits don't reset, so there is no limit
            // for a bar to fill towards.
            let color = if left < 1.0 { t.error } else { t.ink }; // under a dollar
            kv(ui, t, "Credits left", &openrouter::dollars(left), color)
                .on_hover_text(format!("{} remaining of {} credits", openrouter::dollars(left), openrouter::dollars(a.total.unwrap_or(0.0))));
        } else {
            loading(ui, t, "loading credits…");
        }
        kv(ui, t, spent.0, &openrouter::dollars(spent.1), t.ink);
    })
    .response
}
/// The NodeClub account, in the box a Claude account goes in: its key and
/// the tokens used through it since OAK started; `note`, grey at the right
/// of its first row (where the key is kept).
pub(crate) fn nodeclub_panel(ui: &mut Ui, t: &Theme, a: Option<&crate::nodeclub::Account>, note: Option<&str>, w: f32) -> egui::Response {
    side_box(ui, t, w, |ui| {
        head_row(ui, t, "NodeClub", note.filter(|v| !v.is_empty()));
        let Some(a) = a else {
            caption(ui, "No API key", t.ink3);
            return;
        };
        let key = if a.label.ends_with(" keys") { a.label.clone() } else { format!("key {}", a.label) };
        key_line(ui, &key, t.ink3);
        if let Some(e) = &a.error {
            key_line(ui, e, t.error);
        } else if !a.checked {
            loading(ui, t, "checking key…");
        }
        let tokens = crate::local::tokens_text(a.total());
        kv(ui, t, "Tokens since Oak started", &tokens, t.ink).on_hover_text(format!(
            "{} new prompt tokens, {} read from the cache, {} written, over {} requests",
            crate::local::tokens_text(a.input),
            crate::local::tokens_text(a.cached),
            crate::local::tokens_text(a.output),
            a.requests
        ));
    })
    .response
}

/// A Freebuff account, in the box a Claude account goes in: its email
/// (Freebuff is free: there is nothing to count); `note`, grey at the right
/// of its first row.
pub(crate) fn freebuff_panel(ui: &mut Ui, t: &Theme, a: Option<&crate::freebuff::Account>, note: Option<&str>, w: f32) -> egui::Response {
    side_box(ui, t, w, |ui| {
        head_row(ui, t, "Freebuff", note.filter(|v| !v.is_empty()));
        let Some(a) = a else {
            caption(ui, "No Freebuff account", t.ink3);
            return;
        };
        if !a.email.is_empty() {
            key_line(ui, &a.email, t.ink3);
        }
        match &a.error {
            Some(e) => key_line(ui, e, t.error),
            None => {
                caption(ui, "free", t.ink3);
            }
        }
    })
    .response
}

/// The user's own llama.cpp server, in the box a Claude account goes in:
/// its address and the tokens used through it, all time (prompts, from the
/// server's cache and written, on hover).
pub(crate) fn local_panel(ui: &mut Ui, t: &Theme, w: f32) -> egui::Response {
    side_box(ui, t, w, |ui| {
        head_row(ui, t, "Local llama.cpp", Some("free"));
        let Some(s) = crate::local::server() else {
            caption(ui, "No server added", t.ink3);
            return;
        };
        key_line(ui, &s.url, t.ink3);
        kv(ui, t, "Tokens used", &crate::local::tokens_text(s.total()), t.ink).on_hover_text(format!(
            "{} prompt tokens processed, {} read from the server's cache, {} written, over {} requests",
            crate::local::tokens_text(s.input),
            crate::local::tokens_text(s.cache_read),
            crate::local::tokens_text(s.output),
            s.requests
        ));
    })
    .response
}

/// A box's line (a key, an email, an error): small, in `color`, cut
/// short, the whole on hover.
fn key_line(ui: &mut Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).small().color(color)).truncate()).on_hover_text(text);
}

/// An account box's first row: "name, plan", and the rotation note in
/// grey on the right.
fn head_row(ui: &mut Ui, t: &Theme, head: &str, status: Option<&str>) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let mut right = rect.right();
    if let Some(s) = status {
        let g = ui.painter().text(egui::pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, s, font.clone(), t.ink3);
        right = g.left() - 8.0;
    }
    let head = clip_width(ui, head, &font, right - rect.left());
    ui.painter().text(egui::pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, head, font, t.ink2);
}

/// A spinner and a grey note ("loading usage…"), while a box's data comes in.
fn loading(ui: &mut Ui, t: &Theme, what: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(10.0).color(t.ink3));
        caption(ui, what, t.ink3);
    });
}

/// What is typed into the panel's size boxes while they are being edited,
/// and which of them was saved last (the hint under them suggests the other).
#[derive(Default)]
pub(crate) struct SizeInputs {
    trigger: String,
    keep: String,
    changed: Option<rolling::Side>,
}

/// "120", "120k" or "1.5" (thousands of tokens) as a token count.
pub(crate) fn parse_thousands(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches(['k', 'K']).trim();
    s.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0).map(|v| (v * 1000.0).round() as u64)
}

/// `label` and a box for a size in thousands of tokens. While it isn't
/// being edited it shows `current`. Enter, Tab or clicking elsewhere
/// returns what was typed; Escape puts the current value back.
fn size_box(ui: &mut Ui, t: &Theme, label: &str, buf: &mut String, current: u64, tip: &str) -> Option<u64> {
    caption(ui, label, t.ink3);
    let id = Id::unique(("roll-", label));
    if !ui.memory(|m| m.has_focus(id)) {
        *buf = rolling::fmt_thousands(current);
    }
    let edit = TextEdit::singleline(buf).id(id).font(egui::TextStyle::Small).desired_width(34.0).margin(Margin::symmetric(3, 1));
    let resp = crate::text_menu::add(ui, edit).on_hover_text(tip);
    caption(ui, "k", t.ink3);
    let escape = ui.input(|i| i.key_pressed(Key::Escape));
    (resp.lost_focus() && !escape).then(|| parse_thousands(buf)).flatten().filter(|&n| n != current)
}

/// Smart context at a glance, beside the message box: the switch for this
/// conversation (or, before one starts, the default for new ones), its
/// context size against the trigger, what summarizing has saved, and the
/// trigger and kept sizes (for all conversations).
pub(crate) fn rolling_panel(ui: &mut Ui, t: &Theme, session: Option<&Session>, settings: &mut Settings, inputs: &mut SizeInputs, w: f32) -> egui::Response {
    // The sizes as they apply to the conversation's model (or, before one
    // starts, the one picked): capped by its context window.
    let model = session.map_or(settings.model.clone(), |s| s.conv.lock().unwrap().model.clone());
    let max = rolling::max_trigger(api::context_window(&model));
    // (A local model's: smaller still, `rolling::config_for`.)
    let cfg = rolling::config_for(&model);
    // Suggestions priced at the model's rates (cache reads are cheaper on
    // some models, which moves the best trigger).
    let rates = rolling::rates(&model);
    side_box(ui, t, w, |ui| rolling_rows(ui, t, session, settings, inputs, cfg, max, rates)).response
}

/// Saves new sizes (within the model's cap) to the settings.
fn save_sizes(settings: &mut Settings, c: rolling::Config, max: u64) {
    rolling::set_config(rolling::capped(c, max));
    let now = rolling::config();
    settings.roll_trigger = now.trigger;
    settings.roll_keep = now.keep;
    store::save_settings(settings);
}

#[allow(clippy::too_many_arguments)]
fn rolling_rows(ui: &mut Ui, t: &Theme, session: Option<&Session>, settings: &mut Settings, inputs: &mut SizeInputs, cfg: rolling::Config, max: u64, rates: rolling::Rates) {
    let (on, context, compressions, saved, cost, compacting, error) = match session {
        Some(s) => {
            let compacting = s.live.lock().unwrap().compacting;
            let c = s.conv.lock().unwrap();
            let r = &c.rolling;
            (!r.off, Some(r.context), r.compressions, r.saved, r.last_cost, compacting, r.last_error.clone())
        }
        None => (settings.rolling, None, 0, 0, None, false, None),
    };
    ui.horizontal(|ui| {
        caption(ui, "Smart context", t.ink2);
        // Summarizing, or the last summary failed: a mark beside the title.
        if compacting {
            ui.add(egui::Spinner::new().size(9.0).color(t.accent)).on_hover_text("Summarizing older context…");
        } else if let Some(e) = &error {
            failure_mark(ui, t, e);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| rolling_switch(ui, t, session, settings, on));
    });
    let value = match context {
        Some(n) if on => format!("{} / {}", rolling::fmt_tokens(n), rolling::fmt_tokens(cfg.trigger)),
        Some(n) => rolling::fmt_tokens(n),
        None => "–".into(),
    };
    // A suggestion for the sizes (below) takes the Saved row's place, which
    // then shows when the Context row is hovered.
    // Right after a size is typed, the other one that suits it, however
    // close the pair is; otherwise only a pair over 5% off. (Within the cap:
    // a suggestion past it would only be capped again.)
    let suggest = match inputs.changed {
        Some(side) => Some(rolling::paired(cfg, side, rates)),
        None => rolling::suggestion(cfg, rolling::Side::Keep, rates),
    };
    // None for a local model: they weigh API prices, and it is free.
    let local = session.map_or(crate::local::serves(&settings.model), |s| crate::local::serves(&s.conv.lock().unwrap().model));
    let suggest = suggest.map(|s| rolling::capped(s, max)).filter(|s| *s != cfg && !local);
    let saved_text = if saved == 0 { "0".into() } else { format!("~{}", rolling::fmt_tokens(saved)) };
    let f = rolling::fmt_tokens;
    let saved_tip = match cost {
        Some(c) => format!(
            "{compressions} summaries. Last: {} cached input, {} new input, {} cache write, {} output tokens.",
            f(c.cache_read),
            f(c.fresh),
            f(c.cache_write),
            f(c.output)
        ),
        None => format!("{compressions} summaries."),
    };
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        let r = kv(ui, t, "Context", &value, t.ink);
        if suggest.is_some() {
            r.on_hover_text(format!("Saved {saved_text}. {saved_tip}"));
        }
        // How far the context is towards the next summary.
        let frac = context.unwrap_or(0) as f32 / cfg.trigger as f32;
        meter(ui, t, ui.available_width(), frac, if on { t.accent } else { t.ink3 });
    });
    if suggest.is_none() {
        kv(ui, t, "Saved", &format!("{saved_text}, {compressions}×"), t.ink).on_hover_text(saved_tip);
    }
    ui.horizontal(|ui| size_row(ui, t, settings, inputs, cfg, max));
    if let Some(s) = suggest {
        suggestion_row(ui, t, settings, s, cfg, max, rates);
    }
}

/// The last summary failed: a red ! beside the title. Hover for the
/// reason, or click to keep it open.
fn failure_mark(ui: &mut Ui, t: &Theme, e: &str) {
    let why = format!("Summary failed:\n{e}\n\nRetries after a reply, at least 5 minutes after failure.");
    let bang = ui
        .add(Button::new(RichText::new("!").small().strong().color(t.error)).frame(false).min_size(vec2(18.0, 14.0)))
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&bang));
    let bang = if open { bang } else { bang.on_hover_text(&why) };
    egui::Popup::from_toggle_button_response(&bang).show(|ui| {
        ui.set_max_width(320.0);
        caption(ui, &why, t.ink);
    });
}

/// Smart context's switch: this conversation's, or before one starts the
/// default for new ones.
fn rolling_switch(ui: &mut Ui, t: &Theme, session: Option<&Session>, settings: &mut Settings, on: bool) {
    let tip = if on { "Turn smart context off" } else { "Turn smart context on" };
    if !switch(ui, t, on, vec2(30.0, 15.0)).on_hover_text(tip).clicked() {
        return;
    }
    match session {
        Some(s) => {
            let mut c = s.conv.lock().unwrap();
            c.rolling.off = on;
            c.save();
        }
        None => {
            settings.rolling = !on;
            store::save_settings(settings);
        }
    }
}

/// The sizes, for every conversation: the trigger and the kept context,
/// saved (within the cap `max`) when one is typed.
fn size_row(ui: &mut Ui, t: &Theme, settings: &mut Settings, inputs: &mut SizeInputs, cfg: rolling::Config, max: u64) {
    ui.spacing_mut().item_spacing.x = 4.0;
    let tip = format!("Summary threshold (thousands of tokens). Maximum: {}k (90% of context window).", rolling::fmt_thousands(max));
    let trigger = size_box(ui, t, "Trigger", &mut inputs.trigger, cfg.trigger, &tip);
    ui.add_space(2.0);
    let tip = format!("Recent verbatim context (thousands of tokens), up to 75% of trigger. Tool output beyond the newest {}k tokens is cleared.", rolling::RECENT / 1000);
    let keep = size_box(ui, t, "Keep", &mut inputs.keep, cfg.keep, &tip);
    if trigger.is_some() || keep.is_some() {
        // Both at once can't happen (one box has focus); keep wins.
        inputs.changed = Some(if keep.is_some() { rolling::Side::Keep } else { rolling::Side::Trigger });
        save_sizes(settings, rolling::Config { trigger: trigger.unwrap_or(cfg.trigger), keep: keep.unwrap_or(cfg.keep) }, max);
        ui.ctx().request_repaint(); // for the suggestion below
    }
}

/// When the sizes `cfg` don't suit each other, the size that suits the one
/// changed last (the keep, until one is) as `s`, under them; a click
/// applies it.
fn suggestion_row(ui: &mut Ui, t: &Theme, settings: &mut Settings, s: rolling::Config, cfg: rolling::Config, max: u64, rates: rolling::Rates) {
    let f = |n: u64| format!("{}k", rolling::fmt_thousands(n));
    // One decimal for small differences (an edit's pair can be 0.1% off).
    let pct = |x: f64| if x.abs() < 10.0 { format!("{:.1}", x) } else { format!("{:.0}", x) };
    let extra = |c: rolling::Config| pct((rolling::cost(c, rates) / rolling::cost(s, rates) - 1.0) * 100.0);
    let (lead, offer, tip) = if s.trigger != cfg.trigger {
        (
            format!("For keep {}:", f(s.keep)),
            format!("trigger {}", f(s.trigger)),
            format!(
                "Estimated lowest-cost trigger: {} kept, {} trigger. Current trigger ({}) costs ~{}% more in context reads and summaries.",
                f(s.keep),
                f(s.trigger),
                f(cfg.trigger),
                extra(cfg)
            ),
        )
    } else {
        let best = rolling::Config { trigger: rolling::best_trigger(cfg.keep, rates), ..cfg };
        (
            format!("For trigger {}:", f(s.trigger)),
            format!("keep {}", f(s.keep)),
            format!(
                "Estimated pairing: {} trigger, {} kept. For the current keep ({}), trigger {} costs ~{}% less than {}.",
                f(s.trigger),
                f(s.keep),
                f(cfg.keep),
                f(best.trigger),
                pct((1.0 - rolling::cost(best, rates) / rolling::cost(cfg, rates)) * 100.0),
                f(cfg.trigger)
            ),
        )
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        caption(ui, &lead, t.ink3);
        let b = ui
            .add(Button::new(RichText::new(offer).small().color(t.accent)).frame(false).min_size(vec2(0.0, 14.0)))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tip);
        if b.clicked() {
            save_sizes(settings, s, max);
            ui.ctx().request_repaint();
        }
    });
}

#[cfg(test)]
mod recall_tests {
    use super::*;
    use crate::app_hover_test::{key, run_frame};
    use serde_json::json;

    /// The user's last message, past what Oak added after it (tool
    /// results, notes) and the attachments' file blocks.
    #[test]
    fn last_user_text_skips_what_oak_added() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "first"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "a.txt", "_oak_file": "a.txt"},
                {"type": "text", "text": "second"},
            ]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "READ", "input": {}}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "x"},
                {"type": "text", "text": format!("{NOTE_TAG}done</task-notification>")},
            ]}),
        ];
        assert_eq!(last_user_text(&msgs).as_deref(), Some("second"));
        assert_eq!(last_user_text(&msgs[..2]).as_deref(), Some("first"));
        assert_eq!(last_user_text(&msgs[1..2]), None);
    }

    /// Sending records the text per tab; a send that left a hint (it
    /// couldn't go) doesn't.
    #[test]
    fn send_input_remembers_what_was_sent() {
        let mut app = App { draft_key: "c1".into(), input: "  hello \n".into(), ..App::default() };
        app.send_input(|_, text, _| assert_eq!(text, "hello"));
        assert_eq!(app.last_sent.get("c1").map(String::as_str), Some("hello"));
        app.input = "refused".into();
        app.send_input(|app, _, _| app.hint = Some("no".into()));
        assert_eq!(app.input, "refused");
        assert_eq!(app.last_sent.get("c1").map(String::as_str), Some("hello"));
    }

    /// Up in the empty, focused box brings the last message back with the
    /// cursor at its end; with text in the box, with a modifier, or without
    /// the keyboard it does nothing to the text.
    #[test]
    fn up_recalls_only_into_the_empty_focused_box() {
        let ctx = egui::Context::default();
        let id = Id::unique("recall-test");
        let mut app = App::default();
        let up = || key(Key::ArrowUp, Modifiers::NONE);
        let mut time = 0.0;
        let mut frame = |app: &mut App, events: Vec<egui::Event>, focus: bool| {
            run_frame(&ctx, vec2(600.0, 400.0), &mut time, 1.0 / 60.0, events, |ui| {
                app.recall(ui, id);
                let r = ui.add(sending_edit(&mut app.input, id));
                if focus {
                    r.request_focus();
                } else {
                    r.surrender_focus();
                }
            });
        };
        frame(&mut app, vec![], true);
        frame(&mut app, vec![up()], true);
        assert_eq!(app.input, "", "nothing sent yet: nothing to recall");

        app.last_sent.insert(String::new(), "line one\nline two".into());
        frame(&mut app, vec![key(Key::ArrowUp, Modifiers::SHIFT)], true);
        assert_eq!(app.input, "");
        frame(&mut app, vec![up()], true);
        assert_eq!(app.input, "line one\nline two");
        frame(&mut app, vec![egui::Event::Text("!".into())], true);
        assert_eq!(app.input, "line one\nline two!", "the cursor is at the end");

        // Up in a draft moves the cursor (to the first line), not the text.
        frame(&mut app, vec![up()], true);
        frame(&mut app, vec![egui::Event::Text("#".into())], true);
        assert_eq!(app.input, "line one#\nline two!");
        assert_eq!(app.last_sent[""], "line one\nline two", "history is unchanged");

        app.input.clear();
        frame(&mut app, vec![], false);
        frame(&mut app, vec![up()], false);
        assert_eq!(app.input, "", "only with the keyboard");
    }
}
