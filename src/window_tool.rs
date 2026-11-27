//! The WINDOW tool (Windows): GUI apps on Oak's hidden desktop
//! (`winctl.rs`), driven without touching the user's screen, mouse,
//! keyboard or focus. `launch` starts an app there as the conversation's
//! own (through `oak --window launch`, a child of Oak's in a job object, so
//! the app ends with Oak, with `close`, or when the conversation is
//! deleted); the other actions run on the hidden desktop's thread
//! (`winctl::on_desktop`). Window ids read `hidden:0x…`.
//!
//! `attach` asks the user for one of their own windows (`approval.rs`: they
//! pick one in Oak, or decline; asked anew each time): the conversation may
//! then use it, as `user:0x…`, until `detach` or its deletion. Its input is
//! posted as to a hidden window but never moves it, on the calling thread
//! (`winctl::input_gently`); it can't be closed. The window a conversation
//! last launched, attached or acted on is its current one: actions without
//! `window` go to it, and the live view beside the transcript
//! (`window_view.rs`) shows it.

use crate::winctl::{self, Input};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// How long `launch` waits for the app's first window.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);

/// An app a conversation launched.
struct App {
    /// The `oak --window launch` process holding it.
    child: std::process::Child,
    /// The app's own process.
    pid: u32,
    program: String,
    /// Its first window (None: none yet).
    window: Option<usize>,
    /// Its output, in the conversation's scratchpad.
    log: PathBuf,
    /// Keeps the job that ends it with Oak.
    _job: Job,
}

/// RAII ownership of a checked kill-on-close job. Closing a conversation
/// or a failed launch closes this handle instead of leaking it until exit.
pub(crate) struct Job(usize);
impl Job {
    fn new(child: &std::process::Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() { return Err(format!("Couldn't create app job: {}", std::io::Error::last_os_error())); }
            let job = Self(h as usize);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info as *const _ as _, std::mem::size_of_val(&info) as u32) == 0 || AssignProcessToJobObject(h, child.as_raw_handle() as _) == 0 {
                return Err(format!("Couldn't isolate app process tree: {}", std::io::Error::last_os_error()));
            }
            Ok(job)
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) { unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0 as _); } }
}

