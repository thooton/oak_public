//! The taskbar widget (Windows): when Oak closes with background tasks
//! left running, an icon in the taskbar's notification area shows how many
//! still run, as a tall number, until Oak starts again (which closes it) or
//! they have all ended (it says so in a notification, then goes). Clicking
//! it opens a list of the tasks: what each is, its conversation, how long
//! it has run or how it ended, and its last line of output; clicking a task
//! opens its output file. The right-click menu opens Oak or hides the icon
//! (the tasks keep running).
//!
//! It is `oak --tasks-tray`, run from a copy of oak.exe in Oak's folder
//! (`tray/Oak tasks.exe`: the name Windows gives its icon and notification),
//! so no process holds oak.exe itself, which a rebuild or an update has to
//! replace. What it shows comes from
//! `tasks/tray.json`, written as Oak closes: the tasks (`tasks::Kept`), their
//! conversations' titles and the theme's colors. It watches each task's bash
//! (by PID and start) and reads the exit code bash writes on the way out.
//! Plain Win32 and GDI, no GPU: it may sit there for hours.
//!
//! Windows 11 puts a program's new icon in the overflow flyout; the widget
//! moves its own onto the taskbar the first time it shows (`promote`), and
//! leaves it where the user puts it after that.

use crate::tasks::Kept;
use crate::theme::Theme;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// The hidden window that owns the icon (and that Oak closes).
const CLASS: &str = "OakTasksTray";
/// The list of tasks.
const POPUP_CLASS: &str = "OakTasksPopup";
/// The icon's messages to its window.
const WM_TRAY: u32 = WM_APP + 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// The once-a-second look at the tasks.
const TICK: usize = 1;
/// How long the icon stays after the last task ended, at most (the
/// notification saying so goes first, usually).
const LINGER: Duration = Duration::from_secs(20);
/// The most tasks the list shows (the rest are counted under them).
const MAX_ROWS: usize = 8;

/// What the widget shows, as Oak left it (`tasks/tray.json`).
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Manifest {
    /// Oak's executable, for "Open Oak".
    oak: PathBuf,
    /// The conversations' titles, by id.
    titles: BTreeMap<String, String>,
    colors: Colors,
    /// The interface font's regular and bold files, when it is a plugin's
    /// (none: IBM Plex Sans, built in).
    faces: Vec<PathBuf>,
    tasks: Vec<Kept>,
}

/// The theme's colors the list is painted with, as 0xRRGGBB.
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(default)]
struct Colors {
    panel: u32,
    hover: u32,
    line: u32,
    ink: u32,
    ink2: u32,
    ink3: u32,
    ink4: u32,
    accent: u32,
    error: u32,
}

impl Default for Colors {
    fn default() -> Self {
        Colors::from(&Theme::oak())
    }
}

impl From<&Theme> for Colors {
    fn from(t: &Theme) -> Self {
        let rgb = |c: eframe::egui::Color32| (c.r() as u32) << 16 | (c.g() as u32) << 8 | c.b() as u32;
        let [r, g, b, a] = t.line.to_srgba_unmultiplied();
        let line = crate::theme::mix(t.panel, eframe::egui::Color32::from_rgb(r, g, b), a as f32 / 255.0);
        Colors {
            panel: rgb(t.panel),
            hover: rgb(crate::theme::mix(t.panel, t.ink, 0.086)),
            line: rgb(line),
            ink: rgb(t.ink),
            ink2: rgb(t.ink2),
            ink3: rgb(t.ink3),
            ink4: rgb(t.ink4),
            accent: rgb(t.accent),
            error: rgb(t.error),
        }
    }
}

fn manifest_path() -> PathBuf {
    crate::store::dir().join("tasks").join("tray.json")
}

/// The titles, colors and font files noted as the close dialog let Oak go
/// (`note`).
type Noted = (BTreeMap<String, String>, Colors, Vec<PathBuf>);
static NOTED: Mutex<Option<Noted>> = Mutex::new(None);

/// As Oak closes: the titles of the conversations with tasks running, the
/// theme and the interface font (its id), for the widget. (A plugin's font
/// is written out now, while plugins are still loaded.)
pub(crate) fn note(titles: Vec<(String, String)>, theme: &Theme, font: &str) {
    *NOTED.lock().unwrap() = Some((titles.into_iter().collect(), Colors::from(theme), save_faces(font)));
}

/// The plugin font `face`'s regular and bold files, written beside the
/// widget (when they changed); none for Plex, or a font not on this machine.
fn save_faces(face: &str) -> Vec<PathBuf> {
    let Some(files) = crate::markdown::plugin_face(face).and_then(|f| f.faces()) else { return vec![] };
    let dir = crate::store::dir().join("tray");
    let _ = std::fs::create_dir_all(&dir);
    let saved: Vec<PathBuf> = files
        .iter()
        .zip(["face-regular", "face-bold"])
        .filter_map(|(bytes, name)| {
            let path = dir.join(name);
            if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                std::fs::write(&path, bytes).ok()?;
            }
            Some(path)
        })
        .collect();
    if saved.len() == 2 { saved } else { vec![] }
}

