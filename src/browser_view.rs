//! The browser pane, on the right of a conversation: a toolbar (back,
//! forward, reload, the address, close) over the page, which takes the
//! user's pointer, wheel and keys. It shows what the conversation's
//! browser (`browser.rs`) is doing, the model's calls included. The globe
//! in the title bar, or Ctrl+Shift+B, opens and closes it.

use super::*;
use crate::browser::{self, Instance, Status};
use std::sync::Arc;

pub(crate) const PANE_DEFAULT_W: f32 = 640.0;
const BAR_H: f32 = 34.0;
const GLOBE: &str = "\u{f059f}";

/// The toolbar's icons, drawn as strokes of one weight in boxes of one
/// size (the icon font's glyphs come in different sizes and weights).
#[derive(Clone, Copy)]
enum Icon {
    Back,
    Forward,
    Reload,
    /// An x: Stop while a page loads, and Close.
    Cross,
}

/// The pointer's state over the page, between frames.
#[derive(Clone, Default)]
struct Pointer {
    /// A button went down on the page and hasn't come up: moves outside it
    /// still go to the page (a drag), and so does the release.
    held: bool,
    /// For double and triple clicks: the last press, where, which button,
    /// and how many in a row.
    last: Option<(f64, egui::Pos2, egui::PointerButton, i64)>,
}

/// A `<select>`'s options, open in Oak (CSS point to pick at, its options).
#[derive(Clone)]
struct OpenSelect {
    at: egui::Pos2,
    select: browser::Select,
}

impl App {
    /// The conversation whose browser pane the shown screen can have: a
    /// conversation the user started (not a subagent, a plugin's tab or a cloud session).
    pub(crate) fn browser_conv(&self) -> Option<String> {
        if self.page != Page::Chats {
            return None;
        }
        let Screen::Chat(id) = &self.screen else { return None };
        let s = self.sessions.get(id)?;
        {
            let c = s.conv.lock().unwrap();
            (c.parent.is_none() && c.side_chat_of.is_none()).then(|| id.clone())
        }
    }

    fn toggle_browser(&mut self, conv: &str, ctx: &egui::Context) {
        // Reveal an already-open browser hidden behind the side chat;
        // switching panes must not end the browser or interrupt its tools.
        if !self.side_browser && self.shown_side_chat().is_some() && browser::is_open(conv) {
            self.side_browser = true;
        } else {
            browser::toggle(conv, ctx);
            self.side_browser = browser::is_open(conv);
        }
        self.side_active = false;
    }

    /// The globe in the title bar, left of the window's buttons: opens or
    /// closes the shown conversation's pane. Ctrl+Shift+B does the same.
    pub(crate) fn browser_toggle(&mut self, ui: &mut Ui, t: &Theme) {
        let Some(conv) = self.browser_conv() else { return };
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::B)) {
            self.toggle_browser(&conv, ui.ctx());
        }
        let r = ui.max_rect();
        let rect = Rect::from_min_size(egui::pos2(r.right() - 3.0 * 36.0 - 40.0, r.top()), vec2(36.0, r.height()));
        let resp = ui.interact(rect, Id::unique("browser-toggle"), Sense::click());
        let open = browser::is_open(&conv) && (self.shown_side_chat().is_none() || self.side_browser);
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, 0.0, t.panel);
        }
        let color = if open { t.accent } else if resp.hovered() { t.ink } else { t.ink3 };
        p.text(rect.center(), Align2::CENTER_CENTER, GLOBE, markdown::icon_font(15.0), color);
        let tip = if open { "Close the browser (Ctrl+Shift+B)" } else if browser::is_open(&conv) { "Show the browser (Ctrl+Shift+B)" } else { "Open the browser (Ctrl+Shift+B)\nThe model can use it while it is open." };
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
            self.toggle_browser(&conv, ui.ctx());
        }
    }
}