#[derive(Default)]
struct State {
    /// Each conversation's apps.
    apps: HashMap<String, Vec<App>>,
    /// Each conversation's current window.
    current: HashMap<String, usize>,
    /// The user's own windows each conversation was allowed.
    attached: HashMap<String, Vec<usize>>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

/// A window id as the tool shows it.
pub(crate) fn shown_id(w: usize) -> String {
    if is_users(w) { format!("user:{w:#x}") } else { format!("hidden:{w:#x}") }
}

/// Whether `w` is a window of the user's own that a conversation attached.
pub(crate) fn is_users(w: usize) -> bool {
    STATE.lock().unwrap().attached.values().any(|v| v.contains(&w))
}

/// A window id as given (`hidden:0x…`, `user:0x…`, `0x…` or a number), as
/// conversation `conv` may use it: the user's own windows only once it
/// attached them.
fn parse_id_for(conv: &str, v: &Value) -> Result<Option<usize>, String> {
    let users = matches!(v, Value::String(s) if s.trim().starts_with("user:"));
    let w = parse_id(&match v {
        Value::String(s) => Value::String(s.trim().trim_start_matches("user:").to_string()),
        v => v.clone(),
    })?;
    let Some(w) = w else { return Ok(None) };
    if !crate::mcp::blender_run::can_use(w, conv) {
        return Err("That Blender belongs to another agent. Load BLENDER for your own independent instance.".into());
    }
    if others_app(conv, w) {
        return Err("That window is another agent's app. Launch your own (action=launch).".into());
    }
    let allowed = STATE.lock().unwrap().attached.get(conv).is_some_and(|a| a.contains(&w));
    if allowed {
        return Ok(Some(w));
    }
    if users || !on_hidden(w) {
        return Err(format!("{} isn't a window you may use: the user's own windows need their permission (action=attach), and the hidden desktop's are in action=list.", if users { format!("user:{w:#x}") } else { format!("{w:#x}") }));
    }
    Ok(Some(w))
}

/// Whether `w` belongs to an app another conversation launched (its first
/// window or any other window of that process): agents each drive their own.
fn others_app(conv: &str, w: usize) -> bool {
    let pid = winctl::process_of(w);
    STATE.lock().unwrap().apps.iter().any(|(id, apps)| id != conv && apps.iter().any(|a| a.window == Some(w) || (pid != 0 && a.pid == pid)))
}

/// Whether `w` is a window on the hidden desktop.
fn on_hidden(w: usize) -> bool {
    winctl::on_desktop(move |d| d.is_some_and(|d| d.list().contains(&w))).unwrap_or(false)
}

/// A window id as given: `hidden:0x…`, `0x…` or a number.
fn parse_id(v: &Value) -> Result<Option<usize>, String> {
    let s = match v {
        Value::Null => return Ok(None),
        Value::Number(n) => return n.as_u64().map(|n| Some(n as usize)).ok_or("`window` is an id from action=list".into()),
        Value::String(s) => s.trim(),
        _ => return Err("`window` is an id from action=list".into()),
    };
    if s.starts_with("user:") {
        return Err("That is a window of the user's own desktop: the WINDOW tool works on hidden windows only.".into());
    }
    let s = s.strip_prefix("hidden:").unwrap_or(s);
    let n = match s.strip_prefix("0x") {
        Some(h) => usize::from_str_radix(h, 16),
        None => s.parse(),
    };
    n.map(Some).map_err(|_| format!("Not a window id: {s} (ids look like hidden:0x1a2b)"))
}

/// The conversation's current window, if it is still there.
pub(crate) fn current(conv: &str) -> Option<usize> {
    let w = *STATE.lock().unwrap().current.get(conv)?;
    winctl::alive(w).then_some(w)
}

pub(crate) fn set_current(conv: &str, w: usize) {
    STATE.lock().unwrap().current.insert(conv.to_string(), w);
}


/// Runs WINDOW's action `name` (WINDOW_CLICK…) for conversation `conv`,
/// whose folder is `folder` and scratchpad `scratch`. Returns the result's
/// blocks (text, and a picture for shots).
pub(crate) fn run(conv: &str, folder: &Path, scratch: &Path, name: &str, input: &Value) -> Result<Vec<Value>, String> {
    let action = name.strip_prefix("WINDOW_").unwrap_or(name).to_lowercase();
    match action.as_str() {
        "launch" => return launch(conv, folder, scratch, input),
        "list" => return list(conv),
        "attach" => return attach(conv, input),
        _ => {}
    }
    let given = parse_id_for(conv, &input["window"])?;
    let conv_s = conv.to_string();
    let w = match given {
        Some(w) => w,
        None => match current(conv) {
            Some(w) => w,
            None => return Err("No current window for this agent. Launch an app, attach with permission, or select one of your windows from action=list.".into()),
        },
    };
    if !crate::mcp::blender_run::can_use(w, conv) {
        return Err("That Blender belongs to another agent.".into());
    }
    if !winctl::alive(w) {
        return Err(format!("No window {} (it closed?): see action=list.", shown_id(w)));
    }
    set_current(&conv_s, w);
    let n = |k: &str| input[k].as_i64().map(|v| v as i32).ok_or(format!("Missing required field `{k}` (a number)"));
    let wait = input["wait"].as_u64().unwrap_or(300).min(10_000);
    let text = |k: &str| input[k].as_str().map(str::to_string).ok_or(format!("Missing required field `{k}`"));
    let right = input["button"].as_str() == Some("right");
    let done: String = match action.as_str() {
        "shot" => return shot(w, None),
        "click" => {
            let (x, y) = (n("x")?, n("y")?);
            let double = input["clicks"].as_u64() == Some(2);
            act(w, wait, move || Input::Click { x, y, right, double })?;
            format!("Clicked {x},{y}")
        }
        "move" => {
            let (x, y) = (n("x")?, n("y")?);
            act(w, wait, move || Input::Move { x, y })?;
            format!("Pointer at {x},{y}")
        }
        "drag" => {
            let (from, to) = ((n("x")?, n("y")?), (n("to_x")?, n("to_y")?));
            let throw = input["throw"].as_bool().unwrap_or(false);
            act(w, wait, move || Input::Drag { from, to, right, throw })?;
            format!("Dragged {},{} to {},{}", from.0, from.1, to.0, to.1)
        }
        "scroll" => {
            let (x, y, lines) = (n("x")?, n("y")?, n("lines")?);
            act(w, wait, move || Input::Scroll { x, y, lines })?;
            format!("Scrolled {lines} line(s) at {x},{y}")
        }
        "type" => {
            let t = text("text")?;
            let len = t.chars().count();
            act(w, wait, move || Input::Type(t))?;
            format!("Typed {len} character(s)")
        }
        "key" => {
            let k = text("key")?;
            let k2 = k.clone();
            act(w, wait, move || Input::Key(k2))?;
            format!("Pressed {k}")
        }
        "resize" => {
            let (width, height) = (n("width")?, n("height")?);
            act(w, wait, move || Input::Resize { width, height })?;
            let (cw, ch) = winctl::size(w);
            format!("Resized: its client area is {cw}x{ch} now")
        }
        "close" if is_users(w) => return Err("That is the user's own window: ask them to close it if it should be (action=detach lets go of it).".into()),
        "close" => return close(conv, w),
        "detach" => return Ok(vec![json!({"type": "text", "text": detach(conv, w)?})]),
        other => return Err(format!("Unknown action `{other}`")),
    };
    let done = format!("{done} in {}.", shown_id(w));
    if input["shot"].as_bool().unwrap_or(false) {
        return shot(w, Some(&done));
    }
    Ok(vec![json!({"type": "text", "text": done})])
}

/// Input to window `w` on the desktop's thread, then `wait` ms for the app
/// to repaint.
fn act(w: usize, wait: u64, i: impl FnOnce() -> Input + Send + 'static) -> Result<(), String> {
    if is_users(w) {
        winctl::input_gently(w, i())?;
        std::thread::sleep(Duration::from_millis(wait));
        return Ok(());
    }
    // A Blender Oak runs takes its input as Blender events.
    if let Some(port) = crate::mcp::blender_run::port_of_window(w) {
        let i = i();
        if !matches!(i, Input::Resize { .. } | Input::Close) {
            crate::mcp::blender_run::input(port, &i)?;
            std::thread::sleep(Duration::from_millis(wait));
            return Ok(());
        }
        return winctl::on_desktop(move |_| winctl::input(w, i))?;
    }
    winctl::on_desktop(move |_| {
        winctl::input(w, i())?;
        std::thread::sleep(Duration::from_millis(wait));
        Ok::<_, String>(())
    })?
}

/// A screenshot of window `w`'s client area, after `before` (what was
/// done) if given.
fn shot(w: usize, before: Option<&str>) -> Result<Vec<Value>, String> {
    let img = picture(w)?;
    let (width, height) = (img.width(), img.height());
    let mut png = std::io::Cursor::new(vec![]);
    img.write_to(&mut png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    let img = crate::images::from_bytes(&png.into_inner())?;
    let scaled = if img.width != width { format!("; sent at {}x{}", img.width, img.height) } else { String::new() };
    let title = winctl::window_title(w);
    let note = format!("{}Screenshot of {} \"{title}\", {width}x{height} (x,y are pixels in it{scaled}).", before.map(|b| format!("{b} ")).unwrap_or_default(), shown_id(w));
    Ok(vec![json!({"type": "text", "text": note}), img.block()])
}

/// Window `w`'s picture: from Blender itself for a Blender Oak runs (the
/// hidden desktop can't capture its OpenGL), else its client area.
pub(crate) fn picture(w: usize) -> Result<image::RgbaImage, String> {
    match crate::mcp::blender_run::port_of_window(w) {
        Some(port) => crate::mcp::blender_run::window_frame(port).ok_or("Blender didn't answer within 5 seconds (busy?).".into()),
        None if is_users(w) => winctl::picture(w),
        None => winctl::on_desktop(move |_| winctl::picture(w))?,
    }
}

fn list(conv: &str) -> Result<Vec<Value>, String> {
    let ws = winctl::on_desktop(|d| d.map(|d| d.list()).unwrap_or_default())?;
    let ws: Vec<usize> = ws.into_iter().filter(|&w| crate::mcp::blender_run::can_use(w, conv) && !others_app(conv, w)).collect();
    let s = STATE.lock().unwrap();
    let mine: Vec<u32> = s.apps.get(conv).into_iter().flatten().map(|a| a.pid).collect();
    let current = s.current.get(conv).copied();
    let attached: Vec<usize> = s.attached.get(conv).into_iter().flatten().copied().filter(|&w| winctl::alive(w)).collect();
    drop(s);
    let users = if attached.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = attached
            .iter()
            .map(|&w| {
                let line = winctl::line(w).replacen(&format!("window {w:#x}"), &format!("user:{w:#x}"), 1);
                if current == Some(w) { format!("{line}  (current)") } else { line }
            })
            .collect();
        format!("\n\nThe user's own windows you attached:\n{}", lines.join("\n"))
    };
    if ws.is_empty() {
        return Ok(vec![json!({"type": "text", "text": format!("No windows on the hidden desktop.{users}")})]);
    }
    let lines: Vec<String> = ws
        .iter()
        .map(|&w| {
            let line = winctl::line(w).replacen(&format!("window {w:#x}"), &shown_id(w), 1);
            let mut tags = vec![];
            if let Some(b) = crate::mcp::blender_run::describe(w, conv) {
                tags.push(b);
            }
            if mine.contains(&winctl::process_of(w)) {
                tags.push("yours");
            }
            if current == Some(w) {
                tags.push("current");
            }
            if tags.is_empty() { line } else { format!("{line}  ({})", tags.join(", ")) }
        })
        .collect();
    Ok(vec![json!({"type": "text", "text": format!("Windows on the hidden desktop (id, process, client size, title):\n{}{users}", lines.join("\n"))})])
}

/// Asks the user for one of their own windows (`title`: part of the one
/// the model means, picked first); once allowed it is the current window,
/// shown.
fn attach(conv: &str, input: &Value) -> Result<Vec<Value>, String> {
    let why = input["why"].as_str().unwrap_or("").trim();
    if why.is_empty() {
        return Err("Say why in `why`: the user reads it before choosing.".into());
    }
    let taken: Vec<usize> = STATE.lock().unwrap().attached.get(conv).cloned().unwrap_or_default();
    let ws: Vec<usize> = winctl::user_windows().into_iter().filter(|w| !taken.contains(w)).collect();
    let choices: Vec<crate::approval::Choice> =
        ws.iter().map(|&w| crate::approval::Choice { label: winctl::window_title(w), detail: winctl::program_of(w) }).collect();
    let hint = input["title"].as_str().unwrap_or("").trim().to_lowercase();
    let suggested = (!hint.is_empty()).then(|| choices.iter().position(|c| c.label.to_lowercase().contains(&hint) || c.detail.to_lowercase().contains(&hint))).flatten();
    let i = crate::approval::ask(conv, "one of your windows", why, choices, suggested)?;
    let w = ws[i];
    if !winctl::alive(w) {
        return Err("The window the user picked has closed meanwhile.".into());
    }
    {
        let mut s = STATE.lock().unwrap();
        s.attached.entry(conv.to_string()).or_default().push(w);
        s.current.insert(conv.to_string(), w);
    }
    let done = format!(
        "The user let you use their window user:{w:#x} (\"{}\", {}). It is on their screen and they may use it too: act on what you see, and let go with action=detach when done.",
        winctl::window_title(w),
        winctl::program_of(w)
    );
    shot(w, Some(&done))
}

/// Lets go of the user's window `w`.
fn detach(conv: &str, w: usize) -> Result<String, String> {
    let mut s = STATE.lock().unwrap();
    let list = s.attached.entry(conv.to_string()).or_default();
    let Some(i) = list.iter().position(|&x| x == w) else {
        return Err(format!("{w:#x} isn't one of the user's windows you attached."));
    };
    list.remove(i);
    if s.current.get(conv) == Some(&w) {
        s.current.remove(conv);
    }
    Ok(format!("Let go of the user's window {w:#x}."))
}

/// The program `launch` was given: a path relative to the folder if it is
/// one there, else as given (a name on the PATH).
fn program_path(folder: &Path, program: &str) -> String {
    let p = Path::new(program);
    if p.is_relative() && (program.contains(['/', '\\']) || folder.join(p).exists()) {
        let joined = folder.join(p);
        if joined.exists() {
            return joined.to_string_lossy().into_owned();
        }
    }
    program.to_string()
}

fn launch(conv: &str, folder: &Path, scratch: &Path, input: &Value) -> Result<Vec<Value>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find Oak's program: {e}"))?;
    launch_with(&exe, conv, folder, scratch, input)
}

