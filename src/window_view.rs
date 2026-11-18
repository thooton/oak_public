//! The live view of a program window on Oak's hidden desktop (Windows):
//! while the shown conversation has a current window there (the WINDOW
//! tool's, `window_tool.rs`), a picture of it sits beside the transcript
//! where the browser's preview does (under the browser's and Blender's when
//! those are shown), see-through until the pointer is on it, renewed about
//! once a second. A click expands it over the window, live (about fifteen
//! pictures a second) and interactive: the pointer, the wheel and keys go
//! to the app as the WINDOW tool's input does (posted to its window, so
//! the user's real cursor and focus stay in Oak). The X, a click beside
//! the picture, or Esc with the pointer off the picture go back.
//!
//! Each window has its own background frame worker and texture cache, so
//! one busy agent cannot overwrite or stall another agent's view. Private
//! Blender uses this renderer with its own fixed window selection, never
//! WINDOW's current app. Capture/input share the hidden desktop thread
//! (`winctl::on_desktop`). A Blender Oak runs (`mcp/blender_run.rs`) is
//! pictured by Blender itself and takes the input as Blender's events.

use super::*;
#[cfg(test)]
mod tests;
use crate::winctl::{self, Live};
use egui::TextureHandle;
use std::sync::{Arc, LazyLock, Mutex};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often a picture is taken for the preview, and while expanded.
const EVERY: Duration = Duration::from_secs(1);
const EVERY_EXPANDED: Duration = Duration::from_millis(66);
const EVERY_BLENDER: Duration = Duration::from_millis(110);
/// How long a want lasts without being renewed (a view not drawn stops it).
const WANT_FOR: Duration = Duration::from_secs(3);
/// Longer side of a picture for the preview (expanded: the window's own).
const SMALL: u32 = 720;
const FADE: f32 = 0.25;
const EXPAND: f32 = 0.16;

struct Frame {
    seq: u64,
    window: usize,
    img: Arc<image::RgbaImage>,
    /// The window's client size when taken (the picture may be smaller).
    client: (u32, u32),
}

struct Slot {
    until: Instant,
    expanded: bool,
    frame: Option<Frame>,
    ctx: egui::Context,
}

static FRAMES: LazyLock<Mutex<HashMap<usize, Slot>>> = LazyLock::new(Mutex::default);
static NEXT_FRAME: AtomicU64 = AtomicU64::new(1);

fn want(ctx: &egui::Context, w: usize, expanded: bool) {
    let mut frames = FRAMES.lock().unwrap();
    if let Some(s) = frames.get_mut(&w) {
        s.until = Instant::now() + WANT_FOR;
        s.expanded = expanded;
        return;
    }
    frames.insert(w, Slot { until: Instant::now() + WANT_FOR, expanded, frame: None, ctx: ctx.clone() });
    drop(frames);
    if std::thread::Builder::new().name(format!("window-view-{w:x}")).spawn(move || worker(w)).is_err() {
        FRAMES.lock().unwrap().remove(&w);
    }
}

/// Each window has its own worker and cache: a busy Blender cannot replace
/// another agent's picture or block an unrelated app's view.
fn worker(w: usize) {
    loop {
        let expanded = {
            let mut frames = FRAMES.lock().unwrap();
            let Some(s) = frames.get(&w) else { return };
            if s.until <= Instant::now() || !winctl::alive(w) { frames.remove(&w); return; }
            s.expanded
        };
        let began = Instant::now();
        let blender = crate::mcp::blender_run::port_of_window(w).is_some();
        if let Ok(img) = crate::window_tool::picture(w) {
            let client = (img.width(), img.height());
            let img = if !expanded && img.width().max(img.height()) > SMALL {
                let k = SMALL as f32 / img.width().max(img.height()) as f32;
                image::imageops::resize(&img, (img.width() as f32 * k).round().max(1.0) as u32, (img.height() as f32 * k).round().max(1.0) as u32, image::imageops::FilterType::Triangle)
            } else { img };
            let mut frames = FRAMES.lock().unwrap();
            if let Some(s) = frames.get_mut(&w) {
                s.frame = Some(Frame { seq: NEXT_FRAME.fetch_add(1, Ordering::Relaxed), window: w, img: Arc::new(img), client });
                s.ctx.request_repaint();
            }
        }
        let every = match (expanded, blender) {
            (false, _) => EVERY,
            (true, false) => EVERY_EXPANDED,
            (true, true) => EVERY_BLENDER,
        };
        std::thread::sleep(every.saturating_sub(began.elapsed()));
    }
}

