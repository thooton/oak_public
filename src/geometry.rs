//! The window's size, place and maximized state, kept in `window.json` so
//! Oak opens as it was closed.
//!
//! What is kept is the window's normal (not maximized) geometry in logical
//! pixels, which don't change with Oak's zoom, with the monitor scale it was
//! on and whether it was maximized. A state is taken only once it has held
//! for a second, so the in-between frames of a maximize or a drag (where the
//! size and the maximized flag can disagree) are never kept.
//!
//! On opening, a place that no monitor shows any more (a monitor unplugged,
//! the layout or resolution changed) is dropped, so the system places the
//! window; one partly off its monitor is moved onto it, shrunk to fit if
//! need be. A window closed maximized opens maximized, on the monitor it
//! was on, and still restores to its normal size and place. The monitors are the system's work areas on Windows and RandR's
//! monitors on X11. Wayland doesn't let apps place their windows, so there
//! only the size and the maximized state come back.

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The size of a first window, in logical pixels.
pub(crate) const DEFAULT: [f32; 2] = [1200.0, 800.0];
/// The smallest the window may be.
pub(crate) const MIN: [f32; 2] = [760.0, 480.0];
/// The largest size taken from the file (anything bigger is damage).
const MAX: f32 = 16384.0;
/// How long a state must hold before it is kept.
const SETTLE: Duration = Duration::from_secs(1);
/// How long, and how often, a window opened maximized is asked to be.
const MAXIMIZE_FOR: Duration = Duration::from_secs(3);
const MAXIMIZE_EVERY: Duration = Duration::from_millis(200);

/// `window.json`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub(crate) struct Geometry {
    /// The normal window's inner size, in logical pixels.
    pub width: f32,
    pub height: f32,
    /// Its top-left corner on the desktop, in logical pixels; missing
    /// where the system doesn't tell (Wayland).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    /// Physical pixels per logical pixel where the window was.
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub maximized: bool,
}

fn one() -> f32 {
    1.0
}

/// A monitor's usable area, in physical pixels on the desktop.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area {
    fn overlap(&self, o: &Area) -> f32 {
        let w = (self.x + self.w).min(o.x + o.w) - self.x.max(o.x);
        let h = (self.y + self.h).min(o.y + o.h) - self.y.max(o.y);
        w.max(0.0) * h.max(0.0)
    }
}

fn path() -> std::path::PathBuf {
    crate::store::dir().join("window.json")
}

/// The window to open: the saved size, place and maximized state on
/// `builder`, made to fit the monitors there are now; the default size
/// when nothing was saved.
pub(crate) fn restore(builder: egui::ViewportBuilder) -> egui::ViewportBuilder {
    let builder = builder.with_min_inner_size(MIN);
    let Some(saved) = crate::store::read_json::<Geometry>(&path()) else {
        return builder.with_inner_size(DEFAULT);
    };
    let g = place(saved, &monitors());
    *TRACK.lock().unwrap() = Track { saved: Some(saved), maximize: g.maximized.then(|| (Instant::now(), None)), ..Default::default() };
    let mut builder = builder.with_inner_size([g.width, g.height]).with_maximized(g.maximized);
    if let (Some(x), Some(y)) = (g.x, g.y) {
        builder = builder.with_position([x, y]);
    }
    builder
}

/// `g` made safe to open with: a sane size, and a place on one of
/// `monitors` (or none, for the system to choose). With no monitors known
/// the place is kept as it is.
pub(crate) fn place(mut g: Geometry, monitors: &[Area]) -> Geometry {
    let sane = |v: f32, min: f32, default: f32| if v.is_finite() && v > 0.0 { v.clamp(min, MAX) } else { default };
    g.width = sane(g.width, MIN[0], DEFAULT[0]);
    g.height = sane(g.height, MIN[1], DEFAULT[1]);
    if !(g.scale.is_finite() && (0.25..=8.0).contains(&g.scale)) {
        g.scale = 1.0;
    }
    let (Some(x), Some(y)) = (g.x.filter(|v| v.is_finite()), g.y.filter(|v| v.is_finite())) else {
        g.x = None;
        g.y = None;
        return g;
    };
    if monitors.is_empty() {
        return g;
    }
    let s = g.scale;
    let win = Area { x: x * s, y: y * s, w: g.width * s, h: g.height * s };
    let best = monitors.iter().map(|m| (m, win.overlap(m))).max_by(|a, b| a.1.total_cmp(&b.1));
    let Some((m, _)) = best.filter(|(_, overlap)| *overlap > 0.0) else {
        g.x = None;
        g.y = None;
        return g;
    };
    // On its monitor whole: shrunk to fit it (never below the least size),
    // then moved in from any edge it crosses, keeping the top left in view.
    g.width = g.width.min(m.w / s).max(MIN[0]);
    g.height = g.height.min(m.h / s).max(MIN[1]);
    let (w, h) = (g.width * s, g.height * s);
    g.x = Some(win.x.min(m.x + m.w - w).max(m.x) / s);
    g.y = Some(win.y.min(m.y + m.h - h).max(m.y) / s);
    g
}

