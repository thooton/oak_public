//! The Duck.ai page: one private chat through duck.ai (`duck.rs`), opened
//! from Home. It starts as duck.ai's own start page does, the message box in
//! the middle under "Ask me anything", all on the background; the first
//! message slides the box down to where a conversation's composer sits, the
//! composer's top line fades in over it and the fire button under the
//! attach button, which burns the chat and goes back to the start. Nothing
//! of it is kept.

use super::*;
use crate::composer::{BTN, COMPOSER_H, GAP, attach_button, composer_max_h, settings_link, square_button};
use crate::duck::{self, Model};
use crate::widgets::{chevron_v, text_width};
use eframe::emath::easing;

/// Nerd Font: a fire (burn the chat), a bolt (Fast), a light bulb
/// (Reasoning), a stop square.
const FIRE_ICON: &str = "\u{f06d}";
const FAST_ICON: &str = "\u{f0e7}";
const REASON_ICON: &str = "\u{f0eb}";
const STOP_ICON: &str = "\u{f04d}";

/// The row of controls along the message box's bottom.
const TOOLS_H: f32 = 30.0;
/// The start screen's heading over the box.
const HEADING_H: f32 = 56.0;
/// The chat's state this frame, as the page needs it.
struct ChatNow {
    busy: bool,
    error: Option<duck::Error>,
    round: u64,
    starting: bool,
    started: bool,
}