/// `launch` through Oak program `exe`.
fn launch_with(exe: &Path, conv: &str, folder: &Path, scratch: &Path, input: &Value) -> Result<Vec<Value>, String> {
    let program = input["program"].as_str().map(str::trim).filter(|p| !p.is_empty()).ok_or("Missing required field `program`")?;
    let args: Vec<String> = match &input["args"] {
        Value::Array(a) => a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect(),
        Value::String(s) if !s.trim().is_empty() => s.split_whitespace().map(str::to_string).collect(),
        _ => vec![],
    };
    let width = input["width"].as_u64().unwrap_or(1280).clamp(200, 7680) as u32;
    let height = input["height"].as_u64().unwrap_or(800).clamp(150, 4320) as u32;
    let path = program_path(folder, program);
    let _ = std::fs::create_dir_all(scratch);
    let stem: String = Path::new(program).file_stem().unwrap_or_default().to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let log = scratch.join(format!("window-{}-{}.log", if stem.is_empty() { "app" } else { &stem }, crate::api::now_ms()));
    let Started { child, pid, window: w, job } = start(exe, program, &path, &args, &[], folder, &log, (width, height))?;
    let mut s = STATE.lock().unwrap();
    s.apps.entry(conv.to_string()).or_default().push(App { child, pid, program: program.to_string(), window: Some(w), log: log.clone(), _job: job });
    s.current.insert(conv.to_string(), w);
    drop(s);
    let done = format!(
        "Launched {program} on the hidden desktop (process {pid}); its window is {} and is your current one. The user sees it live beside the transcript. Its output goes to {}. It runs until action=close (or Oak exits).",
        shown_id(w),
        log.display()
    );
    shot(w, Some(&done))
}

