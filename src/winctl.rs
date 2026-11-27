// `oak --window ...`: run a GUI app on a hidden desktop and drive it from
// the command line, so an agent can launch, click, type into and screenshot
// a window without touching the user's screen, mouse, keyboard or focus.
//
// The app is started on a separate Win32 desktop ("oak-apps" by default) in
// the user's window station. Nothing on it is ever shown; its windows still
// paint (GPU apps included), and PrintWindow copies them out. Input is posted
// straight to the window as mouse and keyboard messages, so the real cursor
// and the foreground window never change. CREATE_NO_WINDOW suppresses
// consoles: Windows Terminal can otherwise open those on the real desktop.
//
// `launch` keeps running while the app does (start it with TASK_NEW): the
// app's output comes through it, and stopping it stops the app. Ordinary
// args use C-runtime quoting; cmd.exe /c and /k get their command text
// verbatim, enclosed once with /s so cmd strips only that outer pair.
//
// Inside Oak the same is done in-process (`on_desktop`): the WINDOW tool
// (`window_tool.rs`) and the live view's input (`window_view.rs`) run their
// steps on one thread attached to the hidden desktop.

use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, CREATE_NO_WINDOW, CreateProcessW, GetCurrentThreadId, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW, WaitForSingleObject,
};

type Hwnd = *mut c_void;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RectI {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateDesktopW(name: *const u16, device: *const u16, devmode: *const c_void, flags: u32, access: u32, sa: *const c_void) -> *mut c_void;
    fn OpenDesktopW(name: *const u16, flags: u32, inherit: i32, access: u32) -> *mut c_void;
    fn CloseDesktop(desk: *mut c_void) -> i32;
    fn SetThreadDesktop(desk: *mut c_void) -> i32;
    fn EnumDesktopWindows(desk: *mut c_void, f: unsafe extern "system" fn(Hwnd, isize) -> i32, l: isize) -> i32;
    fn GetWindowThreadProcessId(h: Hwnd, pid: *mut u32) -> u32;
    fn IsWindowVisible(h: Hwnd) -> i32;
    fn IsWindow(h: Hwnd) -> i32;
    fn GetWindowTextW(h: Hwnd, buf: *mut u16, n: i32) -> i32;
    fn GetWindowTextLengthW(h: Hwnd) -> i32;
    fn GetClientRect(h: Hwnd, r: *mut RectI) -> i32;
    fn GetWindowRect(h: Hwnd, r: *mut RectI) -> i32;
    fn PostMessageW(h: Hwnd, msg: u32, w: usize, l: isize) -> i32;
    fn SetWindowPos(h: Hwnd, after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn PrintWindow(h: Hwnd, dc: *mut c_void, flags: u32) -> i32;
    fn GetDC(h: Hwnd) -> *mut c_void;
    fn ReleaseDC(h: Hwnd, dc: *mut c_void) -> i32;
    fn VkKeyScanW(ch: u16) -> i16;
    fn MapVirtualKeyW(code: u32, map: u32) -> u32;
    fn GetKeyboardState(state: *mut u8) -> i32;
    fn SetKeyboardState(state: *const u8) -> i32;
    fn SetProcessDpiAwarenessContext(ctx: isize) -> i32;
    fn GetCursorPos(p: *mut [i32; 2]) -> i32;
    fn ClientToScreen(h: Hwnd, p: *mut [i32; 2]) -> i32;
    fn ChildWindowFromPointEx(h: Hwnd, p: Pt, flags: u32) -> Hwnd;
    fn MapWindowPoints(from: Hwnd, to: Hwnd, p: *mut [i32; 2], n: u32) -> i32;
    fn GetGUIThreadInfo(thread: u32, info: *mut GuiThreadInfo) -> i32;
    fn GetAncestor(h: Hwnd, flags: u32) -> Hwnd;
    fn EnumWindows(f: unsafe extern "system" fn(Hwnd, isize) -> i32, l: isize) -> i32;
    fn GetWindow(h: Hwnd, cmd: u32) -> Hwnd;
    fn GetWindowLongPtrW(h: Hwnd, index: i32) -> isize;
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmGetWindowAttribute(h: Hwnd, attr: u32, value: *mut c_void, size: u32) -> i32;
}

thread_local! {
    /// Input to a window of the user's own desktop: nothing may move it
    /// (see `under_cursor`).
    static GENTLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Pt {
    x: i32,
    y: i32,
}

#[repr(C)]
struct GuiThreadInfo {
    size: u32,
    flags: u32,
    active: Hwnd,
    focus: Hwnd,
    capture: Hwnd,
    menu_owner: Hwnd,
    move_size: Hwnd,
    caret: Hwnd,
    caret_rect: RectI,
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
    fn CreateCompatibleBitmap(dc: *mut c_void, w: i32, h: i32) -> *mut c_void;
    fn SelectObject(dc: *mut c_void, obj: *mut c_void) -> *mut c_void;
    fn DeleteObject(obj: *mut c_void) -> i32;
    fn DeleteDC(dc: *mut c_void) -> i32;
    fn GetDIBits(dc: *mut c_void, bmp: *mut c_void, start: u32, lines: u32, bits: *mut c_void, info: *mut BitmapInfoHeader, usage: u32) -> i32;
}

const GENERIC_ALL: u32 = 0x1000_0000;
/// SetWindowPos: no move, no z-order change, no activation.
const SWP_MOVE_ONLY: u32 = 0x0002 | 0x0004 | 0x0010;
/// SetWindowPos: no size, no z-order change, no activation.
const SWP_SIZE_ONLY: u32 = 0x0001 | 0x0004 | 0x0010;
const WM_CLOSE: u32 = 0x0010;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MOUSEWHEEL: u32 = 0x020A;
const MK_LBUTTON: usize = 1;
const MK_RBUTTON: usize = 2;
const VK_SHIFT: u8 = 0x10;
const VK_CONTROL: u8 = 0x11;
const VK_MENU: u8 = 0x12;

pub const HELP: &str = "\
oak --window: run a GUI app on a hidden desktop and drive it, without touching
the user's screen, mouse, keyboard or focus.

  launch [--desktop D] [--size WxH] -- PROGRAM [ARGS...]
      Starts PROGRAM on the hidden desktop and prints its window once it
      appears. Keeps running while the app does, passing on its output:
      start it with TASK action=new, and TASK action=delete closes the app.
      Arguments are literal, except cmd.exe /c or /k: pass the whole command
      as one argument, quoted for cmd (not C backslash-escaped quotes).
      Oak adds /s and an outer quote pair for cmd to strip.
  list                      Windows on the desktop: id, process, size, title.
  shot FILE.png             Screenshot of the window's client area.
  record DIR SECONDS [FPS]  Screenshots in a row (default 30 a second) into
                            DIR as 00000.bmp, 00001.bmp...: a video's frames
                            (ffmpeg -framerate FPS -i DIR/%05d.bmp out.mp4).
                            Stops early when DIR/stop exists.
  click X Y [--right] [--double]
  move X Y                  Move the pointer (hover).
  drag X1 Y1 X2 Y2 [--right] [--throw]
                            --throw: quick, let go while still moving.
  scroll X Y LINES          Wheel at X,Y; positive LINES scroll up.
  type TEXT                 Types the text.
  key KEY                   e.g. enter, tab, escape, backspace, delete, up,
                            down, left, right, home, end, pageup, pagedown,
                            f1-f12, space, a, 7, ctrl+a, shift+tab, alt+f4.
  resize W H                Sets the window's outer size.
  close                     Asks every window on the desktop to close.

Coordinates are pixels in the window's client area, the same as in the
screenshot. Commands act on the largest window on the desktop; pass
--window ID (from list) for another one, e.g. a dialog. Input commands
wait --wait MS (default 300) afterwards so the app can repaint before a shot.

A window on a hidden desktop is never under the real cursor, so Windows tells
the app the pointer left right after each move; an app that drops the
pointer then shows no hover and can lose a drag. Launched apps get
OAK_HIDDEN_DESKTOP set (to the desktop's name) and can ignore it: Oak does.
";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Opts {
    desktop: String,
    window: Option<usize>,
    wait: u64,
    rest: Vec<String>,
    flags: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { desktop: "oak-apps".into(), window: None, wait: 300, rest: vec![], flags: vec![] };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--" => {
                o.rest.push("--".into());
                o.rest.extend(it.by_ref().cloned());
            }
            "--desktop" => o.desktop = value("--desktop")?,
            "--window" => o.window = Some(parse_id(&value("--window")?)?),
            "--wait" => o.wait = value("--wait")?.parse().map_err(|_| "--wait takes milliseconds")?,
            "--size" => {
                o.flags.push("--size".into());
                o.flags.push(value("--size")?);
            }
            f if f.starts_with("--") => o.flags.push(f.to_string()),
            _ => o.rest.push(a.clone()),
        }
    }
    Ok(o)
}