/// Oak has closed with `kept` left running: the widget starts (in place of
/// one already there, whose tasks still running it takes over).
pub(crate) fn start(kept: Vec<Kept>) {
    let (titles, colors, faces) = NOTED.lock().unwrap().take().unwrap_or_default();
    let mut m = Manifest { oak: crate::update::exe().cloned().unwrap_or_default(), titles, colors, faces, tasks: kept };
    if let Some(old) = crate::store::read_json::<Manifest>(&manifest_path()) {
        for k in old.tasks {
            if !m.tasks.iter().any(|t| t.pid == k.pid) && k.open().map(close_handle).is_some() {
                if let Some(title) = old.titles.get(&k.conv) {
                    m.titles.entry(k.conv.clone()).or_insert_with(|| title.clone());
                }
                m.tasks.push(k);
            }
        }
    }
    let Ok(json) = serde_json::to_vec_pretty(&m) else { return };
    if crate::store::write_atomic(&manifest_path(), json).is_err() {
        return;
    }
    close_running();
    let Some(exe) = widget_exe().or_else(|| std::env::current_exe().ok()) else { return };
    let _ = std::process::Command::new(exe)
        .arg("--tasks-tray")
        .current_dir(crate::store::dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn close_handle(h: usize) {
    unsafe { CloseHandle(h as HANDLE) };
}

/// The widget's own copy of this oak.exe (copied when it differs), so it
/// doesn't hold oak.exe; none when it can't be made.
fn widget_exe() -> Option<PathBuf> {
    let src = std::env::current_exe().ok()?;
    let dest = crate::store::dir().join("tray").join("Oak tasks.exe");
    let stamp = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
    if stamp(&src).is_some() && stamp(&src) == stamp(&dest) {
        return Some(dest);
    }
    std::fs::create_dir_all(dest.parent()?).ok()?;
    let tmp = dest.with_extension("exe.tmp");
    std::fs::copy(&src, &tmp).ok()?;
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(dest)
}

/// Closes the widget, if one is up (Oak is watching the tasks again), and
/// waits a moment for it to go.
pub(crate) fn close_running() {
    let class = wide(CLASS);
    let find = || unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    let mut hwnd = find();
    if hwnd.is_null() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !hwnd.is_null() && Instant::now() < deadline {
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        std::thread::sleep(Duration::from_millis(50));
        hwnd = find();
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// How a task ended.
#[derive(Clone, Copy, PartialEq, Debug)]
enum End {
    /// Its exit code.
    Code(i32),
    /// Killed: no exit code written.
    Stopped,
}

struct Task {
    k: Kept,
    /// Its bash's handle while it runs (0 once it has ended).
    handle: usize,
    /// How and when (ms since 1970) it ended.
    end: Option<(End, i64)>,
    /// Its last line of output (read while the list shows).
    last: String,
}

impl Task {
    fn new(k: Kept) -> Task {
        let handle = k.open().unwrap_or(0);
        let mut t = Task { k, handle, end: None, last: String::new() };
        if handle == 0 {
            t.ended();
        }
        t
    }

    /// Notes how it ended.
    fn ended(&mut self) {
        if self.handle != 0 {
            close_handle(self.handle);
            self.handle = 0;
        }
        self.end = Some(match self.k.exit_code() {
            Some((code, at)) => (End::Code(code), at),
            None => (End::Stopped, crate::api::now_ms()),
        });
    }

    /// Looks whether it has ended; true when it just did.
    fn poll(&mut self) -> bool {
        if self.end.is_some() || unsafe { WaitForSingleObject(self.handle as HANDLE, 0) } != 0 {
            return false;
        }
        self.ended();
        true
    }

    /// What the list calls it: the model's summary, else the command.
    fn name(&self) -> String {
        let s = if self.k.summary.trim().is_empty() { &self.k.command } else { &self.k.summary };
        s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
    }

    /// How long it has run, or how it ended.
    fn status(&self) -> String {
        match self.end {
            None => crate::tasks::fmt_duration(Duration::from_millis((crate::api::now_ms() - self.k.started_ms).max(0) as u64)),
            Some((End::Code(0), _)) => "done".into(),
            Some((End::Code(c), _)) => format!("exit {c}"),
            Some((End::Stopped, _)) => "stopped".into(),
        }
    }
}

/// The last line of output in `log` with anything in it, without terminal
/// escapes (and only what a carriage return left of it).
fn last_line(log: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log) else { return String::new() };
    let len = f.metadata().map_or(0, |m| m.len());
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(8192)));
    let mut bytes = vec![];
    let _ = f.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let line = text
        .lines()
        .rev()
        .map(|l| strip_escapes(l.rsplit('\r').find(|p| !p.trim().is_empty()).unwrap_or("")))
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default();
    line.trim().chars().take(300).collect()
}

/// `s` without ANSI escape sequences (colors, cursor moves) and other
/// control characters (tabs become spaces).
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: up to BEL or ESC \.
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// What is under the pointer in the list.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Task(usize),
    OpenOak,
    Hide,
}

struct Widget {
    m: Manifest,
    tasks: Vec<Task>,
    /// The hidden window that owns the icon.
    hwnd: HWND,
    popup: HWND,
    /// The icon shown, and the count and look it shows.
    icon: HICON,
    shown: Option<(usize, u32, bool)>,
    /// When the list last hid (a click on the icon that hid it doesn't
    /// open it again).
    hidden_at: Option<Instant>,
    hover: Option<Hit>,
    /// Where the list's clickable parts were painted.
    hits: Vec<(RECT, Hit)>,
    /// When the last task ended (the notification went out).
    finished: Option<Instant>,
    /// The notification has gone (or was clicked).
    told: bool,
    /// When the icon was added (`promote` waits for Explorer to note it).
    added: Instant,
    promoted: bool,
}

thread_local! {
    static W: RefCell<Option<Widget>> = const { RefCell::new(None) };
}

/// Runs `f` on the widget, unless it is busy further up the stack (a
/// message sent while it was handling another): then nothing.
fn with<R>(f: impl FnOnce(&mut Widget) -> R) -> Option<R> {
    W.with(|w| w.try_borrow_mut().ok().and_then(|mut w| w.as_mut().map(f)))
}