impl App {
    pub(crate) fn duck_page(&mut self, ui: &mut Ui, t: &Theme) {
        let ctx = ui.ctx().clone();
        let area = ui.max_rect();
        warm_once(&ctx);
        let models = duck::models(&ctx);
        let model = models.iter().find(|m| m.id == self.settings.duck_model).or(models.first()).cloned();
        let chat = duck::chat(|c| ChatNow { busy: c.busy, error: c.error.clone(), round: c.round, starting: c.starting, started: !c.msgs.is_empty() });
        let k = ctx.animate_bool_with_time_and_easing(Id::unique("duck-started"), chat.started, 0.45, easing::cubic_in_out);
        let lay = duck_layout(area.x_range());

        crate::composer::take_drops(&ctx, &mut self.duck_files, &mut self.duck_hint);
        // The box's height last frame: it grows upwards with its text.
        let h_id = Id::unique("duck-box-h");
        let box_h = ctx.data(|d| d.get_temp::<f32>(h_id)).unwrap_or(COMPOSER_H).max(COMPOSER_H);
        // Above the box, as wide as it: why the last message couldn't go,
        // and the files attached.
        let notes_h = self.duck_notes_height(ui, t, lay.chat.span());
        let bottom = area.bottom() - GAP;
        let low_top = bottom - box_h;
        let high_top = (area.center().y - box_h / 2.0 + HEADING_H / 2.0).max(area.top() + HEADING_H + notes_h + 8.0);
        let top = egui::lerp(high_top..=low_top, k);

        // The composer's top line, as a conversation's panel draws it.
        let line_y = low_top - notes_h - GAP;
        let line_a = ((k - 0.55) / 0.45).clamp(0.0, 1.0);
        if line_a > 0.0 {
            ui.painter().hline(area.x_range(), line_y, Stroke::new(1.0, t.line.gamma_multiply(line_a)));
        }

        // The chat, over the line.
        if k > 0.0 {
            let col = crate::composer::transcript_x(area.x_range());
            let rect = Rect::from_x_y_ranges(col, area.top()..=line_y - 1.0);
            ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
                ui.set_clip_rect(rect.intersect(ui.clip_rect()));
                ui.multiply_opacity(k);
                duck_transcript(ui, t, chat.round, chat.error.as_ref(), chat.busy);
            });
        }

        // The heading, fading as the box goes down.
        let head_a = (1.0 - k * 2.0).clamp(0.0, 1.0);
        if head_a > 0.0 {
            let at = egui::pos2(lay.chat.center(), top - notes_h - HEADING_H / 2.0);
            ui.painter().text(at, Align2::CENTER_CENTER, "Ask me anything", FontId::proportional(26.0), t.ink.gamma_multiply(head_a));
        }

        // Notes and attachments, right over the box.
        if notes_h > 0.0 {
            let r = Rect::from_x_y_ranges(lay.chat, top - notes_h..=top);
            ui.scope_builder(UiBuilder::new().max_rect(r), |ui| self.duck_notes(ui, t, if chat.started { None } else { chat.error.as_ref() }));
        }

        let (resp, h, send_now, stop) = self.duck_box(ui, t, Rect::from_x_y_ranges(lay.chat, top..=top + composer_max_h(&ctx)), &models, model.as_ref(), &chat);
        if (h - box_h).abs() > 0.5 {
            ctx.data_mut(|d| d.insert_temp(h_id, h));
            ctx.request_repaint();
        }
        let (pick, burn) = side_buttons(ui, t, Rect::from_min_size(egui::pos2(lay.buttons, top), vec2(BTN, 2.0 * BTN + 6.0)), k, chat.started);

        // Settings, in the bottom left corner: the only thing along the
        // bottom on the start screen.
        let room = Rect::from_min_max(egui::pos2(area.left() + GAP, bottom - 24.0), egui::pos2(lay.buttons - 8.0, bottom));
        if room.width() >= crate::composer::GEAR_W - 0.5 && settings_link(ui, t, room).1 {
            self.settings_from = Some(Page::Duck);
            self.page = Page::Settings;
        }

        if pick {
            self.duck_pick_files();
        }
        if stop {
            duck::stop();
        }
        if burn {
            duck::burn(&ctx);
            self.duck_input.clear();
            self.duck_files.clear();
            self.duck_hint = None;
        }
        if ui.memory(|m| m.focused().is_none()) {
            resp.request_focus();
        }
        if send_now && !chat.busy && let Some(m) = &model {
            self.duck_send(&ctx, m);
        }
        // (Moving: every frame until it is where it goes.)
        if k > 0.0 && k < 1.0 {
            ctx.request_repaint();
        }
        crate::composer::drop_overlay(ui, t, area, "Drop images, PDFs or text files to attach them");
    }

    /// The message box in `rect` (as tall as it may grow), with the model's
    /// controls along its bottom. Gives the text box, the box's height,
    /// whether Enter sent and whether Stop was clicked.
    fn duck_box(&mut self, ui: &mut Ui, t: &Theme, rect: Rect, models: &[Model], model: Option<&Model>, chat: &ChatNow) -> (egui::Response, f32, bool, bool) {
        let input_id = Id::unique("duck-input");
        let enter = ui.memory(|m| m.has_focus(input_id)) && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter));
        let max_h = composer_max_h(ui.ctx());
        let focused = ui.memory(|m| m.has_focus(input_id));
        let mut stop = false;
        let shown = ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            let v = ui.visuals();
            let stroke = if focused { v.selection.stroke } else { v.widgets.inactive.bg_stroke };
            egui::Frame::new().fill(t.panel).stroke(stroke).show(ui, |ui| {
                let w = ui.available_width();
                let edit = ScrollArea::vertical()
                    .id_salt("duck-input-scroll")
                    .max_height(max_h - 2.0 - TOOLS_H)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        crate::text_menu::add(
                            ui,
                            TextEdit::multiline(&mut self.duck_input)
                                .id(input_id)
                                .frame(egui::Frame::NONE.inner_margin(Margin { left: 10, right: 10, top: 10, bottom: 2 }))
                                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                                .min_size(vec2(w, COMPOSER_H - 2.0 - TOOLS_H))
                                .desired_width(w)
                                .hint_text("Ask anything privately"),
                        )
                    })
                    .inner;
                let (tools, _) = ui.allocate_exact_size(vec2(w, TOOLS_H), Sense::hover());
                // A click in the box's empty part writes in it.
                if ui.interact(tools, Id::unique("duck-tools-bg"), Sense::click()).clicked() {
                    edit.request_focus();
                }
                // Right to left: the model against the box's right edge
                // (its label lined up with the text's margin), the mode
                // beside it, and Stop while an answer comes.
                let row = Rect::from_min_max(egui::pos2(tools.left() + 2.0, tools.top()), egui::pos2(tools.right() - 2.0, tools.bottom() - 4.0));
                ui.scope_builder(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if let Some(m) = model {
                        self.duck_model_picker(ui, t, models, m);
                        self.duck_mode_toggle(ui, t, m);
                    }
                    if chat.busy && tool_chip(ui, t, Some(STOP_ICON), "Stop", true, "Stop the answer").clicked() {
                        stop = true;
                    }
                    if chat.starting && !chat.busy && !chat.started {
                        caption(ui, "starting duck.ai…", t.ink4);
                    }
                });
                edit
            })
        });
        (shown.inner.inner, shown.inner.response.rect.height(), enter, stop)
    }

    /// Sends what is written, and the files, to duck.ai.
    fn duck_send(&mut self, ctx: &egui::Context, m: &Model) {
        let shown = self.duck_input.trim().to_string();
        if shown.is_empty() && self.duck_files.is_empty() {
            return;
        }
        let (files, folded) = match duck::uploads(&self.duck_files, m) {
            Ok(x) => x,
            Err(e) => {
                self.duck_hint = Some(e);
                return;
            }
        };
        if shown.is_empty() && files.is_empty() && folded.is_empty() {
            return;
        }
        let text = format!("{shown}{folded}").trim().to_string();
        let blocks = std::mem::take(&mut self.duck_files);
        duck::send(shown, text, blocks, files, m.id.clone(), m.effort(self.settings.duck_reasoning), ctx);
        self.duck_input.clear();
        self.duck_hint = None;
    }

    /// How tall `duck_notes` is (0 with nothing to show).
    fn duck_notes_height(&self, ui: &Ui, t: &Theme, w: f32) -> f32 {
        let error = duck::chat(|c| c.msgs.is_empty().then(|| c.error.as_ref().map(|e| e.to_string())).flatten());
        let mut h = 0.0;
        for s in [self.duck_hint.clone(), error].into_iter().flatten() {
            let g = ui.painter().layout(s, egui::TextStyle::Body.resolve(ui.style()), t.error, w);
            h += g.size().y + 6.0;
        }
        if !self.duck_files.is_empty() {
            h += 56.0 + 10.0;
        }
        h
    }

    /// The hint, an error before the chat started (with the way past a
    /// check), and the files attached, removable.
    fn duck_notes(&mut self, ui: &mut Ui, t: &Theme, error: Option<&duck::Error>) {
        ui.spacing_mut().item_spacing.y = 6.0;
        if let Some(h) = &self.duck_hint {
            ui.label(RichText::new(h).color(t.error));
        }
        if let Some(e) = error {
            error_line(ui, t, e);
        }
        if !self.duck_files.is_empty() {
            crate::composer::attachment_strip(ui, t, &mut self.duck_files, "duck-attachment-x", false);
        }
    }

    fn duck_pick_files(&mut self) {
        let Some(files) = rfd::FileDialog::new().add_filter("Images, PDFs and text", &["png", "jpg", "jpeg", "webp", "gif", "pdf", "txt", "md", "csv", "json", "rs", "py", "js", "ts", "html", "css", "toml", "yaml", "yml", "xml", "log"]).add_filter("Any file", &["*"]).pick_files() else {
            return;
        };
        for f in files {
            crate::composer::attach_file(&mut self.duck_files, &mut self.duck_hint, &f);
        }
    }

    /// The model, as duck.ai's picker names it; a click lists the others.
    fn duck_model_picker(&mut self, ui: &mut Ui, t: &Theme, models: &[Model], m: &Model) {
        let name = short_name(&m.name);
        let font = FontId::proportional(12.5);
        let text_w = ui.fonts_mut(|f| f.layout_no_wrap(name.clone(), font.clone(), Color32::WHITE).size().x);
        let (rect, resp) = ui.allocate_exact_size(vec2(text_w + 30.0, 24.0), Sense::click());
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let lit = resp.hovered() || open;
        let p = ui.painter();
        if lit {
            p.rect_filled(rect, 0.0, t.tool_hover);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let color = if lit { t.ink } else { t.ink2 };
        p.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, &name, font, color);
        chevron_v(p, egui::pos2(rect.right() - 11.0, rect.center().y), open, color);
        let resp = if open { resp } else { resp.on_hover_text("Model") };
        egui::Popup::menu(&resp).width(230.0).show(|ui| {
            caption(ui, "duck.ai models", t.ink3);
            for x in models {
                let on = x.id == m.id;
                let mut what: Vec<&str> = match (x.can_toggle(), x.reasons(false)) {
                    (true, _) => vec!["fast or reasoning"],
                    (false, true) => vec!["reasoning"],
                    (false, false) => vec!["fast"],
                };
                what.extend([x.images.then_some("images"), x.pdf.then_some("PDFs")].into_iter().flatten());
                let r = ui.selectable_label(on, RichText::new(&x.name).color(if on { t.accent } else { t.ink })).on_hover_text(what.join(", "));
                if r.clicked() {
                    self.settings.duck_model = x.id.clone();
                    store::save_settings(&self.settings);
                }
            }
        });
    }

    /// Fast or Reasoning, for a model that does both; a model that does one
    /// shows which, and doesn't switch.
    fn duck_mode_toggle(&mut self, ui: &mut Ui, t: &Theme, m: &Model) {
        let reasons = m.reasons(self.settings.duck_reasoning);
        let (icon, label) = if reasons { (REASON_ICON, "Reasoning") } else { (FAST_ICON, "Fast") };
        let can = m.can_toggle();
        let tip = match (can, reasons) {
            (true, false) => "Fast: answers at once. Click for Reasoning",
            (true, true) => "Reasoning: thinks first. Click for Fast",
            (false, true) => "This model always reasons",
            (false, false) => "This model has no reasoning mode",
        };
        if tool_chip(ui, t, Some(icon), label, can, tip).clicked() && can {
            self.settings.duck_reasoning = !reasons;
            store::save_settings(&self.settings);
        }
    }
}