fn parse_id(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let r = match s.strip_prefix("0x") {
        Some(h) => usize::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| format!("Not a window id: {s}"))
}

/// Entry point for `oak --window ARGS`. Returns the exit code.
pub fn main(args: &[String]) -> i32 {
    unsafe { SetProcessDpiAwarenessContext(-4) }; // per-monitor v2: real pixels
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("oak --window: {e}");
            1
        }
    }
}

fn run(args: &[String]) -> Result<i32, String> {
    let Some(cmd) = args.first() else {
        print!("{HELP}");
        return Ok(0);
    };
    let o = parse(&args[1..])?;
    let n = |i: usize| -> Result<i32, String> {
        o.rest.get(i).ok_or(format!("{cmd}: missing argument {}", i + 1))?.parse::<i32>().map_err(|_| format!("{cmd}: argument {} must be a number", i + 1))
    };
    let flag = |f: &str| o.flags.iter().any(|x| x == f);
    if cmd == "launch" {
        return launch(&o);
    }
    if matches!(cmd.as_str(), "help" | "--help" | "-h") {
        print!("{HELP}");
        return Ok(0);
    }
    // Read before switching desktops: the cursor can't be read from a
    // thread on a desktop that isn't the one receiving input.
    read_cursor();
    let desk = Desktop::open(&o.desktop)?;
    match cmd.as_str() {
        "list" => {
            let ws = desk.windows();
            if ws.is_empty() {
                println!("No windows on desktop {}.", o.desktop);
            }
            for w in ws {
                println!("{}", describe(w));
            }
            return Ok(0);
        }
        "close" => {
            let ws = desk.windows();
            for &w in &ws {
                unsafe { PostMessageW(w as Hwnd, WM_CLOSE, 0, 0) };
            }
            println!("Asked {} window(s) to close.", ws.len());
            return Ok(0);
        }
        _ => {}
    }
    let w = desk.target(o.window)? as Hwnd;
    match cmd.as_str() {
        "shot" => {
            let path = o.rest.first().ok_or("shot: give a file name, e.g. shot out.png")?;
            let (width, height) = shot(w, std::path::Path::new(path))?;
            println!("Saved {width}x{height} screenshot of {} to {path}", title(w));
            return Ok(0);
        }
        "record" => {
            let dir = std::path::Path::new(o.rest.first().ok_or("record: give a folder for the frames")?);
            let secs = n(1)?.max(1) as f64;
            let fps = o.rest.get(2).map_or(Ok(30), |_| n(2))?.clamp(1, 120) as f64;
            std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
            let (start, every) = (Instant::now(), Duration::from_secs_f64(1.0 / fps));
            let total = (secs * fps).round() as u32;
            // Files are written on another thread, so a slow disk doesn't
            // slow the captures.
            let (tx, rx) = std::sync::mpsc::channel::<(u32, std::sync::Arc<(u32, u32, Vec<u8>)>)>();
            let out = dir.to_path_buf();
            let writer = std::thread::spawn(move || -> Result<(), String> {
                for (i, f) in rx {
                    write_bmp(&out.join(format!("{i:05}.bmp")), f.0, f.1, &f.2)?;
                }
                Ok(())
            });
            let mut count = 0u32;
            while count < total && !dir.join("stop").exists() {
                if let Some(wait) = (start + every * count).checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                // Each frame of the video is the first capture at or after
                // its time: one that came late fills the frames it missed.
                let slot = ((start.elapsed().as_secs_f64() * fps).round() as u32).min(total - 1);
                let frame = std::sync::Arc::new(capture(w)?);
                while count <= slot {
                    tx.send((count, frame.clone())).map_err(|_| "The writer stopped.")?;
                    count += 1;
                }
            }
            drop(tx);
            writer.join().map_err(|_| "The writer panicked.")??;
            println!("Recorded {count} frames ({fps} a second) of {} to {}", title(w), dir.display());
            return Ok(0);
        }
        "click" => {
            let (x, y) = (n(0)?, n(1)?);
            let (right, double) = (flag("--right"), flag("--double"));
            click(w, x, y, right, double);
        }
        "move" => {
            let (x, y) = (n(0)?, n(1)?);
            point_at(w, x, y, 0);
        }
        "drag" => drag(w, (n(0)?, n(1)?), (n(2)?, n(3)?), flag("--right"), flag("--throw")),
        "scroll" => scroll(w, n(0)?, n(1)?, n(2)? * 120),
        "type" => type_text(w, &o.rest.iter().map(|a| unmangle(a)).collect::<Vec<_>>().join(" ")),
        "key" => key(w, o.rest.first().ok_or("key: which key?")?)?,
        "resize" => resize(w, n(0)?, n(1)?),
        other => return Err(format!("Unknown command {other}. Run `oak --window help`.")),
    }
    std::thread::sleep(Duration::from_millis(o.wait));
    Ok(0)
}