/// `oak --tasks-tray`: the widget, until Oak closes it, the user hides it
/// or the tasks have all ended.
pub fn main() -> i32 {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let Some(m) = crate::store::read_json::<Manifest>(&manifest_path()) else { return 1 };
    if m.tasks.is_empty() {
        return 0;
    }
    load_fonts(&m.faces);
    let tasks: Vec<Task> = m.tasks.iter().cloned().map(Task::new).collect();
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let popup_class = wide(POPUP_CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(main_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        wc.lpfnWndProc = Some(popup_proc);
        wc.lpszClassName = popup_class.as_ptr();
        wc.style = CS_DROPSHADOW;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        RegisterClassW(&wc);
        let title = wide("Oak background tasks");
        let null = std::ptr::null_mut();
        // A top-level window (never shown), so that it hears TaskbarCreated.
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, null, null, instance, null);
        let popup = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, popup_class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, null, null, instance, null);
        if hwnd.is_null() || popup.is_null() {
            return 1;
        }
        W.with(|w| {
            *w.borrow_mut() = Some(Widget {
                m,
                tasks,
                hwnd,
                popup,
                icon: null,
                shown: None,
                hidden_at: None,
                hover: None,
                hits: vec![],
                finished: None,
                told: false,
                added: Instant::now(),
                promoted: false,
            })
        });
        with(|w| {
            w.add_icon();
            w.tick();
        });
        SetTimer(hwnd, TICK, 1000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    0
}

/// The message Explorer sends when the taskbar comes back (after Explorer
/// restarted): the icon is to be added again.
fn taskbar_created() -> u32 {
    static MSG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) })
}

impl Widget {
    fn running(&self) -> usize {
        self.tasks.iter().filter(|t| t.end.is_none()).count()
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = self.hwnd;
        d.uID = 1;
        d
    }

    fn add_icon(&mut self) {
        self.shown = None;
        self.render_icon();
        let mut d = self.data();
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon;
        put(&mut d.szTip, &self.tip());
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &d);
            d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &d);
        }
    }

    fn tip(&self) -> String {
        match self.running() {
            0 => "Oak: the background tasks have ended".into(),
            1 => "Oak: 1 background task running".into(),
            n => format!("Oak: {n} background tasks running"),
        }
    }

    /// Draws the icon again when the count, the icon size or the taskbar's
    /// light or dark changed. True when it did.
    fn render_icon(&mut self) -> bool {
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.hwnd)) }.max(16) as u32;
        let key = (self.running(), size, light_taskbar());
        if self.shown == Some(key) {
            return false;
        }
        let color = if key.2 { 0x1a1a1a } else { 0xffffff };
        let icon = make_icon(&number_pixels(key.0, size, color), size as i32);
        if !self.icon.is_null() {
            unsafe { DestroyIcon(self.icon) };
        }
        self.icon = icon;
        self.shown = Some(key);
        true
    }

    fn update_icon(&mut self) {
        if !self.render_icon() {
            return;
        }
        let mut d = self.data();
        d.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.hIcon = self.icon;
        put(&mut d.szTip, &self.tip());
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
    }

    /// The once-a-second look: tasks that ended, the icon, the list, and
    /// whether the widget is done. True when it is to go.
    fn tick(&mut self) -> bool {
        for t in &mut self.tasks {
            t.poll();
        }
        let visible = unsafe { IsWindowVisible(self.popup) } != 0;
        if visible {
            for t in &mut self.tasks {
                t.last = last_line(&t.k.log);
            }
            unsafe { InvalidateRect(self.popup, std::ptr::null(), 0) };
        }
        self.update_icon();
        if !self.promoted && self.added.elapsed() > Duration::from_secs(2) {
            self.promoted = true;
            promote();
        }
        if self.running() == 0 && self.finished.is_none() {
            self.finished = Some(Instant::now());
            self.notify_done();
        }
        let done = self.finished.is_some_and(|at| self.told || at.elapsed() > LINGER);
        done && !visible
    }

    /// The notification that the tasks have all ended, and how.
    fn notify_done(&mut self) {
        let n = self.tasks.len();
        let failed = self.tasks.iter().filter(|t| !matches!(t.end, Some((End::Code(0), _)))).count();
        let title = if n == 1 { "Oak's background task has ended".to_string() } else { format!("Oak's {n} background tasks have ended") };
        let mut body = self.tasks.iter().map(|t| format!("{} ({})", t.name(), t.status())).collect::<Vec<_>>().join("\n");
        if failed == 0 && n > 3 {
            body = format!("All {n} finished.");
        }
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        put(&mut d.szInfoTitle, &title);
        put(&mut d.szInfo, &body);
        d.dwInfoFlags = NIIF_NONE | NIIF_RESPECT_QUIET_TIME;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
    }

    fn open_oak(&self) {
        let mut cmd = std::process::Command::new(&self.m.oak);
        if let Some(dir) = self.m.oak.parent() {
            cmd.current_dir(dir);
        }
        let _ = cmd.spawn();
    }

    /// Where the list goes: by the icon, inside the work area of its
    /// monitor, on the side away from the taskbar. Also its size.
    fn popup_rect(&mut self) -> RECT {
        for t in &mut self.tasks {
            t.last = last_line(&t.k.log);
        }
        let s = scale(unsafe { GetDpiForWindow(self.hwnd) });
        let (w, h) = (px(WIDTH, s), px(list_height(self.tasks.len()), s));
        let id = NOTIFYICONIDENTIFIER { cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32, hWnd: self.hwnd, uID: 1, guidItem: unsafe { std::mem::zeroed() } };
        let mut icon = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { Shell_NotifyIconGetRect(&id, &mut icon) } != 0 {
            let mut p = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut p) };
            icon = RECT { left: p.x, top: p.y, right: p.x, bottom: p.y };
        }
        let center = POINT { x: (icon.left + icon.right) / 2, y: (icon.top + icon.bottom) / 2 };
        let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        unsafe { GetMonitorInfoW(MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST), &mut mi) };
        let work = mi.rcWork;
        let gap = px(8.0, s);
        let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
        let mut x = clamp(center.x - w / 2, work.left + gap, work.right - w - gap);
        let mut y = clamp(center.y - h / 2, work.top + gap, work.bottom - h - gap);
        if center.y >= work.bottom {
            y = work.bottom - h - gap;
        } else if center.y <= work.top {
            y = work.top + gap;
        } else if center.x >= work.right {
            x = work.right - w - gap;
        } else if center.x <= work.left {
            x = work.left + gap;
        }
        RECT { left: x, top: y, right: x + w, bottom: y + h.min(work.bottom - work.top - 2 * gap) }
    }
}

