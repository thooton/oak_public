//! The window's own chrome, since OAK draws it: the title bar, resizing
//! from the edges, and on Linux the icon and desktop entry.

use super::*;

/// OAK's icon, as a PNG and as the SVG it is drawn from.
pub(crate) const ICON_PNG: &[u8] = include_bytes!("../assets/oak-icon.png");
#[cfg(target_os = "linux")]
const ICON_SVG: &[u8] = include_bytes!("../assets/oak-icon.svg");

/// On Linux, a desktop shows a window's icon from the .desktop file named
/// after its app id ("oak"): Wayland has no window icons, so without one
/// Plasma shows a generic "W". OAK puts its icon in the user's icon theme
/// and, if there is no oak.desktop yet, adds one that runs this binary (as
/// linux/install.sh does). An oak.desktop from before OAK had an icon gets
/// the icon.
#[cfg(target_os = "linux")]
pub(crate) fn install_desktop_entry() {
    /// `text` with every line starting with `prefix` replaced by `line`.
    fn set_line(text: &str, prefix: &str, line: &str) -> String {
        text.lines().map(|l| if l.starts_with(prefix) { line } else { l }).collect::<Vec<_>>().join("\n") + "\n"
    }
    let Some(data) = dirs::data_dir() else { return };
    let mut changed = false;
    let mut put = |path: std::path::PathBuf, bytes: &[u8]| {
        if std::fs::read(&path).ok().as_deref() != Some(bytes) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            changed |= std::fs::write(&path, bytes).is_ok();
        }
    };
    let icons = data.join("icons/hicolor");
    put(icons.join("256x256/apps/oak.png"), ICON_PNG);
    put(icons.join("scalable/apps/oak.svg"), ICON_SVG);
    let desktop = data.join("applications/oak.desktop");
    let entry = match std::fs::read_to_string(&desktop) {
        Ok(old) => set_line(&old, "Icon=", "Icon=oak"),
        Err(_) => {
            let Ok(exe) = std::env::current_exe() else { return };
            set_line(include_str!("../linux/oak.desktop"), "Exec=", &format!("Exec=\"{}\"", exe.display()))
        }
    };
    put(desktop, entry.as_bytes());
    if changed {
        // Plasma's app list (where its taskbar finds icons) catches up now
        // rather than some time after the window shows.
        let _ = std::process::Command::new("kbuildsycoca6").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
}
/// Zed's title bar on Windows: 32 points tall, 36-point caption buttons.
pub(crate) const TITLE_H: f32 = 32.0;
const CAPTION_W: f32 = 36.0;

/// The window's own title bar (the system's is turned off): drag it to
/// move the window, double-click to maximize, and minimize, maximize and
/// close at the right.
pub(crate) fn title_bar(ui: &mut Ui, t: &Theme) {
    let r = ui.max_rect();
    let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
    let p = ui.painter().clone();
    p.rect_filled(r, 0.0, t.bg);
    p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, t.line));
    let close = Rect::from_min_max(egui::pos2(r.right() - CAPTION_W, r.top()), r.max);
    let max = close.translate(vec2(-CAPTION_W, 0.0));
    let min = max.translate(vec2(-CAPTION_W, 0.0));
    let drag = Rect::from_min_max(r.min, egui::pos2(min.left(), r.bottom()));
    let resp = ui.interact(drag, Id::unique("title-drag"), Sense::click_and_drag());
    if resp.double_clicked() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    } else if resp.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    p.text(egui::pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, "Oak", FontId::proportional(12.0), t.ink3);
    for (i, rect) in [min, max, close].into_iter().enumerate() {
        let resp = ui.interact(rect, Id::unique(("caption", i)), Sense::click());
        let (bg, fg) = match (i, resp.is_pointer_button_down_on(), resp.hovered()) {
            (2, true, _) => (Color32::from_rgb(232, 17, 32).gamma_multiply(0.8), Color32::WHITE),
            (2, _, true) => (Color32::from_rgb(232, 17, 32), Color32::WHITE),
            (_, true, _) => (t.panel2, t.ink),
            (_, _, true) => (t.panel, t.ink),
            _ => (Color32::TRANSPARENT, t.ink2),
        };
        p.rect_filled(rect, 0.0, bg);
        let c = rect.center();
        let s = Stroke::new(1.0, fg);
        match i {
            0 => {
                p.hline(c.x - 5.0..=c.x + 5.0, c.y + 0.5, s);
            }
            1 if maximized => {
                p.rect_stroke(Rect::from_center_size(c + vec2(-1.0, 1.0), vec2(8.0, 8.0)), 0.0, s, egui::StrokeKind::Inside);
                p.hline(c.x - 3.0..=c.x + 5.0, c.y - 4.5, s);
                p.vline(c.x + 4.5, c.y - 5.0..=c.y + 3.0, s);
            }
            1 => {
                p.rect_stroke(Rect::from_center_size(c, vec2(10.0, 10.0)), 0.0, s, egui::StrokeKind::Inside);
            }
            _ => {
                p.line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], s);
                p.line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], s);
            }
        }
        if resp.clicked() {
            ui.ctx().send_viewport_cmd(match i {
                0 => egui::ViewportCommand::Minimized(true),
                1 => egui::ViewportCommand::Maximized(!maximized),
                _ => egui::ViewportCommand::Close,
            });
        }
    }
}

/// Resizing from the window's edges, which the system no longer handles
/// without its frame: the pointer near an edge shows the resize cursor,
/// and pressing there starts the system's resize.
pub(crate) fn window_edges(ctx: &egui::Context) {
    use egui::{CursorIcon as C, ResizeDirection as D};
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)) {
        return;
    }
    let Some(p) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    let r = ctx.content_rect();
    let e = 5.0;
    let corner = 12.0;
    let (l, rt, tp, b) = (p.x - r.left() < e, r.right() - p.x < e, p.y - r.top() < e, r.bottom() - p.y < e);
    let (lc, rc, tc, bc) = (p.x - r.left() < corner, r.right() - p.x < corner, p.y - r.top() < corner, r.bottom() - p.y < corner);
    let dir = match () {
        _ if (tp && lc) || (l && tc) => Some((D::NorthWest, C::ResizeNorthWest)),
        _ if (tp && rc) || (rt && tc) => Some((D::NorthEast, C::ResizeNorthEast)),
        _ if (b && lc) || (l && bc) => Some((D::SouthWest, C::ResizeSouthWest)),
        _ if (b && rc) || (rt && bc) => Some((D::SouthEast, C::ResizeSouthEast)),
        _ if tp => Some((D::North, C::ResizeNorth)),
        _ if b => Some((D::South, C::ResizeSouth)),
        _ if l => Some((D::West, C::ResizeWest)),
        _ if rt => Some((D::East, C::ResizeEast)),
        _ => None,
    };
    if let Some((d, cursor)) = dir {
        ctx.set_cursor_icon(cursor);
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(d));
        }
    }
}