/// The newest picture of window `w` as a texture (uploaded once per
/// picture), and the window's client size.
fn texture(ctx: &egui::Context, w: usize) -> Option<(TextureHandle, (u32, u32))> {
    let s = FRAMES.lock().unwrap();
    let f = s.get(&w)?.frame.as_ref().filter(|f| f.window == w)?;
    let (seq, img, client) = (f.seq, f.img.clone(), f.client);
    drop(s);
    let id = Id::unique(("window-view-texture", w));
    let handle = match ctx.data(|d| d.get_temp::<(u64, TextureHandle)>(id)) {
        Some((s, h)) if s == seq => h,
        Some((_, mut h)) => {
            h.set(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()), egui::TextureOptions::LINEAR);
            h
        }
        None => ctx.load_texture("window-view", egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()), egui::TextureOptions::LINEAR),
    };
    ctx.data_mut(|d| d.insert_temp(id, (seq, handle.clone())));
    Some((handle, client))
}

fn expanded_id(conv: &str) -> Id {
    Id::unique(("window-view-expanded", conv))
}

/// Which mouse buttons are held, as the window's messages say them.
fn held(ctx: &egui::Context) -> usize {
    ctx.input(|i| {
        (if i.pointer.button_down(egui::PointerButton::Primary) { winctl::HELD_LEFT } else { 0 })
            | (if i.pointer.button_down(egui::PointerButton::Secondary) { winctl::HELD_RIGHT } else { 0 })
            | (if i.pointer.button_down(egui::PointerButton::Middle) { winctl::HELD_MIDDLE } else { 0 })
    })
}

/// An egui key as `winctl::key` names it (None: one it doesn't know).
fn key_name(k: Key) -> Option<String> {
    let named = match k {
        Key::Enter => "enter",
        Key::Tab => "tab",
        Key::Escape => "escape",
        Key::Backspace => "backspace",
        Key::Delete => "delete",
        Key::Insert => "insert",
        Key::Home => "home",
        Key::End => "end",
        Key::PageUp => "pageup",
        Key::PageDown => "pagedown",
        Key::ArrowLeft => "left",
        Key::ArrowRight => "right",
        Key::ArrowUp => "up",
        Key::ArrowDown => "down",
        Key::Space => "space",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.into());
    }
    let n = k.name();
    if let Some(f) = n.strip_prefix('F').filter(|f| f.parse::<u8>().is_ok_and(|f| (1..=12).contains(&f))) {
        return Some(format!("f{f}"));
    }
    (n.chars().count() == 1).then(|| n.to_lowercase())
}

/// The user's input to window `w`: posted on the hidden desktop, or as
/// Blender's events to a Blender Oak runs (with the modifiers held).
fn send(ctx: &egui::Context, w: usize, l: Live) {
    match crate::mcp::blender_run::port_of_window(w) {
        Some(port) => {
            let m = ctx.input(|i| i.modifiers);
            crate::mcp::blender_run::live(port, &l, crate::mcp::blender_run::Mods { shift: m.shift, ctrl: m.ctrl || m.command, alt: m.alt });
        }
        // The user's own window: from here (it isn't on the hidden desktop).
        None if crate::window_tool::is_users(w) => winctl::live_gently(w, l),
        None => winctl::post_to_desktop(move |_| winctl::live(w, l)),
    }
}

impl App {
    /// After the transcript (in `view`) and the previews above it
    /// (`above`): the live view of the shown conversation's window on the
    /// hidden desktop, and expanded over the window when clicked.
    pub(crate) fn window_preview(&mut self, ctx: &egui::Context, t: &Theme, view: Rect, above: Option<Rect>) -> Option<Rect> {
        let conv = self.program_conv()?;
        let window = crate::window_tool::current(&conv).filter(|&w| crate::mcp::blender_run::port_of_window(w).is_none());
        self.program_preview(ctx, t, view, above, &conv, window, "window")
    }

    /// Shared interactive renderer, but a dedicated selection and UI identity
    /// for each protocol/agent/window. Blender never follows WINDOW current.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn program_preview(&mut self, ctx: &egui::Context, t: &Theme, view: Rect, above: Option<Rect>, conv: &str, window: Option<usize>, kind: &str) -> Option<Rect> {
        let key = format!("{kind}:{conv}:{}", window.unwrap_or(0));
        let expanded = window.is_some() && ctx.data(|d| d.get_temp::<bool>(expanded_id(&key))).unwrap_or(false);
        if let Some(w) = window {
            want(ctx, w, expanded);
        }
        let tex = window.and_then(|w| texture(ctx, w));
        let live = tex.is_some();
        let alpha = ctx.animate_bool_with_time(Id::unique(("window-view-fade", key.as_str())), live, FADE);
        let (Some((tex, client)), Some(w)) = (tex.filter(|_| alpha > 0.0), window) else { return None };
        let e = ctx.animate_bool_with_time(Id::unique(("window-view-expand", key.as_str())), expanded, EXPAND);
        let size = tex.size();