/// Copies `s` into a fixed UTF-16 field, cut to fit, with its terminator.
fn put(field: &mut [u16], s: &str) {
    let units: Vec<u16> = s.encode_utf16().take(field.len() - 1).collect();
    field[..units.len()].copy_from_slice(&units);
    field[units.len()] = 0;
}

/// Whether the taskbar is light (Windows' "system" mode).
fn light_taskbar() -> bool {
    let mut v: u32 = 0;
    let mut n = 4u32;
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("SystemUsesLightTheme");
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut v as *mut u32).cast(), &mut n) } == 0;
    ok && v == 1
}

/// Windows 11 keeps a new program's icon in the overflow flyout: the first
/// time the widget shows, its icon is moved onto the taskbar (in Explorer's
/// list of icons, `IsPromoted`); after that it stays where the user puts it.
fn promote() {
    let marker = crate::store::dir().join("tray").join("promoted");
    if marker.exists() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.to_string_lossy().to_lowercase();
    let root = wide(r"Control Panel\NotifyIconSettings");
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, root.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return;
        }
        let mut i = 0;
        loop {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                break;
            }
            i += 1;
            let mut path = [0u16; 1024];
            let mut size = (path.len() * 2) as u32;
            let value = wide("ExecutablePath");
            if RegGetValueW(key, name.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), path.as_mut_ptr().cast(), &mut size) != 0 {
                continue;
            }
            let path = String::from_utf16_lossy(&path[..path.iter().position(|&c| c == 0).unwrap_or(0)]).to_lowercase();
            if !same_exe(&path, &exe) {
                continue;
            }
            let mut sub: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(key, name.as_ptr(), 0, KEY_SET_VALUE, &mut sub) == 0 {
                let one: u32 = 1;
                let value = wide("IsPromoted");
                if RegSetValueExW(sub, value.as_ptr(), 0, REG_DWORD, (&one as *const u32).cast(), 4) == 0 {
                    let _ = std::fs::write(&marker, b"");
                }
                RegCloseKey(sub);
            }
            break;
        }
        RegCloseKey(key);
    }
}

/// Whether Explorer's `path` for an icon's program is `exe` (lowercase
/// both): Explorer writes a known folder as its GUID ("{3EB685DB-...}\oak\..."
/// for the roaming AppData), so then only what follows has to match.
fn same_exe(path: &str, exe: &str) -> bool {
    if path == exe {
        return true;
    }
    match path.strip_prefix('{').and_then(|p| p.split_once('}')) {
        Some((_, rest)) => rest.len() > 1 && exe.ends_with(rest),
        None => false,
    }
}

/// The icon's pixels (BGRA, straight alpha, top row first): `n` (99 at
/// most) as tall, condensed digits filling a `size`-pixel square, in `rgb`.
fn number_pixels(n: usize, size: u32, rgb: u32) -> Vec<u8> {
    use ab_glyph::{Font, FontRef, OutlinedGlyph, PxScale, ScaleFont, point};
    let font = FontRef::try_from_slice(crate::markdown::PLEX_BOLD).expect("the bundled font");
    let text = n.min(99).to_string();
    let s = size as f32;
    let layout = |sx: f32, sy: f32, dx: f32, base: f32| -> Vec<OutlinedGlyph> {
        let scaled = font.as_scaled(PxScale { x: sx, y: sy });
        let mut x = dx;
        let mut out = vec![];
        for c in text.chars() {
            let id = font.glyph_id(c);
            out.extend(font.outline_glyph(id.with_scale_and_position(PxScale { x: sx, y: sy }, point(x, base))));
            x += scaled.h_advance(id);
        }
        out
    };
    let bounds = |gs: &[OutlinedGlyph]| {
        gs.iter().map(|g| g.px_bounds()).fold((f32::MAX, f32::MAX, f32::MIN, f32::MIN), |b, r| (b.0.min(r.min.x), b.1.min(r.min.y), b.2.max(r.max.x), b.3.max(r.max.y)))
    };
    // Measured at a 100-pixel em, then scaled: as tall as the icon (less a
    // pixel at 16), and narrowed to fit (never wider than a tall face's).
    let (x0, y0, x1, y1) = bounds(&layout(100.0, 100.0, 0.0, 0.0));
    let height = s - (s / 16.0).floor();
    let sy = 100.0 * height / (y1 - y0);
    let sx = (100.0 * (s - 1.0) / (x1 - x0)).min(sy * 0.62);
    let (x0, y0, x1, y1) = bounds(&layout(sx, sy, 0.0, 0.0));
    // The baseline on a pixel edge, the digits centered.
    let base = ((s + (y1 - y0)) / 2.0 - y1).round();
    let dx = (s - (x1 - x0)) / 2.0 - x0;
    let mut alpha = vec![0f32; (size * size) as usize];
    for g in layout(sx, sy, dx, base) {
        let b = g.px_bounds();
        g.draw(|x, y, c| {
            let (px, py) = (b.min.x as i32 + x as i32, b.min.y as i32 + y as i32);
            if (0..size as i32).contains(&px) && (0..size as i32).contains(&py) {
                let a = &mut alpha[(py as u32 * size + px as u32) as usize];
                *a = (*a + c).min(1.0);
            }
        });
    }
    alpha.iter().flat_map(|&a| [rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8, (a * 255.0).round() as u8]).collect()
}