pub(crate) fn pane(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>) {
    let r = ui.max_rect();
    // The panel keeps the size its contents take: claimed whole, or it
    // would shrink back to what the toolbar and page allocate.
    ui.expand_to_include_rect(r);
    ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0, t.line));
    let bar = Rect::from_min_max(r.min + vec2(1.0, 0.0), egui::pos2(r.right(), r.top() + BAR_H));
    toolbar(ui, t, inst, bar);
    let page = Rect::from_min_max(egui::pos2(r.left() + 1.0, bar.bottom()), r.max);
    page_view(ui, t, inst, page);
}

/// A toolbar button: an icon, dimmed when it can't be used.
fn bar_button(ui: &mut Ui, t: &Theme, rect: Rect, id: Id, icon: Icon, tip: &str, enabled: bool) -> bool {
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.painter().rect_filled(Rect::from_center_size(rect.center(), vec2(24.0, 24.0)), 4.0, t.panel2);
    }
    let color = if !enabled { t.ink4 } else if hovered { t.ink } else { t.ink2 };
    paint_icon(ui.painter(), rect.center(), icon, color);
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    resp.on_hover_text(tip).clicked() && enabled
}

/// `icon` centered on `c`, in a box about 12 points across.
fn paint_icon(p: &egui::Painter, c: egui::Pos2, icon: Icon, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    match icon {
        Icon::Back | Icon::Forward => {
            let d = if matches!(icon, Icon::Back) { -1.0 } else { 1.0 };
            let tip = c + vec2(6.0 * d, 0.0);
            p.line_segment([c - vec2(6.0 * d, 0.0), tip], stroke);
            p.add(egui::Shape::line(vec![tip + vec2(-4.5 * d, -4.5), tip, tip + vec2(-4.5 * d, 4.5)], stroke));
        }
        Icon::Reload => {
            // Clockwise from the right round to the upper right, into the
            // corner of an arrowhead there.
            let r = 5.5;
            let to = 300f32.to_radians();
            let n = 24;
            let corner = c + vec2(r, -2.4);
            let mut line: Vec<_> = (0..=n).map(|i| c + r * egui::Vec2::angled(to * i as f32 / n as f32)).collect();
            line.push(corner);
            p.add(egui::Shape::line(line, stroke));
            p.add(egui::Shape::line(vec![corner + vec2(0.0, -3.4), corner, corner + vec2(-3.4, 0.0)], stroke));
        }
        Icon::Cross => {
            let h = 4.5;
            p.line_segment([c + vec2(-h, -h), c + vec2(h, h)], stroke);
            p.line_segment([c + vec2(-h, h), c + vec2(h, -h)], stroke);
        }
    }
}