/// The hidden desktop, attached to this thread so its windows can be
/// enumerated, captured and sent input.
pub(crate) struct Desktop(*mut c_void);

impl Desktop {
    fn open(name: &str) -> Result<Desktop, String> {
        let h = unsafe { OpenDesktopW(wide(name).as_ptr(), 0, 0, GENERIC_ALL) };
        if h.is_null() {
            return Err(format!("No desktop {name}: nothing has been launched on it (or it has all closed)."));
        }
        if unsafe { SetThreadDesktop(h) } == 0 {
            return Err(format!("Couldn't switch to desktop {name}: {}", std::io::Error::last_os_error()));
        }
        Ok(Desktop(h))
    }

    fn create(name: &str) -> Result<Desktop, String> {
        let w = wide(name);
        let h = unsafe { CreateDesktopW(w.as_ptr(), std::ptr::null(), std::ptr::null(), 0, GENERIC_ALL, std::ptr::null()) };
        if h.is_null() {
            return Err(format!("Couldn't create desktop {name}: {}", std::io::Error::last_os_error()));
        }
        Ok(Desktop(h))
    }

    /// Visible, titled top-level windows, largest first.
    fn windows(&self) -> Vec<usize> {
        unsafe extern "system" fn each(h: Hwnd, l: isize) -> i32 {
            let out = unsafe { &mut *(l as *mut Vec<usize>) };
            if unsafe { IsWindowVisible(h) != 0 && GetWindowTextLengthW(h) > 0 } {
                out.push(h as usize);
            }
            1
        }
        let mut out: Vec<usize> = vec![];
        unsafe { EnumDesktopWindows(self.0, each, &mut out as *mut _ as isize) };
        out.sort_by_key(|&w| std::cmp::Reverse(area(w as Hwnd)));
        out
    }

    fn target(&self, id: Option<usize>) -> Result<usize, String> {
        match id {
            Some(w) if unsafe { IsWindow(w as Hwnd) } != 0 => Ok(w),
            Some(w) => Err(format!("No window {w:#x}.")),
            None => self.windows().first().copied().ok_or("No windows on the desktop. Launch an app first.".into()),
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe { CloseDesktop(self.0) };
    }
}

fn area(w: Hwnd) -> i64 {
    let mut r = RectI::default();
    unsafe { GetWindowRect(w, &mut r) };
    (r.right - r.left) as i64 * (r.bottom - r.top) as i64
}

fn title(w: Hwnd) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(w, buf.as_mut_ptr(), 512) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn client_size(w: Hwnd) -> (i32, i32) {
    let mut r = RectI::default();
    unsafe { GetClientRect(w, &mut r) };
    (r.right, r.bottom)
}

fn pid_of(w: usize) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(w as Hwnd, &mut pid) };
    pid
}

/// Process `root` and every process it started, and they started, now.
fn descendants(root: u32) -> Vec<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    let mut pairs = vec![];
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            return vec![root];
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                pairs.push((e.th32ProcessID, e.th32ParentProcessID));
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let p = out[i];
        out.extend(pairs.iter().filter(|(c, parent)| *parent == p && *c != p && !out.contains(c)).map(|(c, _)| *c).collect::<Vec<_>>());
        i += 1;
    }
    out
}

fn describe(w: usize) -> String {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(w as Hwnd, &mut pid) };
    let (cw, ch) = client_size(w as Hwnd);
    format!("window {w:#x}  pid {pid}  {cw}x{ch}  \"{}\"", title(w as Hwnd))
}

fn xy(x: i32, y: i32) -> isize {
    ((y as u16 as u32) << 16 | x as u16 as u32) as isize
}

fn post(w: Hwnd, msg: u32, wp: usize, lp: isize) {
    unsafe { PostMessageW(w, msg, wp, lp) };
}

/// The window a press went to while its button is held: the moves and the
/// release go to it, as with real input (the capture).
static PRESSED: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

/// The window that gets the mouse at client point x,y of top-level `w`:
/// its deepest visible, enabled child there (classic apps are made of
/// child controls, and posted input isn't routed to them as real input
/// is), or the one pressed while a button is held; and the point in that
/// window's client coordinates.
fn mouse_target(w: Hwnd, x: i32, y: i32) -> (Hwnd, i32, i32) {
    let mut pt = [x, y];
    if let Some(p) = *PRESSED.lock().unwrap()
        && unsafe { IsWindow(p as Hwnd) } != 0
    {
        unsafe { MapWindowPoints(w, p as Hwnd, &mut pt, 1) };
        return (p as Hwnd, pt[0], pt[1]);
    }
    let mut cur = w;
    for _ in 0..32 {
        // CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT
        let child = unsafe { ChildWindowFromPointEx(cur, Pt { x: pt[0], y: pt[1] }, 1 | 2 | 4) };
        if child.is_null() || child == cur {
            break;
        }
        unsafe { MapWindowPoints(cur, child, &mut pt, 1) };
        cur = child;
    }
    (cur, pt[0], pt[1])
}

/// A mouse message at client point x,y of `w`, to the window there.
fn mouse(w: Hwnd, msg: u32, wp: usize, x: i32, y: i32) {
    let (t, cx, cy) = mouse_target(w, x, y);
    let down = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | 0x0207 | WM_LBUTTONDBLCLK);
    let up = matches!(msg, WM_LBUTTONUP | WM_RBUTTONUP | 0x0208);
    post(t, msg, wp, xy(cx, cy));
    if down {
        *PRESSED.lock().unwrap() = Some(t as usize);
    } else if up && wp & (MK_LBUTTON | MK_RBUTTON | 0x10) == 0 {
        *PRESSED.lock().unwrap() = None;
    }
}

/// The window keys go to in top-level `w`: its thread's focused one when
/// that is within `w`, else `w`.
fn key_target(w: Hwnd) -> Hwnd {
    let thread = unsafe { GetWindowThreadProcessId(w, std::ptr::null_mut()) };
    let mut g: GuiThreadInfo = unsafe { std::mem::zeroed() };
    g.size = std::mem::size_of::<GuiThreadInfo>() as u32;
    if unsafe { GetGUIThreadInfo(thread, &mut g) } != 0 && !g.focus.is_null() && unsafe { GetAncestor(g.focus, 2) } == w {
        return g.focus; // GA_ROOT
    }
    w
}