        let (col, right, top) = crate::browser_view::preview_slot(ctx, conv, view);
        let top = above.map_or(top, |r| r.bottom() + 10.0);
        let (rect, over) = crate::browser_view::preview_layout(view, col, right, top, (size[0] as u32, size[1] as u32));
        let shown = alpha * (1.0 - e);
        let uv = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
        let title = winctl::window_title(w);
        if shown > 0.0 {
            let id = Id::unique(("window-view", key.as_str()));
            egui::Area::new(id).order(egui::Order::Middle).fixed_pos(rect.min).show(ctx, |ui| {
                let (r, resp) = ui.allocate_exact_size(rect.size(), Sense::click());
                let hover = ui.ctx().animate_bool_with_time(id.with("hover"), resp.hovered(), 0.12);
                let rest = if over { crate::browser_view::REST_ALPHA_OVER } else { crate::browser_view::REST_ALPHA };
                let mut p = ui.painter().clone();
                p.set_opacity(shown * (rest + (1.0 - rest) * hover));
                p.rect_filled(r.translate(vec2(0.0, 3.0)).expand(1.0), 6.0, Color32::from_black_alpha(70));
                p.add(egui::epaint::RectShape::filled(r, 5.0, Color32::WHITE).with_texture(tex.id(), uv));
                p.rect_stroke(r, 5.0, Stroke::new(1.0, if hover > 0.5 { t.ink3 } else { t.line }), egui::StrokeKind::Inside);
                // Which window this is, in its corner.
                let name: String = if title.chars().count() > 40 { title.chars().take(39).chain(['…']).collect() } else { title.clone() };
                let galley = p.layout_no_wrap(if name.is_empty() { "Window".into() } else { name }, egui::FontId::proportional(11.0), Color32::WHITE);
                let tag = Rect::from_min_size(r.left_bottom() + vec2(6.0, -6.0 - galley.size().y - 4.0), galley.size() + vec2(10.0, 4.0));
                p.rect_filled(tag, 4.0, Color32::from_black_alpha(150));
                p.galley(tag.min + vec2(5.0, 2.0), galley, Color32::WHITE);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("An app on Oak's hidden desktop, live\nClick to expand and use it").clicked() {
                    ui.ctx().data_mut(|d| d.insert_temp(expanded_id(&key), true));
                }
            });
        }
        if e > 0.0 {
            window_lightbox(ctx, t, &key, w, &title, &tex, client, rect, e);
        }
        (shown > 0.0).then_some(rect)
    }
}