fn toolbar(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, bar: Rect) {
    let p = ui.painter().clone();
    p.rect_filled(bar, 0.0, t.panel);
    p.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, t.line));
    let (url, back, forward, loading, ready) = {
        let v = inst.view.lock().unwrap();
        (v.url.clone(), v.back, v.forward, v.loading, v.status == Status::Ready)
    };
    let conv = &inst.conv;
    let b = |i: f32| Rect::from_min_size(egui::pos2(bar.left() + 4.0 + i * 30.0, bar.top() + 2.0), vec2(30.0, BAR_H - 4.0));
    if bar_button(ui, t, b(0.0), Id::unique(("browser-back", conv)), Icon::Back, "Back", ready && back) {
        inst.go(-1);
    }
    if bar_button(ui, t, b(1.0), Id::unique(("browser-forward", conv)), Icon::Forward, "Forward", ready && forward) {
        inst.go(1);
    }
    let (icon, tip) = if loading { (Icon::Cross, "Stop") } else { (Icon::Reload, "Reload") };
    if bar_button(ui, t, b(2.0), Id::unique(("browser-reload", conv)), icon, tip, ready) {
        if loading { inst.stop_loading() } else { inst.reload() }
    }
    let close = Rect::from_min_size(egui::pos2(bar.right() - 34.0, bar.top() + 2.0), vec2(30.0, BAR_H - 4.0));
    if bar_button(ui, t, close, Id::unique(("browser-close", conv)), Icon::Cross, "Close the browser (Ctrl+Shift+B)", true) {
        browser::close(conv);
    }
    // The address: what the page is at, until the user edits it.
    let field = Rect::from_min_max(egui::pos2(b(3.0).left() + 4.0, bar.top() + 5.0), egui::pos2(close.left() - 6.0, bar.bottom() - 5.0));
    let id = Id::unique(("browser-address", conv));
    let text_id = id.with("text");
    let focused = ui.memory(|m| m.has_focus(id));
    let mut text = ui.data(|d| d.get_temp::<String>(text_id)).filter(|_| focused).unwrap_or_else(|| shown_url(&url));
    let resp = crate::text_menu::put(
        ui,
        field,
        TextEdit::singleline(&mut text).id(id).font(FontId::proportional(12.5)).hint_text("Search or enter address").margin(Margin::symmetric(8, 3)).desired_width(field.width()),
    );
    if resp.gained_focus() {
        // All of it selected, to type over.
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), id);
        }
    }
    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && ready {
        inst.navigate(&text);
        ui.data_mut(|d| d.remove::<String>(text_id));
    } else if ui.memory(|m| m.has_focus(id)) {
        ui.data_mut(|d| d.insert_temp(text_id, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(text_id));
    }
}

/// The address as the bar shows it (about:blank as nothing).
fn shown_url(url: &str) -> String {
    if url == "about:blank" { String::new() } else { url.to_string() }
}