/// Where the real mouse cursor was when the command (or `on_desktop` job)
/// started.
static CURSOR: std::sync::Mutex<Option<[i32; 2]>> = std::sync::Mutex::new(None);

/// Reads the real cursor; only from a thread on the desktop receiving input.
fn read_cursor() {
    let mut cursor = [0i32; 2];
    if unsafe { GetCursorPos(&mut cursor) } != 0 {
        *CURSOR.lock().unwrap() = Some(cursor);
    }
}

/// Moves the hidden window (never the cursor) so that its client point
/// x,y lies under the real mouse cursor. Windows decides whether the
/// pointer has left a window by where the real cursor is; if it isn't over
/// the window, the app is told at once that the pointer left, and a click
/// or hover posted to it can be dropped.
fn under_cursor(w: Hwnd, x: i32, y: i32) {
    if GENTLE.with(|g| g.get()) {
        return; // the user's window stays where they put it
    }
    let Some(cursor) = *CURSOR.lock().unwrap() else { return };
    let mut origin = [0i32; 2];
    let mut r = RectI::default();
    unsafe {
        ClientToScreen(w, &mut origin);
        GetWindowRect(w, &mut r);
    }
    let (dx, dy) = (cursor[0] - (origin[0] + x), cursor[1] - (origin[1] + y));
    if (dx, dy) != (0, 0) {
        unsafe { SetWindowPos(w, std::ptr::null_mut(), r.left + dx, r.top + dy, 0, 0, SWP_SIZE_ONLY) };
    }
}

/// Brings the pointer to client point x,y: the window under the real
/// cursor, then a move there, from a pixel off. After the hidden desktop's
/// "pointer left" the app knows no position, and a move to where it was
/// before is dropped as no move (winit), so a press or release would come
/// with no position and be ignored; every button or wheel message follows
/// this.
fn point_at(w: Hwnd, x: i32, y: i32, buttons: usize) {
    under_cursor(w, x, y);
    mouse(w, WM_MOUSEMOVE, buttons, x + 1, y);
    mouse(w, WM_MOUSEMOVE, buttons, x, y);
}

fn drag(w: Hwnd, (x1, y1): (i32, i32), (x2, y2): (i32, i32), right: bool, throw: bool) {
    let (down, up, held) = if right { (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON) } else { (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON) };
    point_at(w, x1, y1, 0);
    mouse(w, down, held, x1, y1);
    // Slow enough that even a debug build sees the moves over several
    // frames (all in one frame, a drag reads as a click). A throw: a few
    // frames' worth, let go with no pause at the end.
    let (steps, ms) = if throw { (8, 20) } else { (20, 40) };
    for i in 1..=steps {
        let (x, y) = (x1 + (x2 - x1) * i / steps, y1 + (y2 - y1) * i / steps);
        std::thread::sleep(Duration::from_millis(ms));
        under_cursor(w, x, y);
        mouse(w, WM_MOUSEMOVE, held, x, y);
    }
    if !throw {
        std::thread::sleep(Duration::from_millis(40));
        point_at(w, x2, y2, held);
    }
    mouse(w, up, 0, x2, y2);
}

/// The wheel at client point x,y: `delta` in wheel units (120 a line,
/// positive up).
fn scroll(w: Hwnd, x: i32, y: i32, delta: i32) {
    point_at(w, x, y, 0);
    // The wheel message carries screen coordinates.
    let mut at = [x, y];
    unsafe { ClientToScreen(w, &mut at) };
    let (t, _, _) = mouse_target(w, x, y);
    post(t, WM_MOUSEWHEEL, (delta as i16 as u16 as usize) << 16, xy(at[0], at[1]));
}

fn type_text(w: Hwnd, text: &str) {
    for unit in text.encode_utf16() {
        type_unit(w, unit);
    }
}

/// Sets the window's outer size.
fn resize(w: Hwnd, width: i32, height: i32) {
    unsafe { SetWindowPos(w, std::ptr::null_mut(), 0, 0, width, height, SWP_MOVE_ONLY) };
}

fn click(w: Hwnd, x: i32, y: i32, right: bool, double: bool) {
    let (down, up, mk, dbl) =
        if right { (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON, WM_RBUTTONDOWN) } else { (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON, WM_LBUTTONDBLCLK) };
    point_at(w, x, y, 0);
    mouse(w, down, mk, x, y);
    mouse(w, up, 0, x, y);
    if double {
        mouse(w, WM_MOUSEMOVE, 0, x, y);
        mouse(w, dbl, mk, x, y);
        mouse(w, up, 0, x, y);
    }
}

/// The lParam of a key message for virtual key `vk`.
fn key_lparam(vk: u8, up: bool) -> isize {
    let scan = unsafe { MapVirtualKeyW(vk as u32, 0) }; // MAPVK_VK_TO_VSC
    let extended = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C) as u32; // page/end/home/arrows/ins/del/win
    let mut l = 1 | (scan & 0xFF) << 16 | extended << 24;
    if up {
        l |= 0xC000_0000;
    }
    l as isize
}

/// The virtual key of the left-hand copy of modifier `m`.
fn side_key(m: u8) -> u8 {
    match m {
        VK_SHIFT => 0xA0,
        VK_CONTROL => 0xA2,
        _ => 0xA4,
    }
}

/// Holds modifier keys down in the app's keyboard state for one key press.
/// Posted messages don't change it, and apps ask GetKeyState whether Ctrl
/// is down, so share the app thread's input state and set it there.
struct Held {
    thread: u32,
    before: [u8; 256],
}

impl Held {
    fn press(w: Hwnd, mods: &[u8]) -> Option<Held> {
        if mods.is_empty() {
            return None;
        }
        let thread = unsafe { GetWindowThreadProcessId(w, std::ptr::null_mut()) };
        unsafe { AttachThreadInput(GetCurrentThreadId(), thread, 1) };
        let mut before = [0u8; 256];
        unsafe { GetKeyboardState(before.as_mut_ptr()) };
        let mut state = before;
        for &m in mods {
            state[m as usize] |= 0x80;
            // (The left key too: apps ask the side-specific one.)
            state[side_key(m) as usize] |= 0x80;
            post(w, if m == VK_MENU { WM_SYSKEYDOWN } else { WM_KEYDOWN }, m as usize, key_lparam(m, false));
        }
        unsafe { SetKeyboardState(state.as_ptr()) };
        Some(Held { thread, before })
    }