/// Where the box and its buttons go across the page: the box in the
/// transcript's column (there are no side boxes here), the buttons left of
/// it, as far in as the Settings link leaves room for.
struct DuckLayout {
    buttons: f32,
    chat: Rangef,
}

fn duck_layout(area: Rangef) -> DuckLayout {
    let col = crate::composer::transcript_x(area);
    let lead = area.min + GAP + crate::composer::GEAR_W + 8.0 + BTN + 8.0;
    let left = col.min.max(lead).min(col.max - 120.0);
    DuckLayout { buttons: left - 8.0 - BTN, chat: Rangef::new(left, col.max) }
}

/// "GPT-6 Luna" → "6 Luna", as duck.ai's own picker shortens its models.
fn short_name(name: &str) -> String {
    name.strip_prefix("GPT-").unwrap_or(name).to_string()
}

/// Starts the browser as the page first opens, so the first message is
/// quick.
fn warm_once(ctx: &egui::Context) {
    let warm_id = Id::unique("duck-warmed");
    if !ctx.data(|d| d.get_temp::<bool>(warm_id).unwrap_or(false)) {
        ctx.data_mut(|d| d.insert_temp(warm_id, true));
        duck::warm(ctx);
    }
}

/// The attach button at the box's top left, in `col`, and the fire button
/// under it, fading in with the chat (`k`). Gives whether to pick files and
/// whether to burn the chat.
fn side_buttons(ui: &mut Ui, t: &Theme, col: Rect, k: f32, started: bool) -> (bool, bool) {
    let (mut pick, mut burn) = (false, false);
    ui.scope_builder(UiBuilder::new().max_rect(col), |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        pick = attach_button(ui, t, false);
        if k > 0.01 {
            ui.multiply_opacity(k);
            let tip = "Burn this chat: it's gone, here and on duck.ai, and a new one starts";
            let clicked = square_button(ui, t, false, tip, |p, r, color| {
                let font = markdown::icon_font(15.0);
                p.text(r.center(), Align2::CENTER_CENTER, FIRE_ICON, font, color);
            });
            burn = clicked && started;
        }
    });
    (pick, burn)
}