/// An app started on the hidden desktop.
pub(crate) struct Started {
    /// The `oak --window launch` process holding it (ending it ends the
    /// app's tree; see `kill_tree`).
    pub(crate) child: std::process::Child,
    pub(crate) pid: u32,
    /// Its first window.
    pub(crate) window: usize,
    /// The job that ends it with Oak (kept open while it runs).
    pub(crate) job: Job,
}

/// Starts `path` (shown as `program`) with `args` and `env` on the hidden
/// desktop through Oak program `exe`'s `--window launch`, in `folder`, its
/// output to `log`, its window `size`; waits for that window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start(exe: &Path, program: &str, path: &str, args: &[String], env: &[(&str, String)], folder: &Path, log: &Path, size: (u32, u32)) -> Result<Started, String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--window", "launch", "--job-wait", "--desktop", winctl::DESKTOP, "--size", &format!("{}x{}", size.0, size.1), "--"]).arg(path).args(args);
    cmd.envs(env.iter().map(|(k, v)| (*k, v.as_str())));
    cmd.current_dir(folder).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {program}: {e}"))?;
    let job = match Job::new(&child) {
        Ok(job) => job,
        Err(e) => { kill_tree(child.id()); return Err(e); }
    };
    // Do not let descendants escape before the kill-on-close job owns it.
    child.stdin.as_mut().ok_or("App launcher has no start pipe")?.write_all(b"\n").map_err(|e| format!("Couldn't release app launcher: {e}"))?;
    // Its lines go to the log; the launcher's first ones (the app's pid,
    // its window) come back here too.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log_file = std::sync::Arc::new(Mutex::new(std::fs::File::create(log).ok()));
    let pump = |r: Box<dyn std::io::Read + Send>, tx: std::sync::mpsc::Sender<String>, f: std::sync::Arc<Mutex<Option<std::fs::File>>>| {
        let _ = std::thread::Builder::new().name("window-app-output".into()).spawn(move || {
            for line in BufReader::new(r).lines() {
                let Ok(line) = line else { break };
                if let Some(f) = f.lock().unwrap().as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                let _ = tx.send(line);
            }
        });
    };
    if let Some(o) = stdout {
        pump(Box::new(o), tx.clone(), log_file.clone());
    }
    if let Some(e) = stderr {
        pump(Box::new(e), tx, log_file);
    }
    let started = std::time::Instant::now();
    let (mut pid, mut window, mut said) = (0u32, None, vec![]);
    while started.elapsed() < LAUNCH_TIMEOUT {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if let Some(p) = line.split("(pid ").nth(1).and_then(|r| r.split(')').next()).and_then(|p| p.parse().ok()).filter(|_| line.starts_with("Started ")) {
                    pid = p;
                } else if let Some(id) = line.strip_prefix("window 0x").and_then(|r| r.split_whitespace().next()).and_then(|h| usize::from_str_radix(h, 16).ok()) {
                    window = Some(id);
                    break;
                } else if said.len() < 20 || line.starts_with("oak --window:") || line.starts_with("The app exited") || line.starts_with("A window of another") {
                    said.push(line);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        if let Ok(Some(_)) = child.try_wait() {
            // Its last lines.
            while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
                said.push(line);
            }
            break;
        }
    }
    let Some(window) = window else {
        let exited = matches!(child.try_wait(), Ok(Some(_))) || said.iter().any(|l| l.starts_with("The app exited"));
        let handed_off = said.iter().any(|l| l.starts_with("A window of another"));
        if !exited {
            kill_tree(child.id());
        }
        let tail = said.iter().rev().take(8).rev().cloned().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "{program} {}.{}",
            if handed_off {
                "handed off to a program already running (as Windows 11's Store apps do), whose window isn't yours to drive: such apps can't run here"
            } else if exited {
                "exited without showing a window"
            } else {
                "showed no window within 60 seconds, so it was ended"
            },
            if tail.is_empty() { String::new() } else { format!(" Its output:\n{tail}") }
        ));
    };
    Ok(Started { child, pid, window, job })
}