    fn release(self, w: Hwnd, mods: &[u8]) {
        // Give the app time to read the state before putting it back.
        std::thread::sleep(Duration::from_millis(150));
        for &m in mods.iter().rev() {
            post(w, if m == VK_MENU { WM_SYSKEYUP } else { WM_KEYUP }, m as usize, key_lparam(m, true));
        }
        unsafe {
            SetKeyboardState(self.before.as_ptr());
            AttachThreadInput(GetCurrentThreadId(), self.thread, 0);
        }
    }
}

/// Whether `w` is a child control (a classic app's: its message loop
/// translates key-downs into characters itself, so a character posted too
/// would come twice).
fn is_child(w: Hwnd) -> bool {
    unsafe { GetAncestor(w, 2) != w } // GA_ROOT
}

fn press(w: Hwnd, vk: u8, ch: Option<u16>, alt: bool) {
    let (down, up) = if alt { (WM_SYSKEYDOWN, WM_SYSKEYUP) } else { (WM_KEYDOWN, WM_KEYUP) };
    post(w, down, vk as usize, key_lparam(vk, false));
    if let Some(c) = ch.filter(|_| !is_child(w)) {
        post(w, WM_CHAR, c as usize, key_lparam(vk, false));
    }
    post(w, up, vk as usize, key_lparam(vk, true));
}

/// Types one UTF-16 unit: as its key when the keyboard layout has one,
/// else as a bare character.
fn type_unit(w: Hwnd, unit: u16) {
    let w = key_target(w);
    let scan = if unit == '\n' as u16 { 0x0D } else { unsafe { VkKeyScanW(unit) } };
    let unit = if unit == '\n' as u16 { '\r' as u16 } else { unit };
    // A control gets the character alone: a key-down would be translated
    // into it again (and with the keyboard state of the app's thread).
    if scan == -1 || is_child(w) {
        post(w, WM_CHAR, unit as usize, 1);
    } else {
        let vk = (scan & 0xFF) as u8;
        let shift = scan & 0x100 != 0;
        let held = Held::press(w, if shift { &[VK_SHIFT] } else { &[] });
        press(w, vk, Some(unit), false);
        if let Some(h) = held {
            h.release(w, &[VK_SHIFT]);
        }
    }
    std::thread::sleep(Duration::from_millis(8));
}

fn key(w: Hwnd, spec: &str) -> Result<(), String> {
    let w = key_target(w);
    let lower = spec.to_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    let name = parts.pop().unwrap_or("");
    let mut mods = vec![];
    for m in parts {
        mods.push(match m {
            "ctrl" | "control" => VK_CONTROL,
            "shift" => VK_SHIFT,
            "alt" => VK_MENU,
            _ => return Err(format!("Unknown modifier {m}")),
        });
    }
    let (vk, ch) = named_key(name)?;
    // With Ctrl or Alt held, a key types nothing; Ctrl+letter's control
    // character is what TranslateMessage would have produced.
    let ch = if mods.contains(&VK_MENU) {
        None
    } else if mods.contains(&VK_CONTROL) {
        ch.filter(|c| (b'a' as u16..=b'z' as u16).contains(c)).map(|c| c - b'a' as u16 + 1)
    } else if mods.contains(&VK_SHIFT) {
        ch.map(|c| if (b'a' as u16..=b'z' as u16).contains(&c) { c - 32 } else { c })
    } else {
        ch
    };
    let held = Held::press(w, &mods);
    press(w, vk, ch, mods.contains(&VK_MENU));
    if let Some(h) = held {
        h.release(w, &mods);
    }
    Ok(())
}

/// A key's virtual key and the character it types (None when it types
/// none), from its name: one of the named keys, or a single character.
fn named_key(name: &str) -> Result<(u8, Option<u16>), String> {
    let named = match name {
        "enter" | "return" => Some((0x0D, Some(13))),
        "tab" => Some((0x09, Some(9))),
        "escape" | "esc" => Some((0x1B, Some(27))),
        "backspace" => Some((0x08, Some(8))),
        "space" => Some((0x20, Some(32))),
        "delete" | "del" => Some((0x2E, None)),
        "insert" => Some((0x2D, None)),
        "home" => Some((0x24, None)),
        "end" => Some((0x23, None)),
        "pageup" => Some((0x21, None)),
        "pagedown" => Some((0x22, None)),
        "left" => Some((0x25, None)),
        "up" => Some((0x26, None)),
        "right" => Some((0x27, None)),
        "down" => Some((0x28, None)),
        _ => None,
    };
    if let Some(k) = named {
        return Ok(k);
    }
    if let Some(n @ 1..=12) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return Ok((0x6F + n, None));
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else { return Err(format!("Unknown key {name}")) };
    let scan = unsafe { VkKeyScanW(c as u16) };
    if scan == -1 {
        return Err(format!("No key for {c} on this keyboard layout; use `type`."));
    }
    Ok((scan as u8, Some(c as u16)))
}

/// Captures the window's client area (as composed, so GPU-drawn content is
/// included) to a PNG. Returns its size.
fn shot(w: Hwnd, path: &std::path::Path) -> Result<(u32, u32), String> {
    let (cw, ch, mut px) = capture(w)?;
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2); // BGRA -> RGBA
        p[3] = 255;
    }
    let img = image::RgbaImage::from_raw(cw, ch, px).ok_or("bad image size")?;
    img.save_with_format(path, image::ImageFormat::Png).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
    Ok((cw, ch))
}

/// A 32-bit top-down BMP of `px` (BGRA rows): quick to write, for `record`.
fn write_bmp(path: &std::path::Path, w: u32, h: u32, px: &[u8]) -> Result<(), String> {
    let mut out = Vec::with_capacity(54 + px.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + px.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    out.extend_from_slice(px);
    std::fs::write(path, out).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
}

/// The window's client area: its size and its pixels (BGRA rows, top down).
fn capture(w: Hwnd) -> Result<(u32, u32, Vec<u8>), String> {
    let (cw, ch) = client_size(w);
    if cw <= 0 || ch <= 0 {
        return Err("The window has no client area (minimized?).".into());
    }
    let mut px = vec![0u8; cw as usize * ch as usize * 4];
    unsafe {
        let dc = GetDC(w);
        let mem = CreateCompatibleDC(dc);
        let bmp = CreateCompatibleBitmap(dc, cw, ch);
        let old = SelectObject(mem, bmp);
        let ok = PrintWindow(w, mem, 1 | 2); // PW_CLIENTONLY | PW_RENDERFULLCONTENT
        let info = BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: cw,
            height: -ch, // top-down
            planes: 1,
            bit_count: 32,
            compression: 0,
            size_image: 0,
            x_ppm: 0,
            y_ppm: 0,
            clr_used: 0,
            clr_important: 0,
        };
        // BITMAPINFO is the header followed by an (unused) color table.
        let mut buf = [0u8; 64];
        std::ptr::copy_nonoverlapping(&info as *const _ as *const u8, buf.as_mut_ptr(), std::mem::size_of::<BitmapInfoHeader>());
        let lines = GetDIBits(mem, bmp, 0, ch as u32, px.as_mut_ptr() as *mut c_void, buf.as_mut_ptr() as *mut BitmapInfoHeader, 0);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(w, dc);
        if ok == 0 || lines == 0 {
            return Err(format!("Couldn't capture the window: {}", std::io::Error::last_os_error()));
        }
    }
    Ok((cw as u32, ch as u32, px))
}