/// An icon from `number_pixels`.
fn make_icon(pixels: &[u8], size: i32) -> HICON {
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits = std::ptr::null_mut();
        let screen = GetDC(std::ptr::null_mut());
        let color = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        ReleaseDC(std::ptr::null_mut(), screen);
        if color.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len().min((size * size * 4) as usize));
        let stride = ((size + 15) / 16 * 2) as usize;
        let zeros = vec![0u8; stride * size as usize];
        let mask = CreateBitmap(size, size, 1, 1, zeros.as_ptr().cast());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

// ------------------------------------------------------------ the list

/// The list's sizes, in pixels at 96 DPI.
const WIDTH: f32 = 400.0;
const PAD: f32 = 14.0;
const HEADER_H: f32 = 40.0;
const ROW_H: f32 = 64.0;
const MORE_H: f32 = 26.0;
const FOOTER_H: f32 = 40.0;

fn scale(dpi: u32) -> f32 {
    dpi.max(96) as f32 / 96.0
}

fn px(v: f32, s: f32) -> i32 {
    (v * s).round() as i32
}

/// The list's height for `n` tasks.
fn list_height(n: usize) -> f32 {
    HEADER_H + ROW_H * n.min(MAX_ROWS) as f32 + if n > MAX_ROWS { MORE_H } else { 0.0 } + FOOTER_H + 2.0
}

/// GDI colors are 0x00BBGGRR.
fn cref(rgb: u32) -> u32 {
    (rgb & 0xff) << 16 | (rgb & 0xff00) | (rgb >> 16 & 0xff)
}

/// The faces the list is drawn in, as GDI knows them (`load_fonts`): the
/// interface font's family, and the code font's.
static FACES: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Oak's fonts, for this process only: the interface font (the plugin's
/// regular and bold in `faces`, else IBM Plex Sans) and Oak's code font
/// (egui's Hack). Falls back to Segoe UI and Consolas.
fn load_fonts(faces: &[PathBuf]) -> &'static (String, String) {
    FACES.get_or_init(|| {
        let add = |bytes: &[u8]| {
            let n = 0u32;
            unsafe { !AddFontMemResourceEx(bytes.as_ptr().cast(), bytes.len() as u32, std::ptr::null(), &n).is_null() }
        };
        let plugin: Vec<Vec<u8>> = faces.iter().filter_map(|p| std::fs::read(p).ok()).collect();
        let files: Vec<&[u8]> = match plugin.first().and_then(|f| family_name(f)) {
            Some(_) if plugin.len() == 2 => plugin.iter().map(|f| f.as_slice()).collect(),
            _ => vec![crate::markdown::PLEX_REGULAR, crate::markdown::PLEX_BOLD],
        };
        let sans = match family_name(files[0]) {
            Some(family) if files.iter().all(|f| add(f)) => family,
            _ => "Segoe UI".to_string(),
        };
        let defaults = eframe::egui::FontDefinitions::default();
        let mono = match defaults.font_data.get("Hack") {
            Some(hack) if add((*hack.font).as_ref()) => family_name((*hack.font).as_ref()).unwrap_or_else(|| "Consolas".into()),
            _ => "Consolas".to_string(),
        };
        (sans, mono)
    })
}

/// A font file's family, as GDI names it (the Windows name record 1).
fn family_name(font: &[u8]) -> Option<String> {
    let u16_at = |o: usize| font.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize);
    let u32_at = |o: usize| font.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let table = (0..u16_at(4)?).map(|i| 12 + 16 * i).find(|&r| font.get(r..r + 4) == Some(b"name"))?;
    let name = u32_at(table + 8)?;
    let strings = name + u16_at(name + 4)?;
    for i in 0..u16_at(name + 2)? {
        let r = name + 6 + 12 * i;
        if u16_at(r)? == 3 && u16_at(r + 6)? == 1 {
            let (len, at) = (u16_at(r + 8)?, strings + u16_at(r + 10)?);
            let units: Vec<u16> = font.get(at..at + len)?.chunks(2).map(|b| u16::from_be_bytes([b[0], *b.get(1).unwrap_or(&0)])).collect();
            return Some(String::from_utf16_lossy(&units)).filter(|s| !s.is_empty());
        }
    }
    None
}

/// The fonts the list is drawn with, at a scale.
struct Fonts {
    title: HFONT,
    body: HFONT,
    small: HFONT,
    mono: HFONT,
}

impl Fonts {
    /// At Oak's sizes: 14 for text, 11.5 small, code a little smaller.
    fn new(s: f32) -> Fonts {
        let (sans, mono) = FACES.get().map_or(("Segoe UI", "Consolas"), |(a, b)| (a.as_str(), b.as_str()));
        let font = |size: f32, weight: i32, face: &str| {
            let face = wide(face);
            unsafe { CreateFontW(-px(size, s), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, face.as_ptr()) }
        };
        Fonts { title: font(14.5, 700, sans), body: font(14.0, 400, sans), small: font(12.0, 400, sans), mono: font(11.5, 400, mono) }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for f in [self.title, self.body, self.small, self.mono] {
            unsafe { DeleteObject(f) };
        }
    }
}

fn fill(dc: HDC, r: &RECT, rgb: u32) {
    unsafe {
        let b = CreateSolidBrush(cref(rgb));
        FillRect(dc, r, b);
        DeleteObject(b);
    }
}