/// What `track` has seen.
#[derive(Default)]
struct Track {
    /// The state on screen, and since when.
    seen: Option<(Seen, Instant)>,
    /// The state to keep.
    kept: Option<Geometry>,
    /// What is in the file.
    saved: Option<Geometry>,
    /// Maximizing the window on opening, since when, and when it was last
    /// asked: X11 window managers ignore it on a window not yet shown (as
    /// eframe's is until its first frame), so it is asked again until it
    /// takes.
    maximize: Option<(Instant, Option<Instant>)>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Seen {
    maximized: bool,
    /// The normal geometry, when the window is neither maximized nor full screen.
    normal: Option<Geometry>,
}

static TRACK: Mutex<Track> = Mutex::new(Track { seen: None, kept: None, saved: None, maximize: None });

impl Track {
    /// Takes the state seen as the one to keep.
    fn settle(&mut self) {
        let Some((seen, _)) = self.seen else { return };
        let base = self.kept.or(self.saved);
        let mut g = match (seen.normal, base) {
            (Some(n), _) => n,
            (None, Some(b)) => b,
            // Maximized from the start, with no normal size known yet.
            (None, None) => Geometry { width: DEFAULT[0], height: DEFAULT[1], x: None, y: None, scale: 1.0, maximized: false },
        };
        g.maximized = seen.maximized;
        self.kept = Some(g);
    }

    fn write(&mut self) {
        if let Some(g) = self.kept
            && self.saved != Some(g)
            && crate::store::write_json(&path(), &g).is_ok()
        {
            self.saved = Some(g);
        }
    }
}

/// Watches the window, each frame, and saves its state once it settles.
pub(crate) fn track(ctx: &egui::Context) {
    let zoom = ctx.zoom_factor();
    let (info, rect) = ctx.input(|i| (i.viewport().clone(), i.viewport_rect()));
    // Minimized has no geometry, and full screen keeps the state from
    // before it.
    if info.minimized == Some(true) || info.fullscreen == Some(true) {
        return;
    }
    let maximized = info.maximized.unwrap_or(false);
    let normal = (!maximized).then(|| {
        // Points are logical pixels divided by the zoom.
        let corner = info.outer_rect.map(|r| r.min * zoom);
        Geometry {
            width: rect.width() * zoom,
            height: rect.height() * zoom,
            x: corner.map(|p| p.x),
            y: corner.map(|p| p.y),
            scale: info.native_pixels_per_point.unwrap_or(1.0),
            maximized: false,
        }
    });
    let seen = Seen { maximized, normal };
    let mut t = TRACK.lock().unwrap();
    let now = Instant::now();
    if let Some((since, asked)) = t.maximize {
        if maximized || now.duration_since(since) > MAXIMIZE_FOR {
            t.maximize = None;
        } else {
            // Not yet: what is on screen isn't the state to keep.
            if asked.is_none_or(|at| now.duration_since(at) >= MAXIMIZE_EVERY) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                t.maximize = Some((since, Some(now)));
            }
            ctx.request_repaint_after(MAXIMIZE_EVERY);
            return;
        }
    }
    match t.seen {
        Some((s, since)) if same(&s, &seen) => {
            if now.duration_since(since) >= SETTLE {
                t.settle();
                t.write();
            } else {
                ctx.request_repaint_after(SETTLE - now.duration_since(since));
            }
        }
        _ => {
            t.seen = Some((seen, now));
            ctx.request_repaint_after(SETTLE);
        }
    }
}

/// The same state, give or take rounding.
fn same(a: &Seen, b: &Seen) -> bool {
    let near = |a: f32, b: f32| (a - b).abs() < 0.5;
    let near_opt = |a: Option<f32>, b: Option<f32>| match (a, b) {
        (Some(a), Some(b)) => near(a, b),
        (a, b) => a.is_none() && b.is_none(),
    };
    a.maximized == b.maximized
        && match (a.normal, b.normal) {
            (Some(a), Some(b)) => near(a.width, b.width) && near(a.height, b.height) && near_opt(a.x, b.x) && near_opt(a.y, b.y),
            (a, b) => a.is_none() && b.is_none(),
        }
}

/// Saves the latest state, settled or not: on the way out.
pub(crate) fn save_now() {
    let mut t = TRACK.lock().unwrap();
    t.settle();
    t.write();
}

/// The monitors' usable areas, or none when they can't be known.
#[cfg(windows)]
fn monitors() -> Vec<Area> {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
    use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    use windows_sys::core::BOOL;
    unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
        // SAFETY: `list` is the Vec passed to EnumDisplayMonitors below,
        // alive for the whole call.
        let list = unsafe { &mut *(list as *mut Vec<Area>) };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, rcMonitor: RECT { left: 0, top: 0, right: 0, bottom: 0 }, rcWork: RECT { left: 0, top: 0, right: 0, bottom: 0 }, dwFlags: 0 };
        // SAFETY: `info` is a MONITORINFO with its size set.
        if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
            let r = info.rcWork;
            list.push(Area { x: r.left as f32, y: r.top as f32, w: (r.right - r.left) as f32, h: (r.bottom - r.top) as f32 });
        }
        1
    }
    let mut list: Vec<Area> = Vec::new();
    // In real pixels, as winit (which asks for the same awareness when it
    // starts) places the window.
    // SAFETY: plain Win32 calls; the callback only touches `list`.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut list as *mut Vec<Area> as LPARAM);
    }
    list.retain(|a| a.w > 0.0 && a.h > 0.0);
    list
}