/// Git Bash turns arguments that look like Unix paths into Windows ones
/// ("/goal x" arrives as "C:/Program Files/Git/goal x"). Text to type is
/// meant literally, so undo that.
fn unmangle(arg: &str) -> String {
    static ROOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let looks_converted = arg.len() > 3 && arg.as_bytes()[1] == b':' && arg.as_bytes()[2] == b'/';
    if !looks_converted || std::env::var_os("MSYSTEM").is_none() {
        return arg.to_string();
    }
    let root = ROOT.get_or_init(|| {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("cygpath").args(["-m", "/"]).creation_flags(0x0800_0000).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().trim_end_matches('/').to_string()).filter(|r| !r.is_empty())
    });
    match root.as_deref().and_then(|r| arg.strip_prefix(r)) {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => arg.to_string(),
    }
}

/// Quotes one argument for a Windows command line.
fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
                continue;
            }
            _ => {}
        }
        if c != '\\' {
            out.push_str(&"\\".repeat(slashes));
            slashes = 0;
            out.push(c);
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// Encodes ordinary executables with C-runtime argument quoting. cmd.exe
/// instead parses everything after /c or /k as shell syntax: preserve that
/// text verbatim, with /s and one outer quote pair that cmd strips. In the
/// tool's args array, pass the whole shell command as one string.
fn command_line(program: &[&String]) -> String {
    let cmd = program.first().is_some_and(|p| {
        p.rsplit(['/', '\\']).next().is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe") || n.eq_ignore_ascii_case("cmd"))
    });
    if cmd
        && let Some(i) = program.iter().skip(1).position(|a| a.eq_ignore_ascii_case("/c") || a.eq_ignore_ascii_case("/k")).map(|i| i + 1)
    {
        let mut parts: Vec<String> = program[..i].iter().map(|a| quote(a)).collect();
        if !program[1..i].iter().any(|a| a.eq_ignore_ascii_case("/s")) {
            parts.push("/s".into());
        }
        parts.push(program[i].to_string());
        let command = program[i + 1..].iter().map(|a| a.as_str()).collect::<Vec<_>>().join(" ");
        parts.push(format!("\"{command}\""));
        return parts.join(" ");
    }
    program.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// `--size`'s "WxH", as a window's outer size.
fn size_of(s: &str) -> (i32, i32) {
    let (w, h) = s.split_once(['x', 'X']).unwrap_or((s, s));
    (w.parse().unwrap_or(1200), h.parse().unwrap_or(800))
}

fn launch(o: &Opts) -> Result<i32, String> {
    // Wait for job assignment before creating any descendant processes.
    if o.flags.iter().any(|f| f == "--job-wait") {
        std::io::Read::read_exact(&mut std::io::stdin(), &mut [0u8; 1]).map_err(|e| format!("App launcher lost its owning Oak: {e}"))?;
    }
    let program: Vec<&String> = match o.rest.iter().position(|a| a == "--") {
        Some(i) => o.rest[i + 1..].iter().collect(),
        None => o.rest.iter().collect(),
    };
    let Some(&exe) = program.first() else { return Err("launch: which program? e.g. launch -- target/debug/app.exe".into()) };
    let size = o.flags.iter().position(|f| f == "--size").and_then(|i| o.flags.get(i + 1)).map(|s| size_of(s));
    let desk = Desktop::create(&o.desktop)?;
    let before = desk.windows();
    let line = command_line(&program);
    let mut cmdline = wide(&line);
    let desk_name = wide(&o.desktop);
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desk_name.as_ptr() as *mut u16;
    // The app writes to our output, which is the task's.
    use std::os::windows::io::AsRawHandle;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = std::ptr::null_mut();
    si.hStdOutput = std::io::stdout().as_raw_handle() as HANDLE;
    si.hStdError = std::io::stderr().as_raw_handle() as HANDLE;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // The app inherits our environment: this tells it where it runs (see
    // HELP). SAFETY: no other thread of ours reads the environment.
    unsafe { std::env::set_var("OAK_HIDDEN_DESKTOP", &o.desktop) };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            // A console program (cmd, Python) otherwise allocates a new
            // console. Windows Terminal can host that on the user's real
            // desktop, ignoring lpDesktop. GUI windows still use lpDesktop;
            // stdout/stderr remain the inherited log pipes.
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(format!("Couldn't start {exe}: {}", std::io::Error::last_os_error()));
    }
    println!("Started {exe} (pid {}) on hidden desktop {}.", pi.dwProcessId, o.desktop);
    // Wait for its first window.
    let t = Instant::now();
    let mut shown = false;
    while t.elapsed() < Duration::from_secs(60) {
        if unsafe { WaitForSingleObject(pi.hProcess, 0) } == 0 {
            break;
        }
        // Only a window of the app or of a program it started: an app that
        // hands off to one already running (Windows 11's Notepad) would
        // otherwise pass off that program's window, the user's, as its own.
        let family = descendants(pi.dwProcessId);
        if let Some(&w) = desk.windows().iter().find(|w| !before.contains(w) && family.contains(&pid_of(**w))) {
            // Let it settle at its first size before resizing or reporting.
            std::thread::sleep(Duration::from_millis(300));
            if let Some((width, height)) = size {
                unsafe { SetWindowPos(w as Hwnd, std::ptr::null_mut(), 0, 0, width, height, SWP_MOVE_ONLY) };
                std::thread::sleep(Duration::from_millis(200));
            }
            println!("{}", describe(w));
            println!("Drive it with `oak --window shot|click|type|key|...` (see `oak --window help`).");
            shown = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !shown && desk.windows().iter().any(|w| !before.contains(w)) {
        println!("A window of another program appeared instead (the app handed off to a program already running?); it isn't the app's, so it is left alone.");
    }
    if !shown && unsafe { WaitForSingleObject(pi.hProcess, 0) } != 0 {
        println!("No window after 60s; still waiting for the app to exit.");
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut code = 0u32;
    unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    println!("The app exited with code {}.", code as i32);
    drop(desk);
    Ok(code as i32)
}

// ------------------------------------------------------------ in Oak

/// The hidden desktop Oak's apps run on.
pub(crate) const DESKTOP: &str = "oak-apps";

type Job = Box<dyn FnOnce(Option<&Desktop>) + Send>;

/// Hands `job` to Oak's thread attached to the hidden desktop (made once),
/// to run after the jobs before it. The real cursor is read first, on the
/// calling thread (see `under_cursor`).
fn submit(job: Job) -> Result<(), String> {
    use std::sync::mpsc::{Sender, channel};
    static JOBS: std::sync::Mutex<Option<Sender<Job>>> = std::sync::Mutex::new(None);
    read_cursor();
    let mut jobs = JOBS.lock().unwrap();
    let sender = jobs.get_or_insert_with(|| {
        unsafe { SetProcessDpiAwarenessContext(-4) };
        let (tx, rx) = channel::<Job>();
        let _ = std::thread::Builder::new().name("hidden-desktop".into()).spawn(move || {
            let mut desk: Option<Desktop> = None;
            for job in rx {
                if desk.is_none() {
                    desk = Desktop::open(DESKTOP).ok();
                }
                job(desk.as_ref());
            }
        });
        tx
    });
    sender.send(job).map_err(|_| "The hidden desktop's thread stopped.".to_string())
}

/// Runs `f` on the hidden desktop's thread and returns what it returns.
/// `f` gets the desktop, None while it doesn't exist (nothing launched
/// yet).
pub(crate) fn on_desktop<R: Send + 'static>(f: impl FnOnce(Option<&Desktop>) -> R + Send + 'static) -> Result<R, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    submit(Box::new(move |d| {
        let _ = tx.send(f(d));
    }))?;
    rx.recv().map_err(|_| "The hidden desktop's thread stopped.".to_string())
}

/// Runs `f` on the hidden desktop's thread, without waiting for it (the
/// live view's input, from the UI thread).
pub(crate) fn post_to_desktop(f: impl FnOnce(Option<&Desktop>) + Send + 'static) {
    let _ = submit(Box::new(f));
}

impl Desktop {
    /// Its windows, largest first.
    pub(crate) fn list(&self) -> Vec<usize> {
        self.windows()
    }
}

/// Whether `w` is still a window.
pub(crate) fn alive(w: usize) -> bool {
    unsafe { IsWindow(w as Hwnd) != 0 }
}

/// A window's line as `list` prints it: id, process, size, title.
pub(crate) fn line(w: usize) -> String {
    describe(w)
}

pub(crate) fn window_title(w: usize) -> String {
    title(w as Hwnd)
}

pub(crate) fn process_of(w: usize) -> u32 {
    pid_of(w)
}

/// The window's client area, as a picture.
pub(crate) fn picture(w: usize) -> Result<image::RgbaImage, String> {
    let (cw, ch, mut px) = capture(w as Hwnd)?;
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2); // BGRA -> RGBA
        p[3] = 255;
    }
    image::RgbaImage::from_raw(cw, ch, px).ok_or("bad image size".into())
}