fn text_width(dc: HDC, font: HFONT, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE { cx: 0, cy: 0 };
    unsafe {
        SelectObject(dc, font);
        GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut size);
    }
    size.cx
}

/// One line of text in `r`, cut with an ellipsis; `right` aligns it right.
fn text(dc: HDC, font: HFONT, rgb: u32, s: &str, mut r: RECT, right: bool) {
    let w: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        SelectObject(dc, font);
        SetTextColor(dc, cref(rgb));
        DrawTextW(dc, w.as_ptr(), w.len() as i32, &mut r, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS | if right { DT_RIGHT } else { DT_LEFT });
    }
}

impl Widget {
    /// Paints the list (`w` by `h` pixels at scale `s`) and notes where its
    /// clickable parts are.
    fn paint(&mut self, dc: HDC, w: i32, h: i32, s: f32) {
        let c = self.m.colors;
        let f = Fonts::new(s);
        let p = |v: f32| px(v, s);
        unsafe { SetBkMode(dc, TRANSPARENT as i32) };
        fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, c.line);
        fill(dc, &RECT { left: 1, top: 1, right: w - 1, bottom: h - 1 }, c.panel);
        self.hits.clear();
        let (left, right) = (p(PAD), w - p(PAD));
        // The header: what this is, and how many still run.
        let mut y = 1;
        let header = RECT { left, top: y, right, bottom: y + p(HEADER_H) };
        let running = self.running();
        let count = match running {
            0 => "all ended".to_string(),
            n => format!("{n} running"),
        };
        let cw = text_width(dc, f.small, &count);
        text(dc, f.small, c.ink3, &count, header, true);
        text(dc, f.title, c.ink, "Oak background tasks", RECT { right: right - cw - p(8.0), ..header }, false);
        y += p(HEADER_H);
        fill(dc, &RECT { left: 1, top: y - 1, right: w - 1, bottom: y }, c.line);
        // The tasks: running ones first, then the last to end first.
        let mut order: Vec<usize> = (0..self.tasks.len()).collect();
        order.sort_by_key(|&i| {
            let t = &self.tasks[i];
            (t.end.is_some(), std::cmp::Reverse(t.end.map_or(t.k.started_ms, |e| e.1)))
        });
        for &i in order.iter().take(MAX_ROWS) {
            let t = &self.tasks[i];
            let row = RECT { left: 1, top: y, right: w - 1, bottom: y + p(ROW_H) };
            if self.hover == Some(Hit::Task(i)) {
                fill(dc, &row, c.hover);
            }
            self.hits.push((row, Hit::Task(i)));
            let (dot, status_color) = match t.end {
                None => (c.accent, c.ink3),
                Some((End::Code(0), _)) => (c.ink3, c.ink3),
                Some(_) => (c.error, c.error),
            };
            let line = |n: i32| RECT { left: left + p(16.0), top: y + p(9.0) + n * p(16.0), right, bottom: y + p(9.0) + (n + 1) * p(16.0) + p(1.0) };
            let l0 = line(0);
            let mid = (l0.top + l0.bottom) / 2;
            fill(dc, &RECT { left, top: mid - p(3.5), right: left + p(7.0), bottom: mid + p(3.5) }, dot);
            let status = t.status();
            let sw = text_width(dc, f.small, &status);
            text(dc, f.small, status_color, &status, l0, true);
            text(dc, f.body, c.ink, &t.name(), RECT { right: right - sw - p(10.0), ..l0 }, false);
            let conv = self.m.titles.get(&t.k.conv).map_or("", |s| s.as_str());
            let second = if t.k.summary.trim().is_empty() { conv.to_string() } else { format!("{conv}: {}", t.k.command.lines().next().unwrap_or("")) };
            text(dc, f.small, c.ink3, &second, line(1), false);
            let last = if t.last.is_empty() { "(no output)" } else { &t.last };
            text(dc, f.mono, c.ink4, last, line(2), false);
            y += p(ROW_H);
        }
        if self.tasks.len() > MAX_ROWS {
            let more = format!("and {} more", self.tasks.len() - MAX_ROWS);
            text(dc, f.small, c.ink3, &more, RECT { left: left + p(16.0), top: y, right, bottom: y + p(MORE_H) }, false);
            y += p(MORE_H);
        }
        // The footer: Oak, and hiding the icon.
        fill(dc, &RECT { left: 1, top: y, right: w - 1, bottom: y + 1 }, c.line);
        let foot = RECT { left: 1, top: y + 1, right: w - 1, bottom: h - 1 };
        let button = |label: &str, at_right: bool, font: HFONT, dc: HDC| {
            let bw = text_width(dc, font, label) + p(20.0);
            let x = if at_right { right + p(6.0) - bw } else { left - p(6.0) };
            RECT { left: x, top: foot.top + p(6.0), right: x + bw, bottom: foot.bottom - p(6.0) }
        };
        let open = button("Open Oak", false, f.body, dc);
        let hide = button("Hide icon", true, f.small, dc);
        for (r, hit) in [(open, Hit::OpenOak), (hide, Hit::Hide)] {
            if self.hover == Some(hit) {
                fill(dc, &r, c.hover);
            }
            self.hits.push((r, hit));
        }
        let inset = |r: RECT| RECT { left: r.left + p(10.0), right: r.right - p(10.0), ..r };
        text(dc, f.body, c.accent, "Open Oak", inset(open), false);
        text(dc, f.small, c.ink3, "Hide icon", inset(hide), true);
        let hint = "Click a task for its output";
        let between = RECT { left: open.right + p(10.0), right: hide.left - p(10.0), ..foot };
        if text_width(dc, f.small, hint) <= between.right - between.left {
            text(dc, f.small, c.ink4, hint, between, false);
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom).map(|h| h.1)
    }

    /// A click on the list.
    fn click(&mut self, hit: Hit) -> Option<Action> {
        match hit {
            Hit::Task(i) => {
                let _ = open::that_detached(&self.tasks[i].k.log);
                Some(Action::HidePopup)
            }
            Hit::OpenOak => {
                self.open_oak();
                Some(Action::HidePopup)
            }
            Hit::Hide => Some(Action::Quit),
        }
    }
}