/// A small flat control in the box's bottom row: an icon and a label,
/// lit under the pointer; `active` false, greyed and still.
fn tool_chip(ui: &mut Ui, t: &Theme, icon: Option<&str>, label: &str, active: bool, tip: &str) -> egui::Response {
    let font = FontId::proportional(12.5);
    let icons = markdown::icon_font(12.0);
    let icon_w = if icon.is_some() { 16.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(text_width(ui, label, &font) + icon_w + 16.0, 24.0), if active { Sense::click() } else { Sense::hover() });
    let hovered = active && resp.hovered();
    let p = ui.painter();
    if hovered {
        p.rect_filled(rect, 0.0, t.tool_hover);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if !active { t.ink4 } else if hovered { t.ink } else { t.ink2 };
    let mut x = rect.left() + 8.0;
    if let Some(i) = icon {
        p.text(egui::pos2(x + 6.0, rect.center().y), Align2::CENTER_CENTER, i, icons, if active { t.accent } else { color });
        x += icon_w;
    }
    p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, label, font, color);
    resp.on_hover_text(tip)
}

/// An error, and for duck.ai's person check the way past it.
fn error_line(ui: &mut Ui, t: &Theme, e: &duck::Error) {
    ui.label(RichText::new(e.to_string()).color(t.error));
    if *e == duck::Error::Captcha
        && ui.button("Open duck.ai in a window").on_hover_text("Pass the check there, close the window, then send again").clicked()
        && let Err(e) = duck::verify_in_window()
    {
        ui.label(RichText::new(e).color(t.error));
    }
}