/// Ends a process and those it started.
pub(crate) fn kill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).creation_flags(0x0800_0000).output();
}

/// Closes window `w`: asks it to; an app this conversation launched is
/// ended if it is still running after 5 seconds (all its windows gone).
fn close(conv: &str, w: usize) -> Result<Vec<Value>, String> {
    if crate::mcp::blender_run::port_of_window(w).is_some() {
        if !crate::mcp::blender_run::can_use(w, conv) { return Err("That Blender belongs to another agent.".into()); }
        let text = crate::mcp::blender_run::close_for(conv, false)?;
        return Ok(close_result(conv, w, "Blender", &format!(" {text}")));
    }
    let pid = winctl::process_of(w);
    let title = winctl::window_title(w);
    winctl::on_desktop(move |_| winctl::input(w, Input::Close))??;
    let mine = {
        let mut s = STATE.lock().unwrap();
        let apps = s.apps.entry(conv.to_string()).or_default();
        // A launcher (cmd.exe, for example) may create the window in a
        // child process rather than in its own.
        apps.iter().position(|a| a.pid == pid || a.window == Some(w)).map(|i| apps.remove(i))
    };
    let mut ended = String::new();
    if let Some(mut app) = mine {
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_secs(5) && !matches!(app.child.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !matches!(app.child.try_wait(), Ok(Some(_))) {
            kill_tree(app.child.id());
            ended = format!(" {} didn't exit within 5 seconds, so it was ended.", app.program);
        } else {
            ended = format!(" {} exited.", app.program);
        }
        let _ = (app.window, &app.log);
    }
    Ok(close_result(conv, w, &title, &ended))
}

/// Clears the closed window before formatting its result. `shown_id`
/// reads STATE too: never call it while holding the state lock.
fn close_result(conv: &str, w: usize, title: &str, ended: &str) -> Vec<Value> {
    {
        let mut s = STATE.lock().unwrap();
        if s.current.get(conv) == Some(&w) {
            s.current.remove(conv);
        }
    }
    vec![json!({"type": "text", "text": format!("Asked {} \"{title}\" to close.{ended}", shown_id(w))})]
}

/// Ends the apps conversation `conv` launched (it was deleted).
pub(crate) fn end(conv: &str) {
    let apps = {
        let mut s = STATE.lock().unwrap();
        s.current.remove(conv);
        s.attached.remove(conv);
        s.apps.remove(conv).unwrap_or_default()
    };
    for a in apps {
        kill_tree(a.child.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_window_ids() {
        assert_eq!(parse_id(&json!("hidden:0x1a2b")).unwrap(), Some(0x1a2b));
        assert_eq!(parse_id(&json!("0x10")).unwrap(), Some(16));
        assert_eq!(parse_id(&json!(42)).unwrap(), Some(42));
        assert_eq!(parse_id(&Value::Null).unwrap(), None);
        assert!(parse_id(&json!("user:0x10")).unwrap_err().contains("user's own"));
        assert!(parse_id(&json!("nope")).is_err());
        assert_eq!(shown_id(0x1a2b), "hidden:0x1a2b");
    }

    /// Isolated so a recurrence of the mutex deadlock fails in five
    /// seconds instead of hanging this process and the rest of the suite.
    #[test]
    fn close_releases_state_before_formatting() {
        const CHILD: &str = "OAK_TEST_WINDOW_CLOSE_RESULT";
        if std::env::var_os(CHILD).is_some() {
            let conv = "close-lock-test";
            set_current(conv, 0x123);
            let result = close_result(conv, 0x123, "Argument probe", " python.exe exited.");
            assert_eq!(result[0]["text"], "Asked hidden:0x123 \"Argument probe\" to close. python.exe exited.");
            assert!(!STATE.lock().unwrap().current.contains_key(conv));
            // A different window selected meanwhile must stay current.
            set_current(conv, 0x456);
            close_result(conv, 0x123, "Argument probe", "");
            assert_eq!(STATE.lock().unwrap().current.get(conv), Some(&0x456));
            return;
        }
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "window_tool::tests::close_releases_state_before_formatting", "--nocapture"])
            .env(CHILD, "1")
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let began = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "close-result regression failed: {status}");
                break;
            }
            if began.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("close result deadlocked while formatting the window id");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `cargo build`, then `cargo test closes_tk_apps_and_cmd_wrappers -- --ignored`.
    /// Uses Python/Tk if installed (OAK_TEST_PYTHON overrides its path).
    #[test]
    #[ignore]
    fn closes_tk_apps_and_cmd_wrappers() {
        use std::os::windows::process::CommandExt;
        let folder = std::env::current_dir().unwrap();
        let scratch = std::env::var_os("OAK_WINDOW_TEST_DIR").map(PathBuf::from)
            .unwrap_or_else(|| crate::temp_dir().join(format!("oak-window-close-{}", std::process::id())));
        std::fs::create_dir_all(&scratch).unwrap();
        let python = std::env::var("OAK_TEST_PYTHON").unwrap_or_else(|_| {
            let out = std::process::Command::new("python").args(["-c", "import sys; print(sys.executable)"])
                .creation_flags(0x0800_0000).output().expect("Python with Tk is needed");
            assert!(out.status.success(), "Python with Tk is needed");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        });
        let probe = scratch.join("argument probe.py");
        std::fs::write(&probe, r#"import ctypes, os, sys, tkinter as tk
assert not ctypes.windll.kernel32.GetConsoleWindow(), 'a console escaped the hidden desktop'
mode = sys.argv[1]
if mode == 'direct':
    assert sys.argv[2:] == ['two words', 'say "hi"', 'C:\\tail with space\\', ''], repr(sys.argv)
else:
    assert os.environ['OAK_WINDOW_PROBE'] == 'from cmd & kept', repr(dict(os.environ))
root = tk.Tk()
root.title('Oak WINDOW close regression ' + mode)
def close():
    print('closed normally', flush=True)
    root.destroy()
root.protocol('WM_DELETE_WINDOW', (lambda: None) if mode == 'forced' else close)
print('probe ready ' + mode, flush=True)
root.mainloop()
"#).unwrap();
        let oak = folder.join("target/debug/oak.exe");
        assert!(oak.exists(), "cargo build first");
        for mode in ["direct", "cmd", "forced"] {
            let conv = format!("window-close-regression-{mode}");
            let input = if mode == "direct" {
                json!({"program": python, "args": ["-u", probe.to_str().unwrap(), mode, "two words", "say \"hi\"", "C:\\tail with space\\", ""]})
            } else {
                let command = format!(r#"set "OAK_WINDOW_PROBE=from cmd & kept" && "{python}" -u "{}" {mode}"#, probe.display());
                json!({"program": "cmd.exe", "args": ["/d", "/c", command]})
            };
            let launched = launch_with(&oak, &conv, &folder, &scratch, &input).unwrap();
            assert_eq!(launched[1]["type"], "image");
            let w = current(&conv).unwrap();
            let log = STATE.lock().unwrap().apps[&conv][0].log.clone();
            let began = std::time::Instant::now();
            let result = run(&conv, &folder, &scratch, "WINDOW_CLOSE", &json!({})).unwrap();
            assert!(began.elapsed() < Duration::from_secs(12), "close was not bounded");
            let text = result[0]["text"].as_str().unwrap();
            assert!(text.contains(if mode == "forced" { "so it was ended" } else { "exited." }), "{text}");
            assert!(!winctl::alive(w), "the probe window is still open");
            assert!(current(&conv).is_none());
            assert!(STATE.lock().unwrap().apps[&conv].is_empty(), "the launcher was not recognized as the owner");
            let output = std::fs::read_to_string(log).unwrap();
            assert!(output.contains(&format!("probe ready {mode}")), "{output}");
            if mode != "forced" {
                assert!(output.contains("closed normally"), "{output}");
            }
            // Another tool call must still be able to use the state and desktop.
            assert!(list(&conv).is_ok());
            println!("{mode}: {text}");
            end(&conv);
        }
    }

    /// Needs a desktop session: `cargo test window_tool -- --ignored`.
    #[test]
    #[ignore]
    fn drives_an_app() {
        let scratch = crate::temp_dir().join("oak-window-tool-test");
        let folder = std::env::current_dir().unwrap();
        // current_exe is the test binary here: launch through the built Oak.
        let oak = folder.join("target/debug/oak.exe");
        assert!(oak.exists(), "cargo build first");
        let r = launch_with(&oak, "t", &folder, &scratch, &json!({"program": "charmap.exe"})).unwrap();
        println!("{}", r[0]["text"]);
        assert_eq!(r[1]["type"], "image");
        let r = run("t", &folder, &scratch, "WINDOW_CLICK", &json!({"x": 60, "y": 90, "shot": true})).unwrap();
        assert_eq!(r[1]["type"], "image");
        println!("{}", r[0]["text"]);
        println!("{}", run("t", &folder, &scratch, "WINDOW_LIST", &json!({})).unwrap()[0]["text"]);
        let r = run("t", &folder, &scratch, "WINDOW_KEY", &json!({"key": "ctrl+a"})).unwrap();
        println!("{}", r[0]["text"]);
        assert!(current("t").is_some());
        end("t");
    }
}
