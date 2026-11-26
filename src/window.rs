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
    let round = crate::gravity::is(t);
    if round {
        // Antigravity: the shell, no line under it, and the arch before the name.
        p.rect_filled(r, 0.0, crate::gravity::shell());
    } else {
        p.rect_filled(r, 0.0, t.bg);
        p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, t.line));
    }
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
    if round {
        let logo = Rect::from_center_size(egui::pos2(r.left() + 24.0, r.center().y), vec2(20.0, 20.0));
        crate::gravity::paint_logo(&p, logo, None);
        p.text(egui::pos2(logo.right() + 8.0, r.center().y), Align2::LEFT_CENTER, "Oak", FontId::proportional(13.0), t.ink2);
    } else {
        p.text(egui::pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, "Oak", FontId::proportional(12.0), t.ink3);
    }
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

/// The client area of a window without the system's frame, on Windows.
/// winit (with egui-winit, which always asks for the drop shadow on an
/// undecorated window) moves the client area 1 pixel down so the system
/// draws a shadow: its WM_NCCALCSIZE does top += 1 and bottom += 1. The
/// client area then runs 1 pixel past the window's bottom, so the system
/// shows the surface (as tall as the client area) in one row fewer, scaled,
/// which softens every edge and glyph more the further down it is. Each of
/// OAK's windows is subclassed to keep that bottom where it was: the client
/// area is the window less its top row (the 1-pixel strip that brings the
/// shadow, as before), and the surface maps 1:1 to the pixels shown.
#[cfg(windows)]
mod client_area {
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, EnumThreadWindows, GWLP_WNDPROC, GetClassNameW, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, IsZoomed,
        NCCALCSIZE_PARAMS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
        WINDOWPLACEMENT, WM_NCCALCSIZE, WM_NCDESTROY,
    };

    /// The windows subclassed so far, each with the procedure it had.
    static SUBCLASSED: Mutex<Vec<(usize, isize)>> = Mutex::new(Vec::new());
    /// winit's window class.
    const CLASS: &str = "Window Class";

    /// Subclasses the UI thread's winit windows that aren't yet (each
    /// frame; a new window is caught on its first).
    pub(super) fn fix() {
        unsafe extern "system" fn each(h: HWND, _: LPARAM) -> i32 {
            let mut buf = [0u16; 64];
            let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
            if String::from_utf16_lossy(&buf[..n.max(0) as usize]) == CLASS {
                subclass(h);
            }
            1
        }
        unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(each), 0) };
    }

    fn subclass(h: HWND) {
        {
            let mut done = SUBCLASSED.lock().unwrap();
            if done.iter().any(|(w, _)| *w == h as usize) {
                return;
            }
            let old = unsafe { GetWindowLongPtrW(h, GWLP_WNDPROC) };
            if old == 0 {
                return;
            }
            done.push((h as usize, old));
            unsafe { SetWindowLongPtrW(h, GWLP_WNDPROC, proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT as usize as isize) };
        }
        // winit made the window as tall as the client area asked for (the
        // saved size, `geometry::restore`), which is now a row less: the
        // window grows by that row (its normal place, if maximized), so the
        // client area is the size asked for and the saved size holds. Then
        // the frame again, now through `proc`.
        unsafe {
            if IsZoomed(h) != 0 {
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                if GetWindowPlacement(h, &mut wp) != 0 {
                    wp.rcNormalPosition.bottom += 1;
                    SetWindowPlacement(h, &wp);
                }
            }
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(h, &mut r);
            let grow = if IsZoomed(h) != 0 { 0 } else { 1 };
            SetWindowPos(h, std::ptr::null_mut(), 0, 0, r.right - r.left, r.bottom - r.top + grow, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    unsafe extern "system" fn proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        let Some(old) = SUBCLASSED.lock().unwrap().iter().find(|(win, _)| *win == h as usize).map(|(_, p)| *p) else { return 0 };
        // SAFETY: `old` is the window procedure the window had.
        let old: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT = unsafe { std::mem::transmute(old) };
        if msg == WM_NCDESTROY {
            SUBCLASSED.lock().unwrap().retain(|(win, _)| *win != h as usize);
        }
        if msg == WM_NCCALCSIZE && w != 0 {
            let p = l as *mut NCCALCSIZE_PARAMS;
            let before: RECT = unsafe { (*p).rgrc[0] };
            let r = unsafe { CallWindowProcW(Some(old), h, msg, w, l) };
            let after = unsafe { &mut (*p).rgrc[0] };
            let winit = (after.top, after.bottom);
            if after.top == before.top + 1 && after.bottom == before.bottom + 1 {
                after.bottom = before.bottom;
            }
            if std::env::var_os("OAK_FRAME_LOG").is_some() {
                let line = format!("window y{}..{}, winit's client y{}..{}, ours y{}..{}\n", before.top, before.bottom, winit.0, winit.1, after.top, after.bottom);
                let _ = std::fs::OpenOptions::new().create(true).append(true).open(crate::store::dir().join("frame.log")).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
            }
            return r;
        }
        unsafe { CallWindowProcW(Some(old), h, msg, w, l) }
    }
}

/// Resizing from the window's edges, which the system no longer handles
/// without its frame: the pointer near an edge shows the resize cursor,
/// and pressing there starts the system's resize.
pub(crate) fn window_edges(ctx: &egui::Context) {
    use egui::{CursorIcon as C, ResizeDirection as D};
    #[cfg(windows)]
    client_area::fix();
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