/// The chat: the user's messages in boxes, the answers as markdown, each
/// after its reasoning (closed, as a conversation's thinking is).
fn duck_transcript(ui: &mut Ui, t: &Theme, round: u64, error: Option<&duck::Error>, busy: bool) {
    let msgs = duck::chat(|c| c.msgs.clone());
    ScrollArea::vertical().id_salt(("duck-transcript", round)).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(12.0);
        // (Each chat's own keys: a burned chat's rows don't stay open.)
        let base = usize::MAX / 2 + round as usize * 4096;
        for (i, m) in msgs.iter().enumerate() {
            if m.user {
                ui.add_space(6.0);
                if !m.files.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for b in &m.files {
                            attachment_view(ui, t, b, 56.0);
                        }
                    });
                    ui.add_space(2.0);
                }
                if !m.text.is_empty() {
                    crate::widgets::inset(ui, t, t.panel, 12, |ui| {
                        ui.label(&m.text);
                    });
                }
                ui.add_space(8.0);
                continue;
            }
            let key = (base + i, 0);
            let thinking = (!m.reasoning.is_empty()).then(|| match m.thought_ms {
                Some(ms) => serde_json::json!({"type": "reasoning", "text": m.reasoning, "_oak_ms": ms}),
                None => serde_json::json!({"type": "reasoning", "text": m.reasoning}),
            });
            let thinking_now = !m.done && m.text.is_empty() && m.tools.is_empty();
            if !m.tools.is_empty() {
                // Its searches (and the thinking before them) as one block,
                // as a conversation's tool calls are.
                transcript::tool_block(ui, t, thinking.as_ref().map(|b| (b, key, thinking_now)), &m.tools, &m.results);
                ui.add_space(6.0);
            } else if let Some(b) = &thinking {
                transcript::thinking_row(ui, t, b, key, thinking_now, None, false);
            }
            if m.text.is_empty() {
                if !m.done && (m.reasoning.is_empty() || !m.tools.is_empty()) {
                    let searching = m.tools.iter().any(|b| !m.results.contains_key(b["id"].as_str().unwrap_or("")));
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        crate::composer::paint_spinner(ui.painter(), r, t.ink3);
                        ui.label(RichText::new(if searching { "Searching…" } else { "Writing…" }).color(t.ink3));
                    });
                }
            } else {
                let text = duck::with_citations(&m.text, &m.citations);
                ui.scope(|ui| transcript::markdown_highlighted(ui, t, &text, key));
            }
            if m.done || !m.text.is_empty() {
                sources_row(ui, t, m, (base + i, 1));
            }
            if m.done && !busy && i + 1 == msgs.len() && m.empty() {
                caption(ui, "Stopped.", t.ink3);
            }
            ui.add_space(10.0);
        }
        if let Some(e) = error {
            ui.add_space(4.0);
            error_line(ui, t, e);
        }
        ui.add_space(12.0);
    });
}

/// Under an answer that searched: "N sources", which opens on the pages,
/// each a link.
fn sources_row(ui: &mut Ui, t: &Theme, m: &duck::Msg, key: (usize, usize)) {
    // The pages it cites, then any others duck.ai lists.
    let mut list = duck::cited(&m.text, &m.citations);
    for c in &m.sources {
        if !list.iter().any(|x| x.url == c.url) {
            list.push(c);
        }
    }
    if list.is_empty() {
        return;
    }
    ui.add_space(4.0);
    let (rect, open, hovered) = transcript::row(ui, t, Id::unique(("duck-sources", key)), true, false);
    let p = ui.painter();
    let y = rect.center().y;
    crate::widgets::chevron(p, egui::pos2(rect.left() + 14.0, y), open, if hovered { t.ink } else { t.ink3 });
    let label = format!("{} source{}", list.len(), crate::plural(list.len()));
    let drawn = p.text(egui::pos2(rect.left() + 28.0, y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), t.ink2);
    // The sites, after it, as far as they fit.
    let mut sites: Vec<&str> = vec![];
    for c in &list {
        if !sites.contains(&c.site.as_str()) {
            sites.push(&c.site);
        }
    }
    let room = rect.right() - 10.0 - drawn.right() - 14.0;
    let sites = crate::widgets::clip_width(ui, &sites.join(", "), &FontId::proportional(12.0), room.max(30.0));
    ui.painter().text(egui::pos2(drawn.right() + 14.0, y), Align2::LEFT_CENTER, sites, FontId::proportional(12.0), t.ink3);
    if !open {
        return;
    }
    ui.horizontal_top(|ui| {
        ui.add_space(28.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add_space(2.0);
            for c in list {
                let title = if c.title.is_empty() { c.url.as_str() } else { c.title.as_str() };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    caption(ui, &c.site, t.ink3);
                    let r = ui.add(egui::Label::new(RichText::new(title).color(t.accent)).truncate().sense(Sense::click()));
                    if r.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if r.clicked() {
                        let _ = open::that(&c.url);
                    }
                    r.on_hover_text(&c.url);
                });
            }
            ui.add_space(4.0);
        });
    });
}