/// The view expanded over the window (`e`: how far, from `from`), taking
/// the user's input for the app once fully open.
#[allow(clippy::too_many_arguments)]
fn window_lightbox(ctx: &egui::Context, t: &Theme, conv: &str, w: usize, title: &str, tex: &TextureHandle, client: (u32, u32), from: Rect, e: f32) {
    let screen = ctx.content_rect();
    let focus = Id::unique(("window-view-input", conv));
    let close = |ctx: &egui::Context| {
        ctx.data_mut(|d| d.insert_temp(expanded_id(conv), false));
        ctx.memory_mut(|m| m.surrender_focus(focus));
    };
    let room = screen.shrink2(vec2((screen.width() * 0.04).max(24.0), (screen.height() * 0.05).max(28.0)));
    let (cw, ch) = (client.0.max(1) as f32, client.1.max(1) as f32);
    // Never larger than the window itself (one pixel per pixel at most).
    let k = (room.width() / cw).min((room.height() - 28.0) / ch).min(1.0 / ctx.pixels_per_point());
    let full = Rect::from_center_size(egui::pos2(room.center().x, room.center().y - 14.0), vec2(cw * k, ch * k));
    let r = Rect::from_min_max(from.min.lerp(full.min, e), from.max.lerp(full.max, e));
    let uv = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    egui::Area::new(Id::unique(("window-lightbox", conv))).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        let (bg, resp) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(bg, 0.0, Color32::from_black_alpha((170.0 * e) as u8));
        p.rect_filled(r.translate(vec2(0.0, 4.0)).expand(2.0), 8.0, Color32::from_black_alpha((90.0 * e) as u8));
        p.add(egui::epaint::RectShape::filled(r, 4.0, Color32::WHITE).with_texture(tex.id(), uv));
        let caption = format!("{title}, on Oak's hidden desktop: use it here (Esc goes to the app; Esc beside it or the X returns)");
        p.text(egui::pos2(r.center().x, r.bottom() + 16.0), egui::Align2::CENTER_CENTER, caption, egui::FontId::proportional(12.5), t.ink2.gamma_multiply(e));
        let x = Rect::from_center_size(r.right_top() + vec2(14.0, -14.0), vec2(26.0, 26.0));
        let xr = ui.interact(x, ui.scope_id().with("x"), Sense::click());
        p.circle_filled(x.center(), 13.0, Color32::from_black_alpha(if xr.hovered() { 220 } else { 160 }));
        let d = 5.0;
        let s = Stroke::new(1.6, Color32::WHITE);
        p.line_segment([x.center() + vec2(-d, -d), x.center() + vec2(d, d)], s);
        p.line_segment([x.center() + vec2(-d, d), x.center() + vec2(d, -d)], s);
        if xr.on_hover_text("Back to the preview").clicked() {
            close(ui.ctx());
            return;
        }
        if e < 1.0 {
            return;
        }
        // Input, from here on: the picture's points as the window's pixels.
        let to_client = |pos: egui::Pos2| {
            let v = (pos - r.min) / r.size();
            ((v.x * cw).round() as i32, (v.y * ch).round() as i32)
        };
        let on_picture = |pos: egui::Pos2| r.contains(pos);
        let pressed_id = Id::unique(("window-view-pressed", conv));
        let mut pressed = ui.data(|d| d.get_temp::<bool>(pressed_id)).unwrap_or(false);
        ui.memory_mut(|m| {
            m.request_focus(focus);
            m.set_focus_lock_filter(focus, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true });
        });
        let events = ui.input(|i| i.events.clone());
        let hover = ui.input(|i| i.pointer.hover_pos());
        let mut last_move = None;
        let mut leave = false;
        for ev in &events {
            match ev {
                egui::Event::PointerMoved(pos) if on_picture(*pos) || pressed => last_move = Some(*pos),
                egui::Event::PointerButton { pos, button, pressed: down, .. } if on_picture(*pos) || (pressed && !down) => {
                    let (x, y) = to_client(*pos);
                    let button = match button {
                        egui::PointerButton::Secondary => 1,
                        egui::PointerButton::Middle => 2,
                        _ => 0,
                    };
                    let buttons = held(ui.ctx());
                    if *down {
                        pressed = true;
                    } else if buttons == 0 {
                        pressed = false;
                    }
                    let down = *down;
                    send(ui.ctx(), w, Live::Button { x, y, button, down, buttons });
                }
                egui::Event::PointerButton { pos, pressed: false, .. } if !on_picture(*pos) && !x.contains(*pos) => leave = true,
                egui::Event::MouseWheel { unit, delta, .. } => {
                    let Some(at) = hover.filter(|p| on_picture(*p)) else { continue };
                    let lines = match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / 40.0,
                        egui::MouseWheelUnit::Page => delta.y * 10.0,
                    };
                    let (x, y) = to_client(at);
                    let delta = (lines * 120.0).round() as i32;
                    if delta != 0 {
                        send(ui.ctx(), w, Live::Wheel { x, y, delta });
                    }
                }
                egui::Event::Text(text) => {
                    for unit in text.encode_utf16().collect::<Vec<_>>() {
                        send(ui.ctx(), w, Live::Char(unit));
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if *key == Key::Escape && !hover.is_some_and(on_picture) {
                        leave = true;
                        continue;
                    }
                    // Plain printable keys come as Text; these are the rest.
                    let printable = key_name(*key).is_some_and(|n| n.chars().count() == 1) || *key == Key::Space;
                    if printable && !modifiers.ctrl && !modifiers.alt && !modifiers.command {
                        continue;
                    }
                    let Some(name) = key_name(*key) else { continue };
                    let mut spec = String::new();
                    if modifiers.ctrl || modifiers.command {
                        spec += "ctrl+";
                    }
                    if modifiers.alt {
                        spec += "alt+";
                    }
                    if modifiers.shift {
                        spec += "shift+";
                    }
                    spec += &name;
                    send(ui.ctx(), w, Live::Key(spec));
                }
                _ => {}
            }
        }
        if let Some(pos) = last_move {
            let (x, y) = to_client(pos);
            let buttons = if pressed { held(ui.ctx()) } else { 0 };
            send(ui.ctx(), w, Live::Move { x, y, buttons });
        }
        // The wheel and keys are the app's: nothing in Oak takes them.
        ui.input_mut(|i| {
            i.smooth_scroll_delta = egui::Vec2::ZERO;
            i.events.retain(|e| !matches!(e, egui::Event::Text(_) | egui::Event::Key { .. }));
        });
        ui.data_mut(|d| d.insert_temp(pressed_id, pressed));
        if hover.is_some_and(on_picture) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
        }
        if leave || (resp.clicked() && !resp.interact_pointer_pos().is_some_and(on_picture)) {
            close(ui.ctx());
        }
    });
}