fn page_view(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, rect: Rect) {
    let ctx = ui.ctx().clone();
    let scale = ctx.pixels_per_point();
    inst.drawn_at(rect.width(), rect.height(), scale);
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, Color32::WHITE);
    // The newest frame, into the texture.
    let frame = inst.view.lock().unwrap().frame.take();
    {
        let mut tex = inst.texture.lock().unwrap();
        if let Some(img) = frame {
            match tex.as_mut() {
                Some(h) => h.set(img, egui::TextureOptions::LINEAR),
                None => *tex = Some(ctx.load_texture(format!("browser-{}", inst.conv), img, egui::TextureOptions::LINEAR)),
            }
        }
        if let Some(h) = tex.as_ref() {
            paint_page_texture(&p, h, rect);
        }
    }
    let status = inst.view.lock().unwrap().status.clone();
    match status {
        Status::Ready => {}
        Status::Starting => {
            p.rect_filled(rect, 0.0, t.bg);
            p.text(rect.center(), Align2::CENTER_CENTER, "Starting the browser…", FontId::proportional(13.0), t.ink3);
            return;
        }
        Status::Failed(e) | Status::Quit(e) => {
            p.rect_filled(rect, 0.0, t.bg);
            let area = Rect::from_center_size(rect.center(), vec2((rect.width() - 40.0).min(420.0), 140.0));
            ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(e).color(t.error));
                    ui.add_space(10.0);
                    if ui.button("Restart the browser").clicked() {
                        browser::restart(&inst.conv, ui.ctx());
                    }
                });
            });
            return;
        }
    }
    let page_id = Id::unique(("browser-page", &inst.conv));
    let resp = ui.interact(rect, page_id, Sense::click_and_drag());
    let (dialog, cursor, select, agent_at, notice, copied) = {
        let mut v = inst.view.lock().unwrap();
        (v.dialog.clone(), v.cursor, v.select.clone(), v.agent_at, v.notice.clone(), v.copied.take())
    };
    if let Some(text) = copied {
        ctx.copy_text(text);
    }
    let sel_id = page_id.with("select");
    let open_select = ui.data(|d| d.get_temp::<OpenSelect>(sel_id));
    if dialog.is_none() && open_select.is_none() {
        forward_input(ui, inst, rect, page_id, resp.hovered(), select.as_ref());
        if resp.hovered() {
            ctx.set_cursor_icon(cursor);
        }
    }
    // The page's file chooser, opened by the user.
    if let Some(c) = inst.take_chooser() {
        let dialog = rfd::FileDialog::new();
        let files = if c.multiple { dialog.pick_files().unwrap_or_default() } else { dialog.pick_file().into_iter().collect() };
        inst.choose_files(&c, files);
    }
    // Where the model just clicked: a ring that fades.
    if let Some((at, when)) = agent_at {
        let age = when.elapsed().as_secs_f32();
        if age < 1.0 {
            let c = rect.min + at.to_vec2();
            p.circle_stroke(c, 6.0 + 14.0 * age, Stroke::new(2.0, t.accent.gamma_multiply(1.0 - age)));
            ctx.request_repaint();
        }
    }
    // The model at work: an accent edge and a tag.
    if inst.agent_busy() {
        p.rect_stroke(rect.shrink(1.0), 0.0, Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
        let g = p.layout_no_wrap("Oak is using the browser".into(), FontId::proportional(11.5), Color32::WHITE);
        let tag = Rect::from_min_size(egui::pos2(rect.right() - g.size().x - 18.0, rect.top() + 8.0), g.size() + vec2(12.0, 6.0));
        p.rect_filled(tag, 4.0, t.accent);
        p.galley(tag.min + vec2(6.0, 3.0), g, Color32::WHITE);
    }
    if let Some((text, when)) = notice
        && when.elapsed().as_secs() < 6
    {
        let g = p.layout(text, FontId::proportional(12.0), t.ink, rect.width() - 40.0);
        let r = Rect::from_min_size(egui::pos2(rect.left() + 12.0, rect.bottom() - g.size().y - 22.0), g.size() + vec2(16.0, 10.0));
        p.rect_filled(r, 4.0, t.panel2);
        p.rect_stroke(r, 4.0, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
        p.galley(r.min + vec2(8.0, 5.0), g, t.ink);
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
    if let Some(d) = dialog {
        dialog_view(ui, t, inst, rect, &d);
    }
    if let Some(s) = open_select {
        select_view(ui, t, inst, rect, sel_id, &s);
    }
}

/// Screencast pixels are not necessarily device pixels: Chromium can
/// downsample them independently of the screen's scale. Map the complete
/// frame to the page's CSS viewport, which is also where input is sent.
fn paint_page_texture(p: &egui::Painter, texture: &egui::TextureHandle, rect: Rect) {
    p.image(texture.id(), rect, Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
}

/// The user's pointer, wheel and keys, to the page.
/// `hovered`: the page is what the pointer is on (not the pane's resize
/// edge, which overlaps it), so a press there is the page's.
fn forward_input(ui: &mut Ui, inst: &Arc<Instance>, rect: Rect, page_id: Id, hovered: bool, select: Option<&browser::Select>) {
    let events = ui.input(|i| i.events.clone());
    if forward_pointer(ui, inst, rect, page_id, hovered, select, &events) {
        forward_keys(ui, inst, page_id, &events);
    }
}

/// The pointer and the wheel, to the page; a press on its `<select>`
/// opens the options in Oak instead. True when the page has the focus.
fn forward_pointer(ui: &mut Ui, inst: &Arc<Instance>, rect: Rect, page_id: Id, hovered: bool, select: Option<&browser::Select>, events: &[egui::Event]) -> bool {
    let ptr_id = page_id.with("pointer");
    let mut ptr = ui.data(|d| d.get_temp::<Pointer>(ptr_id)).unwrap_or_default();
    let (time, down) = ui.input(|i| {
        let down = (i.pointer.button_down(egui::PointerButton::Primary) as i64)
            | (i.pointer.button_down(egui::PointerButton::Secondary) as i64) << 1
            | (i.pointer.button_down(egui::PointerButton::Middle) as i64) << 2;
        (i.time, down)
    });
    let css = |p: egui::Pos2| (p - rect.min).to_pos2();
    let mods_now = ui.input(|i| i.modifiers);
    let mut focused = ui.memory(|m| m.has_focus(page_id));
    for e in events {
        match e {
            egui::Event::PointerMoved(p) if rect.contains(*p) || ptr.held => {
                let held = if ptr.held { down } else { 0 };
                let button = if held & 1 != 0 { "left" } else if held & 2 != 0 { "right" } else if held & 4 != 0 { "middle" } else { "none" };
                inst.mouse("mouseMoved", css(*p), button, held, 0, browser::modifier_bits(mods_now));
            }
            egui::Event::PointerButton { pos, button, pressed: true, modifiers } if hovered && rect.contains(*pos) => {
                // Over a `<select>`: its options open in Oak instead.
                if *button == egui::PointerButton::Primary
                    && let Some(s) = select.filter(|s| !s.disabled)
                {
                    let at = css(*pos);
                    let [x, y, w, h] = s.rect;
                    if at.x >= x && at.x <= x + w && at.y >= y && at.y <= y + h {
                        ui.data_mut(|d| d.insert_temp(page_id.with("select"), OpenSelect { at: egui::pos2(x + w / 2.0, y + h / 2.0), select: s.clone() }));
                        continue;
                    }
                }
                ui.memory_mut(|m| m.request_focus(page_id));
                focused = true;
                let count = match ptr.last {
                    Some((t, p, b, n)) if b == *button && time - t < 0.5 && p.distance(*pos) < 5.0 => n % 3 + 1,
                    _ => 1,
                };
                ptr.last = Some((time, *pos, *button, count));
                ptr.held = true;
                inst.mouse("mousePressed", css(*pos), browser::button_name(*button), down, count, browser::modifier_bits(*modifiers));
            }
            egui::Event::PointerButton { pos, button, pressed: false, modifiers } if ptr.held || rect.contains(*pos) => {
                let count = ptr.last.map_or(1, |l| l.3);
                inst.mouse("mouseReleased", css(*pos), browser::button_name(*button), down, count, browser::modifier_bits(*modifiers));
                if down == 0 {
                    ptr.held = false;
                }
            }
            egui::Event::MouseWheel { unit, delta, modifiers, .. } => {
                let Some(at) = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p)) else { continue };
                let k = match unit {
                    egui::MouseWheelUnit::Point => 1.0,
                    egui::MouseWheelUnit::Line => 40.0,
                    egui::MouseWheelUnit::Page => rect.height(),
                };
                // egui's up is positive; the page's down is.
                inst.wheel(css(at), -delta.x * k, -delta.y * k, browser::modifier_bits(*modifiers));
            }
            _ => {}
        }
    }
    // The wheel over the page is the page's: nothing else scrolls with it.
    if ui.rect_contains_pointer(rect) {
        ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
    }
    ui.data_mut(|d| d.insert_temp(ptr_id, ptr));
    focused
}

/// Keys go to the page while it has the focus: Tab and the arrows too,
/// and nothing else in Oak sees them (but Oak's own shortcuts).
fn forward_keys(ui: &mut Ui, inst: &Arc<Instance>, page_id: Id, events: &[egui::Event]) {
    ui.memory_mut(|m| m.set_focus_lock_filter(page_id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
    let sent: Vec<bool> = events.iter().map(|e| send_key(inst, e)).collect();
    if !sent.contains(&true) {
        return;
    }
    ui.input_mut(|inp| {
        let mut sent = sent.iter();
        inp.events.retain(|_| !sent.next().is_some_and(|s| *s));
        inp.keys_down.clear();
    });
}

/// Sends key, text or clipboard event `e` to the page; false when it
/// isn't one, or is one of Oak's own shortcuts.
fn send_key(inst: &Arc<Instance>, e: &egui::Event) -> bool {
    match e {
        egui::Event::Key { key, pressed, modifiers, .. } => {
            let oak = (modifiers.command && modifiers.shift && *key == Key::B) || (modifiers.alt && *key == Key::H) || (modifiers.command && !modifiers.shift && *key == Key::T) || crate::tabs::tab_key(*key, *modifiers).is_some();
            if oak {
                return false;
            }
            inst.key(key.name(), *pressed, browser::modifier_bits(*modifiers));
        }
        egui::Event::Text(text) => inst.text(text),
        egui::Event::Paste(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => inst.paste(text),
        egui::Event::Copy | egui::Event::Cut => inst.copy(matches!(e, egui::Event::Cut)),
        _ => return false,
    }
    true
}

/// The page's alert, confirm or prompt, over it.
fn dialog_view(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, rect: Rect, d: &browser::Dialog) {
    ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(110));
    let w = (rect.width() - 40.0).min(420.0);
    let text_id = Id::unique(("browser-prompt", &inst.conv));
    let mut text = ui.data(|m| m.get_temp::<String>(text_id)).unwrap_or_else(|| d.default.clone());
    let area = Rect::from_min_size(egui::pos2(rect.center().x - w / 2.0, rect.top() + 60.0), vec2(w, 200.0));
    let mut answer = None;
    ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
        egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).corner_radius(6.0).inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(w - 28.0);
            let host = inst.view.lock().unwrap().url.split('/').nth(2).unwrap_or("The page").to_string();
            ui.label(RichText::new(format!("{host} says")).color(t.ink3).size(11.5));
            ui.add_space(4.0);
            ui.label(RichText::new(if d.kind == "beforeunload" { "Leave the page? Changes may not be saved." } else { &d.message }).color(t.ink));
            ui.add_space(8.0);
            if d.kind == "prompt" {
                let r = crate::text_menu::add(ui, TextEdit::singleline(&mut text).desired_width(f32::INFINITY));
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    answer = Some(true);
                }
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                if ui.button(RichText::new(if d.kind == "beforeunload" { "Leave" } else { "OK" }).color(t.accent)).clicked() {
                    answer = Some(true);
                }
                if d.kind != "alert" && ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
    });
    match answer {
        Some(a) => {
            inst.answer_dialog(a, &text);
            ui.data_mut(|m| {
                m.remove::<String>(text_id);
            });
        }
        None => ui.data_mut(|m| {
            m.insert_temp(text_id, text);
        }),
    }
}