/// What a message leads to that sends messages of its own (done once the
/// widget is no longer borrowed).
enum Action {
    ShowPopup(RECT),
    HidePopup,
    Menu,
    Quit,
}

fn act(action: Option<Action>) {
    let Some(action) = action else { return };
    let Some((hwnd, popup)) = with(|w| (w.hwnd, w.popup)) else { return };
    unsafe {
        match action {
            Action::ShowPopup(r) => {
                SetWindowPos(popup, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_SHOWWINDOW);
                SetForegroundWindow(popup);
                InvalidateRect(popup, std::ptr::null(), 0);
            }
            Action::HidePopup => {
                ShowWindow(popup, SW_HIDE);
            }
            Action::Menu => {
                let menu = CreatePopupMenu();
                for (id, label) in [(1usize, "Show tasks"), (2, "Open Oak"), (0, ""), (3, "Hide icon (tasks keep running)")] {
                    if id == 0 {
                        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                    } else {
                        AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
                    }
                }
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                SetForegroundWindow(hwnd);
                let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, 0, hwnd, std::ptr::null());
                PostMessageW(hwnd, WM_NULL, 0, 0);
                DestroyMenu(menu);
                match chosen {
                    1 => act(with(|w| Action::ShowPopup(w.popup_rect()))),
                    2 => {
                        with(|w| w.open_oak());
                    }
                    3 => act(Some(Action::Quit)),
                    _ => {}
                }
            }
            Action::Quit => {
                DestroyWindow(hwnd);
            }
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp == TICK => {
            if with(|w| w.tick()) == Some(true) {
                act(Some(Action::Quit));
            }
            0
        }
        WM_TRAY => {
            match (lp & 0xffff) as u32 {
                NIN_SELECT | NIN_KEYSELECT => {
                    let action = with(|w| {
                        let visible = unsafe { IsWindowVisible(w.popup) } != 0;
                        let just_hid = w.hidden_at.is_some_and(|at| at.elapsed() < Duration::from_millis(300));
                        if visible || just_hid { Action::HidePopup } else { Action::ShowPopup(w.popup_rect()) }
                    });
                    act(action);
                }
                WM_CONTEXTMENU => act(Some(Action::Menu)),
                NIN_BALLOONUSERCLICK => {
                    with(|w| {
                        w.told = true;
                        w.open_oak();
                    });
                }
                NIN_BALLOONTIMEOUT | NIN_BALLOONHIDE => {
                    with(|w| w.told = true);
                }
                _ => {}
            }
            0
        }
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
            with(|w| w.update_icon());
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            with(|w| {
                let d = w.data();
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &d);
                    DestroyWindow(w.popup);
                }
            });
            unsafe { PostQuitMessage(0) };
            0
        }
        m if m == taskbar_created() && m != 0 => {
            with(|w| w.add_icon());
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xffff) as i16 as i32, ((lp >> 16) & 0xffff) as i16 as i32);
    match msg {
        WM_PAINT => {
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                GetClientRect(hwnd, &mut r);
                let (w, h) = (r.right, r.bottom);
                let mem = CreateCompatibleDC(dc);
                let bmp = CreateCompatibleBitmap(dc, w, h);
                let old = SelectObject(mem, bmp);
                with(|wd| wd.paint(mem, w, h, scale(GetDpiForWindow(hwnd))));
                BitBlt(dc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old);
                DeleteObject(bmp);
                DeleteDC(mem);
                EndPaint(hwnd, &ps);
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let changed = with(|w| {
                let hit = w.hit(x, y);
                let changed = hit != w.hover;
                w.hover = hit;
                changed
            });
            let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            unsafe { TrackMouseEvent(&mut tme) };
            if changed == Some(true) {
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            0
        }
        WM_MOUSELEAVE => {
            if with(|w| w.hover.take().is_some()) == Some(true) {
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            0
        }
        WM_SETCURSOR => {
            let hand = with(|w| w.hover.is_some()).unwrap_or(false);
            unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), if hand { IDC_HAND } else { IDC_ARROW })) };
            1
        }
        WM_LBUTTONUP => {
            let action = with(|w| w.hit(x, y).and_then(|h| w.click(h))).flatten();
            act(action);
            0
        }
        WM_KEYDOWN if wp == VK_ESCAPE as usize => {
            act(Some(Action::HidePopup));
            0
        }
        WM_ACTIVATE if (wp & 0xffff) as u32 == WA_INACTIVE => {
            with(|w| w.hidden_at = Some(Instant::now()));
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            0
        }
        WM_SHOWWINDOW if wp == 0 => {
            with(|w| {
                w.hidden_at = Some(Instant::now());
                w.hover = None;
            });
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_folders_match_by_what_follows() {
        let exe = r"c:\users\a\appdata\roaming\oak\tray\oak-tasks.exe";
        assert!(same_exe(exe, exe));
        assert!(same_exe(r"{3eb685db-65f9-4cf6-a03a-e3ef65729f3d}\oak\tray\oak-tasks.exe", exe));
        assert!(!same_exe(r"{3eb685db-65f9-4cf6-a03a-e3ef65729f3d}\other\oak-tasks.exe", exe));
        assert!(!same_exe(r"{x}", exe));
    }

    #[test]
    fn font_families_are_read() {
        assert_eq!(family_name(crate::markdown::PLEX_REGULAR).as_deref(), Some("IBM Plex Sans"));
        assert_eq!(family_name(crate::markdown::PLEX_BOLD).as_deref(), Some("IBM Plex Sans"));
        assert_eq!(family_name(b"not a font"), None);
    }

    #[test]
    fn escapes_are_stripped() {
        assert_eq!(strip_escapes("\x1b[32mok\x1b[0m\tdone"), "ok done");
        assert_eq!(strip_escapes("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn the_last_line_is_what_a_carriage_return_left() {
        let dir = std::env::temp_dir().join(format!("oak-tray-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("t.log");
        std::fs::write(&log, "first\nCompiling 1/3\rCompiling 2/3\rCompiling 3/3\n\n  \n").unwrap();
        assert_eq!(last_line(&log), "Compiling 3/3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The digits fill the icon's height and stay inside it, one or two.
    #[test]
    fn numbers_fill_the_icon() {
        for size in [16u32, 20, 24, 32] {
            for n in [1, 7, 12, 88, 150] {
                let px = number_pixels(n, size, 0xffffff);
                let rows: Vec<bool> = (0..size).map(|y| (0..size).any(|x| px[((y * size + x) * 4 + 3) as usize] > 128)).collect();
                let top = rows.iter().position(|&r| r).unwrap();
                let bottom = rows.iter().rposition(|&r| r).unwrap();
                assert!(bottom - top + 2 >= size as usize - (size as usize / 16) - 1, "{n} at {size}: rows {top}..{bottom}");
            }
        }
    }

    /// Writes the icon and the list as PNGs (to the temp folder), to look at.
    #[test]
    #[ignore]
    fn snapshot() {
        let out = std::env::var("OAK_TRAY_SNAPSHOT").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
        for (n, size) in [(3, 16u32), (12, 16), (3, 24), (12, 24), (12, 32)] {
            let px = number_pixels(n, size, 0xffffff);
            let mut rgba = vec![];
            for p in px.chunks(4) {
                let a = p[3] as f32 / 255.0;
                let bg = 0x20 as f32;
                let mix = |c: u8| (bg + (c as f32 - bg) * a) as u8;
                rgba.extend([mix(p[2]), mix(p[1]), mix(p[0]), 255]);
            }
            let img = image::RgbaImage::from_raw(size, size, rgba).unwrap();
            let big = image::imageops::resize(&img, size * 8, size * 8, image::imageops::FilterType::Nearest);
            big.save(out.join(format!("tray-icon-{n}-{size}.png"))).unwrap();
        }
        let now = crate::api::now_ms();
        let kept = |id, summary: &str, command: &str, conv: &str, ago: i64| Kept {
            conv: conv.into(),
            id,
            command: command.into(),
            summary: summary.into(),
            started_ms: now - ago,
            ..Default::default()
        };
        let mut w = Widget {
            m: Manifest {
                titles: [("a".to_string(), "Tray widget for background tasks".to_string()), ("b".to_string(), "Fix the flaky upload test".to_string())].into(),
                ..Default::default()
            },
            tasks: vec![
                Task { k: kept(1, "Run the dev server", "npm run dev -- --port 5173", "a", 3_725_000), handle: 1, end: None, last: "  VITE v5.4.2  ready in 412 ms".into() },
                Task { k: kept(2, "", "cargo build --release 2>&1 | tail -20", "a", 95_000), handle: 1, end: None, last: "   Compiling oak v0.2.0 (C:\\Users\\sussy\\Documents\\oak_public-tray)".into() },
                Task { k: kept(3, "Run the upload tests in a loop", "for i in $(seq 100); do pytest tests/test_upload.py -x || break; done", "b", 600_000), handle: 0, end: Some((End::Code(1), now - 30_000)), last: "FAILED tests/test_upload.py::test_resume - TimeoutError".into() },
                Task { k: kept(4, "Download the dataset", "curl -LO https://example.com/data.tar.gz", "b", 900_000), handle: 0, end: Some((End::Code(0), now - 90_000)), last: "100 1532M  100 1532M    0     0  48.1M      0  0:00:31  0:00:31 --:--:-- 51.2M".into() },
            ],
            hwnd: std::ptr::null_mut(),
            popup: std::ptr::null_mut(),
            icon: std::ptr::null_mut(),
            shown: None,
            hidden_at: None,
            hover: Some(Hit::Task(0)),
            hits: vec![],
            finished: None,
            told: false,
            added: Instant::now(),
            promoted: true,
        };
        load_fonts(&[]);
        for scale_pct in [100u32, 150] {
            let s = scale_pct as f32 / 100.0;
            let (wpx, hpx) = (px(WIDTH, s), px(list_height(w.tasks.len()), s));
            unsafe {
                let screen = GetDC(std::ptr::null_mut());
                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: wpx, biHeight: -hpx, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
                let mut bits = std::ptr::null_mut();
                let bmp = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
                let dc = CreateCompatibleDC(screen);
                SelectObject(dc, bmp);
                w.paint(dc, wpx, hpx, s);
                GdiFlush();
                let raw = std::slice::from_raw_parts(bits as *const u8, (wpx * hpx * 4) as usize);
                let rgba: Vec<u8> = raw.chunks(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
                image::RgbaImage::from_raw(wpx as u32, hpx as u32, rgba).unwrap().save(out.join(format!("tray-list-{scale_pct}.png"))).unwrap();
                DeleteDC(dc);
                DeleteObject(bmp);
                ReleaseDC(std::ptr::null_mut(), screen);
            }
        }
        // (The handles above are made up: nothing to close.)
        for t in &mut w.tasks {
            t.handle = 0;
        }
    }
}