/// The monitors RandR reports on X11; none on Wayland, which places
/// windows itself.
#[cfg(target_os = "linux")]
fn monitors() -> Vec<Area> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt;
    let wayland = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"].iter().any(|v| std::env::var_os(v).is_some_and(|v| !v.is_empty()));
    if wayland {
        return Vec::new();
    }
    let Ok((conn, screen)) = x11rb::xcb_ffi::XCBConnection::connect(None) else { return Vec::new() };
    let root = &conn.setup().roots[screen];
    let whole = vec![Area { x: 0.0, y: 0.0, w: f32::from(root.width_in_pixels), h: f32::from(root.height_in_pixels) }];
    let Ok(reply) = conn.randr_get_monitors(root.root, true).map_err(drop).and_then(|c| c.reply().map_err(drop)) else { return whole };
    let list: Vec<Area> = reply
        .monitors
        .iter()
        .map(|m| Area { x: f32::from(m.x), y: f32::from(m.y), w: f32::from(m.width), h: f32::from(m.height) })
        .filter(|a| a.w > 0.0 && a.h > 0.0)
        .collect();
    if list.is_empty() { whole } else { list }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn monitors() -> Vec<Area> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(width: f32, height: f32, x: Option<f32>, y: Option<f32>) -> Geometry {
        Geometry { width, height, x, y, scale: 1.0, maximized: false }
    }
    const FHD: Area = Area { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
    const RIGHT: Area = Area { x: 1920.0, y: 0.0, w: 2560.0, h: 1440.0 };

    #[test]
    fn a_place_on_a_monitor_is_kept() {
        let saved = g(1000.0, 700.0, Some(2200.0), Some(100.0));
        assert_eq!(place(saved, &[FHD, RIGHT]), saved);
        assert_eq!(place(saved, &[]), saved);
    }

    #[test]
    fn a_place_on_a_monitor_now_gone_is_dropped() {
        let p = place(g(1000.0, 700.0, Some(2200.0), Some(100.0)), &[FHD]);
        assert_eq!((p.x, p.y, p.width, p.height), (None, None, 1000.0, 700.0));
    }

    #[test]
    fn a_window_partly_off_its_monitor_is_moved_on_and_shrunk() {
        let p = place(g(1000.0, 700.0, Some(1500.0), Some(-50.0)), &[FHD]);
        assert_eq!((p.x, p.y), (Some(920.0), Some(0.0)));
        // A resolution made smaller: shrunk to the monitor.
        let small = Area { x: 0.0, y: 0.0, w: 1280.0, h: 720.0 };
        let p = place(g(1800.0, 1000.0, Some(100.0), Some(50.0)), &[small]);
        assert_eq!((p.x, p.y, p.width, p.height), (Some(0.0), Some(0.0), 1280.0, 720.0));
    }

    #[test]
    fn scale_turns_logical_into_physical_pixels() {
        // At 150%, logical (1400, 100) is physical (2100, 150): on the right monitor.
        let saved = Geometry { scale: 1.5, ..g(1000.0, 700.0, Some(1400.0), Some(100.0)) };
        assert_eq!(place(saved, &[FHD, RIGHT]), saved);
        // Physical (1500, 150) at 150% for 1000x700 crosses FHD's right edge.
        let p = place(Geometry { scale: 1.5, ..g(1000.0, 700.0, Some(1000.0), Some(100.0)) }, &[FHD]);
        assert_eq!(p.x, Some(280.0));
    }

    #[test]
    fn damaged_values_fall_back() {
        let p = place(Geometry { scale: f32::NAN, ..g(f32::NAN, 5.0, Some(f32::INFINITY), Some(0.0)) }, &[FHD]);
        assert_eq!(p, g(DEFAULT[0], MIN[1], None, None));
        assert_eq!(place(g(1e9, 1e9, None, None), &[]).width, MAX);
    }

    #[test]
    fn old_and_partial_files_read() {
        let g: Geometry = serde_json::from_str(r#"{"width": 900, "height": 600}"#).unwrap();
        assert_eq!((g.x, g.scale, g.maximized), (None, 1.0, false));
    }

    #[test]
    fn a_maximized_window_keeps_its_normal_size() {
        let mut t = Track::default();
        let normal = g(1000.0, 700.0, Some(10.0), Some(20.0));
        t.seen = Some((Seen { maximized: false, normal: Some(normal) }, Instant::now()));
        t.settle();
        t.seen = Some((Seen { maximized: true, normal: None }, Instant::now()));
        t.settle();
        assert_eq!(t.kept, Some(Geometry { maximized: true, ..normal }));
    }
}