/// The window's client size.
pub(crate) fn size(w: usize) -> (i32, i32) {
    client_size(w as Hwnd)
}

/// The user's own windows that could be offered to the model: visible,
/// titled, unowned, not tool windows or cloaked ones (suspended apps),
/// not Oak's; on the calling thread's desktop, front first.
pub(crate) fn user_windows() -> Vec<usize> {
    unsafe extern "system" fn each(h: Hwnd, l: isize) -> i32 {
        let out = unsafe { &mut *(l as *mut Vec<usize>) };
        const GW_OWNER: u32 = 4;
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_TOOLWINDOW: isize = 0x80;
        const DWMWA_CLOAKED: u32 = 14;
        let ok = unsafe { IsWindowVisible(h) != 0 && GetWindowTextLengthW(h) > 0 && GetWindow(h, GW_OWNER).is_null() && GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_TOOLWINDOW == 0 };
        let mut cloaked = 0u32;
        let cloaked = unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4) } == 0 && cloaked != 0;
        if ok && !cloaked && pid_of(h as usize) != std::process::id() {
            out.push(h as usize);
        }
        1
    }
    let mut out: Vec<usize> = vec![];
    unsafe { EnumWindows(each, &mut out as *mut _ as isize) };
    out.retain(|&w| title(w as Hwnd) != "Program Manager");
    out
}

/// The name of window `w`'s program (its file name, as `notepad.exe`).
pub(crate) fn program_of(w: usize) -> String {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let pid = pid_of(w);
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) != 0;
        CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
    }
}

/// `input` to a window of the user's own desktop, on the calling thread:
/// posted as to a hidden one, but the window is never moved.
pub(crate) fn input_gently(w: usize, i: Input) -> Result<(), String> {
    GENTLE.with(|g| g.set(true));
    let r = input(w, i);
    GENTLE.with(|g| g.set(false));
    r
}

/// `live` to a window of the user's own desktop (see `input_gently`).
pub(crate) fn live_gently(w: usize, l: Live) {
    GENTLE.with(|g| g.set(true));
    live(w, l);
    GENTLE.with(|g| g.set(false));
}

/// Input as the WINDOW tool gives it (on the desktop's thread).
pub(crate) enum Input {
    Click { x: i32, y: i32, right: bool, double: bool },
    Move { x: i32, y: i32 },
    Drag { from: (i32, i32), to: (i32, i32), right: bool, throw: bool },
    Scroll { x: i32, y: i32, lines: i32 },
    Type(String),
    Key(String),
    Resize { width: i32, height: i32 },
    Close,
}

pub(crate) fn input(w: usize, i: Input) -> Result<(), String> {
    let w = w as Hwnd;
    match i {
        Input::Click { x, y, right, double } => click(w, x, y, right, double),
        Input::Move { x, y } => point_at(w, x, y, 0),
        Input::Drag { from, to, right, throw } => drag(w, from, to, right, throw),
        Input::Scroll { x, y, lines } => scroll(w, x, y, lines * 120),
        Input::Type(text) => type_text(w, &text),
        Input::Key(spec) => key(w, &spec)?,
        Input::Resize { width, height } => resize(w, width, height),
        Input::Close => post(w, WM_CLOSE, 0, 0),
    }
    Ok(())
}

/// The user's pointer and keys, passed on from the live view as they come
/// (on the desktop's thread): no pauses, buttons held across moves.
pub(crate) enum Live {
    Move { x: i32, y: i32, buttons: usize },
    Button { x: i32, y: i32, button: u8, down: bool, buttons: usize },
    Wheel { x: i32, y: i32, delta: i32 },
    Char(u16),
    Key(String),
}