/// A `<select>`'s options, under it.
fn select_view(ui: &mut Ui, t: &Theme, inst: &Arc<Instance>, rect: Rect, id: Id, s: &OpenSelect) {
    let [x, y, w, h] = s.select.rect;
    let at = rect.min + vec2(x, y + h);
    let mut picked = None;
    let area = egui::Area::new(id.with("area")).order(egui::Order::Foreground).fixed_pos(at).show(ui.ctx(), |ui| {
        egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).inner_margin(Margin::same(4)).show(ui, |ui| {
            ui.set_min_width(w.max(120.0));
            ScrollArea::vertical().max_height((rect.bottom() - at.y - 10.0).max(120.0)).show(ui, |ui| {
                for (i, o) in s.select.options.iter().enumerate() {
                    if ui.selectable_label(s.select.selected == Some(i), o).clicked() {
                        picked = Some(i);
                    }
                }
            });
        });
    });
    let escape = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    if let Some(i) = picked {
        inst.pick_option(s.at, i);
    }
    // The release of the press that opened it, or any click on the select
    // itself, leaves it open.
    let sel_rect = Rect::from_min_size(rect.min + vec2(x, y), vec2(w, h));
    let on_select = ui.input(|i| i.pointer.press_origin()).is_some_and(|o| sel_rect.contains(o));
    if picked.is_some() || escape || (area.response.clicked_elsewhere() && !on_select) {
        ui.data_mut(|d| d.remove::<OpenSelect>(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{Headless, click, conv_json, press, run_frame};

    /// A screencast can arrive at CSS resolution even on a HiDPI screen.
    /// Its image must fill the same rectangle that receives page input.
    #[test]
    fn page_texture_fills_viewport_at_every_screen_scale() {
        let rect = Rect::from_min_size(egui::pos2(700.0, 40.0), vec2(600.0, 900.0));
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for frame_scale in [1.0, scale] {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(scale);
                let size = [(rect.width() * frame_scale) as usize, (rect.height() * frame_scale) as usize];
                let texture = ctx.load_texture("page-test", egui::ColorImage::filled(size, Color32::WHITE), egui::TextureOptions::LINEAR);
                let out = run_frame(&ctx, vec2(1400.0, 1000.0), &mut 0.0, 1.0 / 60.0, vec![], |ui| paint_page_texture(ui.painter(), &texture, rect));
                let meshes = ctx.tessellate(out.shapes, out.pixels_per_point);
                let mesh = meshes.iter().find_map(|p| match &p.primitive {
                    egui::epaint::Primitive::Mesh(m) if m.texture_id == texture.id() => Some(m),
                    _ => None,
                }).expect("page image was not painted");
                assert_eq!(mesh.calc_bounds(), rect, "screen scale {scale}, frame scale {frame_scale}");
            }
        }
    }

    /// The pane keeps its width from frame to frame, and the width the
    /// user drags it to (it used to shrink back to its minimum, 40 points
    /// a frame, as its contents didn't claim the whole of it). Starts a
    /// real browser.
    #[test]
    #[ignore]
    fn whole_app_pane_keeps_its_width() {
        const ID: &str = "pane-width-test";
        let messages = serde_json::json!([{"role":"user","content":[{"type":"text","text":"hi"}]}]);
        let (_dir, mut h, mut app) = Headless::start("pane-width", &[conv_json(ID, "T", messages)]);
        h.screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1920.0, 1000.0));
        app.open(ID.into());
        browser::open(ID, &h.ctx);
        // Runs a frame; the pane's left edge (the address field is 100
        // points into it).
        let run = |h: &mut Headless, app: &mut App, events: Vec<egui::Event>| {
            h.events(app, events);
            h.ctx.read_response(Id::unique(("browser-address", ID))).unwrap().rect.left() - 100.0
        };
        for _ in 0..8 {
            assert_eq!(run(&mut h, &mut app, vec![]), 1920.0 - PANE_DEFAULT_W);
        }
        let (y, x0) = (500.0, 1920.0 - PANE_DEFAULT_W);
        let button = |x: f32, pressed| press(egui::pos2(x, y), egui::PointerButton::Primary, pressed);
        run(&mut h, &mut app, vec![egui::Event::PointerMoved(egui::pos2(x0, y))]);
        run(&mut h, &mut app, vec![button(x0, true)]);
        for k in 1..=10 {
            run(&mut h, &mut app, vec![egui::Event::PointerMoved(egui::pos2(x0 - 20.0 * k as f32, y))]);
        }
        run(&mut h, &mut app, vec![button(x0 - 200.0, false)]);
        for _ in 0..8 {
            assert_eq!(run(&mut h, &mut app, vec![]), x0 - 200.0);
        }
        // A press on the page is still the page's (not the edge's).
        let inst = browser::get(ID).unwrap();
        let start = std::time::Instant::now();
        while inst.view.lock().unwrap().status != Status::Ready {
            assert!(start.elapsed().as_secs() < 30, "the browser didn't start");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mid = egui::pos2(1500.0, y);
        for e in click(mid, egui::PointerButton::Primary) {
            run(&mut h, &mut app, vec![e]);
        }
        assert!(h.ctx.memory(|m| m.has_focus(Id::unique(("browser-page", ID)))));
        browser::close(ID);
    }
}