/// `buttons` in `Live`: which are held (MK_ flags).
pub(crate) const HELD_LEFT: usize = MK_LBUTTON;
pub(crate) const HELD_RIGHT: usize = MK_RBUTTON;
pub(crate) const HELD_MIDDLE: usize = 0x10;

pub(crate) fn live(w: usize, l: Live) {
    let w = w as Hwnd;
    match l {
        Live::Move { x, y, buttons } => {
            under_cursor(w, x, y);
            mouse(w, WM_MOUSEMOVE, buttons, x, y);
        }
        Live::Button { x, y, button, down, buttons } => {
            const WM_MBUTTONDOWN: u32 = 0x0207;
            let msg = match button {
                1 => WM_RBUTTONDOWN,
                2 => WM_MBUTTONDOWN,
                _ => WM_LBUTTONDOWN,
            } + if down { 0 } else { 1 };
            if down {
                point_at(w, x, y, buttons);
            } else {
                under_cursor(w, x, y);
            }
            mouse(w, msg, buttons, x, y);
        }
        Live::Wheel { x, y, delta } => scroll(w, x, y, delta),
        Live::Char(unit) => type_unit(w, unit),
        Live::Key(spec) => {
            let _ = key(w, &spec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_the_c_runtime() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn cmd_commands_keep_their_shell_quotes() {
        let line = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            command_line(&args.iter().collect::<Vec<_>>())
        };
        assert_eq!(line(&["cmd.exe", "/d", "/c", r#"set "NAME=two words" && "C:\Program Files\App\app.exe" "an argument""#]),
            r#"cmd.exe /d /s /c "set "NAME=two words" && "C:\Program Files\App\app.exe" "an argument"""#);
        assert_eq!(line(&[r"C:\Windows\System32\CMD.EXE", "/S", "/K", r#""C:\App Folder\app.exe""#]),
            r#"C:\Windows\System32\CMD.EXE /S /K ""C:\App Folder\app.exe"""#);
        // Only cmd is special: an ordinary program's /c argument is literal.
        let args = ["python.exe", "/c", r#"say "hi""#, r"C:\dir with space\", ""];
        assert_eq!(line(&args), args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
        assert_eq!(line(&["not-cmd.exe", "/c", "two words"]), "not-cmd.exe /c \"two words\"");
    }

    #[test]
    fn cmd_executes_quoted_assignments_and_a_quoted_program() {
        use std::os::windows::process::CommandExt;
        let exe = std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let command = format!(r#"set "OAK_WINDOW_CMD_QUOTE_PROBE=two words & parentheses (kept)" && "{exe}" /d /c set OAK_WINDOW_CMD_QUOTE_PROBE && exit /b 0"#);
        for flags in [vec!["/c"], vec!["/d", "/s", "/C"], vec!["/d", "/k"]] {
            let args: Vec<String> = std::iter::once(exe.clone()).chain(flags.iter().map(|s| s.to_string())).chain(std::iter::once(command.clone())).collect();
            let line = command_line(&args.iter().collect::<Vec<_>>());
            let raw = line.strip_prefix(&format!("{} ", quote(&exe))).unwrap();
            let out = std::process::Command::new(&exe)
                .raw_arg(raw)
                .env_remove("OAK_WINDOW_CMD_QUOTE_PROBE")
                .creation_flags(0x0800_0000)
                .output()
                .unwrap();
            assert!(out.status.success(), "{line}: {}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "OAK_WINDOW_CMD_QUOTE_PROBE=two words & parentheses (kept)", "{line}");
        }
    }

    /// Launches OAK's own debug build on the hidden desktop, types into it
    /// and screenshots it, and checks the user's foreground window and mouse
    /// never moved. Needs `cargo build` first; run with
    /// `cargo test live_hidden_desktop -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_hidden_desktop() {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetForegroundWindow() -> Hwnd;
            fn GetCursorPos(p: *mut [i32; 2]) -> i32;
        }
        let here = || unsafe {
            let mut p = [0; 2];
            GetCursorPos(&mut p);
            (GetForegroundWindow() as usize, p)
        };
        let before = here();
        let dir = crate::temp_dir().join("oak-winctl-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        std::fs::write(dir.join("auth.json"), r#"{"type":"api_key","key":"not-a-real-key"}"#).unwrap();
        // The app and the driver are copies, so the build stays unlocked.
        let built = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("oak.exe");
        let (app, cli) = (dir.join("app.exe"), dir.join("cli.exe"));
        std::fs::copy(&built, &app).unwrap();
        std::fs::copy(&built, &cli).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new(&cli).arg("--window").args(args).args(["--desktop", "oak-test"]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
            println!("{args:?}: {}", text.trim());
            assert!(out.status.success(), "{args:?} failed: {text}");
            text
        };
        let mut launch = std::process::Command::new(&cli)
            .args(["--window", "launch", "--desktop", "oak-test", "--size", "1300x800", "--"])
            .arg(&app)
            .env("OAK_DIR", &dir)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let t = Instant::now();
        while !run(&["list"]).contains("\"Oak\"") {
            assert!(t.elapsed() < Duration::from_secs(30), "no window");
            std::thread::sleep(Duration::from_millis(500));
        }
        std::thread::sleep(Duration::from_secs(2));
        let blank = dir.join("blank.png");
        run(&["shot", blank.to_str().unwrap()]);
        run(&["click", "700", "740"]);
        run(&["type", "Typed on a desktop nobody can see"]);
        let typed = dir.join("typed.png");
        run(&["shot", typed.to_str().unwrap()]);
        let (a, b) = (image::open(&blank).unwrap().to_rgb8(), image::open(&typed).unwrap().to_rgb8());
        assert_eq!(a.dimensions(), b.dimensions());
        // It painted (mostly the page color, not black) and the typing changed it.
        let page = a.pixels().filter(|p| p.0 == [0x14, 0x14, 0x13] || p.0 == [0, 0, 0]).count();
        assert!(page > a.pixels().len() / 2, "the window didn't paint");
        assert!(a.pixels().any(|p| p.0 != [0, 0, 0]), "the capture is black");
        let changed = a.pixels().zip(b.pixels()).filter(|(x, y)| x != y).count();
        assert!(changed > 200, "typing changed only {changed} pixels");
        run(&["close"]);
        let status = launch.wait().unwrap();
        assert!(status.success());
        let mut log = String::new();
        std::io::Read::read_to_string(launch.stdout.as_mut().unwrap(), &mut log).unwrap();
        assert!(log.contains("The app exited with code 0"), "{log}");
        assert_eq!(here(), before, "the user's foreground window or mouse moved");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
