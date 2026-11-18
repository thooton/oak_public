//! Private hidden Blender instances, one per agent/conversation. Loading
//! BLENDER starts that agent's process, isolated port and scratchpad settings.
//! No call falls back to another agent's process or to the user's Blender.
//! `use_own` can select a portable executable; `restart` and `close` refuse
//! to discard unsaved changes unless explicitly allowed. The user's Blender
//! is an explicit, exclusive attachment (`use_users`), never the default.
//!
//! The add-on supplies window pictures and simulated input to Oak's
//! interactive view; ordinary Windows screenshots cannot capture its OpenGL.
//! Caller context is scoped with an unwind-safe guard. Standalone stdio
//! clients without an Oak caller keep using the user's add-on port.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
#[cfg(windows)]
use std::time::{Duration, Instant};

/// The user's own Blender's port (the add-on's default).
pub const USER_PORT: u16 = 9876;

/// Who is calling: the conversation, its folder and scratchpad.
#[derive(Clone, Default)]
pub struct Caller {
    pub conv: String,
    pub folder: PathBuf,
    pub scratch: PathBuf,
    pub cancelled: Arc<AtomicBool>,
}

thread_local! {
    static CALLER: RefCell<Option<Caller>> = const { RefCell::new(None) };
}

/// Runs `f` as conversation `c`'s call (a built-in server runs on the
/// calling thread).
pub fn with_caller<R>(c: Caller, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Caller>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CALLER.with(|k| *k.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CALLER.with(|k| k.replace(Some(c))));
    f()
}

fn caller() -> Option<Caller> {
    CALLER.with(|k| k.borrow().clone())
}

/// Which Blender a conversation works in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    /// Explicit attachment to the user's add-on, exclusive to one agent.
    Users,
}

/// Each conversation's Blender, once its first call chose (an own one is
/// in `OWN`, and goes first).
static TARGET: LazyLock<Mutex<HashMap<String, Target>>> = LazyLock::new(Mutex::default);

/// Each conversation's own Blender's port.
static OWN: LazyLock<Mutex<HashMap<String, u16>>> = LazyLock::new(Mutex::default);

/// Startup is serialized per agent, not across agents.
static GATES: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);
/// A deleted conversation cannot publish an instance after a pending launch.
/// No automatic startup after an explicit close; use_own/restart reopens.
static CLOSED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
static DELETED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn gate(conv: &str) -> Arc<Mutex<()>> {
    GATES.lock().unwrap().entry(conv.into()).or_default().clone()
}

fn check_caller(c: &Caller) -> Result<(), String> {
    if c.cancelled.load(Ordering::SeqCst) || DELETED.lock().unwrap().contains(&c.conv) {
        Err("Blender startup was canceled; no instance was attached.".into())
    } else {
        Ok(())
    }
}

/// Whether this is an Oak agent call (not a standalone MCP client).
pub fn has_caller() -> bool {
    caller().is_some()
}

/// The port this call's Blender listens on: the caller's own, else the
/// one its conversation works in; the user's without a conversation.
pub fn port() -> u16 {
    let Some(c) = caller() else { return USER_PORT };
    port_for(&c.conv)
}

fn port_for(conv: &str) -> u16 {
    if users_for(conv) {
        USER_PORT
    } else {
        port_of_conv(conv).unwrap_or(0)
    }
}

/// Conversation `conv`'s own Blender's port, if it has one.
pub fn port_of_conv(conv: &str) -> Option<u16> {
    OWN.lock().unwrap().get(conv).copied()
}

#[cfg(windows)]
struct Instance {
    port: u16,
    owner: String,
    program: PathBuf,
    file: Option<PathBuf>,
    started: crate::window_tool::Started,
}

#[cfg(windows)]
static RUNNING: LazyLock<Mutex<Vec<Instance>>> = LazyLock::new(Mutex::default);

/// The window of the Blender Oak runs on `port`, if it does.
pub fn window_of(port: u16) -> Option<usize> {
    #[cfg(windows)]
    {
        RUNNING.lock().unwrap().iter().find(|i| i.port == port).map(|i| i.started.window)
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        None
    }
}

/// Whether the user explicitly attached their Blender to this agent.
pub fn users_for(conv: &str) -> bool {
    TARGET.lock().unwrap().get(conv) == Some(&Target::Users)
}

pub fn running_count() -> usize {
    OWN.lock().unwrap().len()
}

/// The agent's own live window, independently of WINDOW's current window.
pub fn window_for(conv: &str) -> Option<usize> {
    if users_for(conv) { return None }
    port_of_conv(conv).and_then(window_of)
}

/// Hidden Blender windows belong to exactly one agent.
pub fn can_use(w: usize, conv: &str) -> bool {
    #[cfg(windows)]
    return RUNNING.lock().unwrap().iter().find(|i| i.started.window == w || i.started.pid == crate::winctl::process_of(w)).is_none_or(|i| i.owner == conv);
    #[cfg(not(windows))]
    { let _ = (w, conv); true }
}

/// The port of the Blender Oak runs whose window is `w`.
pub fn port_of_window(w: usize) -> Option<u16> {
    #[cfg(windows)]
    {
        RUNNING.lock().unwrap().iter().find(|i| i.started.window == w).map(|i| i.port)
    }
    #[cfg(not(windows))]
    {
        let _ = w;
        None
    }
}

/// What a Blender Oak runs is, for WINDOW's list (as conversation `conv`
/// sees it).
pub fn describe(w: usize, conv: &str) -> Option<&'static str> {
    #[cfg(windows)]
    {
        let r = RUNNING.lock().unwrap();
        let i = r.iter().find(|i| i.started.window == w)?;
        Some(if i.owner == conv { "your private Blender" } else { "another agent's Blender" })
    }
    #[cfg(not(windows))]
    {
        let _ = (w, conv);
        None
    }
}

/// What the model is told when Blender can't be found.
const NOT_INSTALLED: &str = "Blender isn't installed (Oak looked in Program Files, Steam and the PATH). Ask the user to install it (blender.org, or `winget install BlenderFoundation.Blender`); Oak then starts it by itself.";

/// Starts the agent's private instance on LOAD, and for old loaded sessions
/// on their first call. A dead instance never redirects to another Blender.
pub fn ensure() -> Result<Option<String>, String> {
    let Some(c) = caller() else { return Ok(None) };
    check_caller(&c)?;
    if CLOSED.lock().unwrap().contains(&c.conv) {
        return Err("Your private Blender is closed. use_own or restart starts it again.".into());
    }
    if users_for(&c.conv) {
        return if super::blender::reachable_on(USER_PORT) { Ok(None) } else {
            Err("The user's attached Blender stopped answering. Your private instance is unchanged; use_own returns to it. Nothing was redirected.".into())
        };
    }
    if let Some(port) = port_of_conv(&c.conv) {
        return if window_of(port).is_some_and(crate_window_alive) && super::blender::reachable_on(port) {
            Ok(None)
        } else {
            Err("Your private Blender stopped answering (closed or busy). Calls were not redirected. Use restart to start another, or use_own with a portable program.".into())
        };
    }
    use_own(None, None).map(Some)
}

fn crate_window_alive(w: usize) -> bool {
    #[cfg(windows)] { crate::winctl::alive(w) }
    #[cfg(not(windows))] { let _ = w; false }
}

/// Resolves an explicitly selected portable Blender, without falling back
/// to an installed one when the requested path is wrong.
fn own_program(folder: &Path, program: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(program) = program else { return Ok(None) };
    let p = Path::new(program);
    let p = if p.is_relative() { folder.join(p) } else { p.to_path_buf() };
    if !p.is_file() {
        return Err(format!("No Blender executable at {}. Give `program` as the path to blender.exe in the extracted portable installation; Oak did not fall back to another installation.", p.display()));
    }
    Ok(Some(p))
}

/// Starts this call's conversation's own Blender (opening `file`, relative
/// to its folder), optionally using portable executable `program`, and
/// points its calls at it. Both kinds go through the same hidden launcher.
pub fn use_own(file: Option<&str>, program: Option<&str>) -> Result<String, String> {
    start_own(file, program, false, false)
}

pub fn restart(file: Option<&str>, program: Option<&str>, discard: bool) -> Result<String, String> {
    start_own(file, program, true, discard)
}

fn start_own(file: Option<&str>, program: Option<&str>, restart: bool, discard: bool) -> Result<String, String> {
    let c = caller().ok_or("Only an Oak agent can start a private Blender.")?;
    let program = own_program(&c.folder, program)?;
    let file = file.map(|f| { let p = Path::new(f); if p.is_relative() { c.folder.join(p) } else { p.to_path_buf() } });
    if let Some(f) = &file && !f.is_file() { return Err(format!("No file {}", f.display())); }
    let gate = gate(&c.conv);
    let _one = gate.lock().unwrap();
    check_caller(&c)?;
    #[cfg(windows)]
    {
        let old = port_of_conv(&c.conv);
        let previous = old.and_then(|p| RUNNING.lock().unwrap().iter().find(|i| i.port == p).map(|i| (i.program.clone(), i.file.clone())));
        let changed = previous.as_ref().is_some_and(|(p, f)| program.as_ref().is_some_and(|q| q != p) || file.as_ref().is_some_and(|q| Some(q) != f.as_ref()));
        if let Some(port) = old {
            if !restart && !changed && window_of(port).is_some_and(crate_window_alive) {
                if !super::blender::reachable_on(port) { return Err("Your private Blender is busy or stopped answering. It was not replaced.".into()); }
                TARGET.lock().unwrap().remove(&c.conv);
                CLOSED.lock().unwrap().remove(&c.conv);
                return Ok(format!("Your private Blender is ready (port {port}); calls and the dedicated live view use only this instance."));
            }
            if window_of(port).is_some_and(crate_window_alive) && !discard { check_saved(port)?; }
        }
        // Resolve the executable before closing any existing scene.
        let program = program.or_else(|| previous.map(|(p, _)| p)).or_else(|| super::blender::find_blender().into_iter().next()).ok_or(NOT_INSTALLED)?;
        check_caller(&c)?;
        end_own(&c.conv);
        let reserved = free_port().ok_or("No free port for another Blender.")?;
        let port = reserved.0;
        let settings = c.scratch.join("blender-user");
        let w = launch(&c.conv, port, settings.clone(), file.clone(), program.clone(), &c)?;
        if let Err(e) = check_caller(&c) { forget(port); return Err(e); }
        OWN.lock().unwrap().insert(c.conv.clone(), port);
        if let Err(e) = check_caller(&c) { end_own(&c.conv); return Err(e); }
        TARGET.lock().unwrap().remove(&c.conv);
        CLOSED.lock().unwrap().remove(&c.conv);
        crate::window_tool::set_current(&c.conv, w);
        Ok(format!("Your private Blender started on Oak's hidden desktop using {} (port {port}). Its settings are isolated in {}. Only this agent controls it; the dedicated Blender preview stays with it even when WINDOW selects another app. Save work that should last: it ends on close, conversation deletion or Oak exit.", program.display(), settings.display()))
    }
    #[cfg(not(windows))]
    {
        let _ = (file, program, restart, discard);
        Err("Oak's private hidden Blender instances currently require Windows.".into())
    }
}

/// Replacing or closing a scene must not silently discard unsaved work.
/// Python edits that bypass Blender's own dirty flag count too (the add-on's
/// `session_state`); a busy or silent Blender is refused, never discarded.
#[cfg(windows)]
fn check_saved(port: u16) -> Result<(), String> {
    let r = super::blender::send_to(port, "session_state", json!({}), Duration::from_secs(5))
        .map_err(|e| format!("Couldn't confirm your Blender's work is saved ({e}); it was left running. Pass discard=true to replace or close it anyway."))?;
    if r["dirty"].as_bool() != Some(false) {
        return Err("Your Blender has unsaved changes. Save with bpy.ops.wm.save_as_mainfile first, or explicitly pass discard=true to restart/close. It was left running.".into());
    }
    Ok(())
}

pub fn close(discard: bool) -> Result<String, String> {
    let c = caller().ok_or("Only an Oak agent can close its private Blender.")?;
    close_for(&c.conv, discard)
}

pub fn close_for(conv: &str, discard: bool) -> Result<String, String> {
    let gate = gate(conv);
    let _one = gate.lock().unwrap();
    #[cfg(windows)]
    if let Some(port) = port_of_conv(conv) && window_of(port).is_some_and(crate_window_alive) && !discard { check_saved(port)?; }
    let note = end_own(conv);
    TARGET.lock().unwrap().remove(conv);
    CLOSED.lock().unwrap().insert(conv.into());
    Ok(if note.is_empty() { "Your private Blender is already closed.".into() } else { note.into() })
}

/// Ends the caller's own Blender, if it has one: what to tell the model.
fn end_own(conv: &str) -> &'static str {
    match OWN.lock().unwrap().remove(conv) {
        Some(_port) => {
            #[cfg(windows)]
            forget(_port);
            "Closed your own Blender (what wasn't saved in it is gone). "
        }
        None => "",
    }
}

/// Explicitly attaches the user's add-on to one agent, keeping its private
/// scene intact. It never becomes the default for any other agent.
pub fn use_users() -> Result<String, String> {
    let c = caller().ok_or("Only a conversation can attach Blender.")?;
    let gate = gate(&c.conv);
    let _one = gate.lock().unwrap();
    check_caller(&c)?;
    if !super::blender::reachable_on(USER_PORT) {
        return Err(format!("The user's Blender doesn't answer on {USER_PORT}; it must be open with Oak's add-on. Your private instance is unchanged."));
    }
    let mut targets = TARGET.lock().unwrap();
    if targets.iter().any(|(id, _)| id != &c.conv) {
        return Err("The user's Blender is already attached to another agent. Use your private instance instead.".into());
    }
    targets.insert(c.conv.clone(), Target::Users);
    CLOSED.lock().unwrap().remove(&c.conv);
    Ok("Explicitly attached to the user's Blender on their screen. Your private scene is still open and unchanged; use_own returns to it. No other agent can attach while you are using it. Save in the user's scene only when asked.".into())
}

/// Ends conversation `conv`'s own Blender and forgets its choice (it was
/// deleted).
pub fn end(conv: &str) {
    DELETED.lock().unwrap().insert(conv.into());
    end_own(conv);
    TARGET.lock().unwrap().remove(conv);
    GATES.lock().unwrap().remove(conv);
}

pub fn shutdown() {
    let ids: Vec<String> = GATES.lock().unwrap().keys().cloned().collect();
    for id in ids { end(&id); }
}

/// Ends and forgets the Blender Oak runs on `port`, if any.
#[cfg(windows)]
fn forget(port: u16) {
    let gone: Vec<Instance> = {
        let mut r = RUNNING.lock().unwrap();
        let (gone, keep) = std::mem::take(&mut *r).into_iter().partition(|i| i.port == port);
        *r = keep;
        gone
    };
    RETIRED.lock().unwrap().insert(port);
    SENDERS.lock().unwrap().remove(&port);
    POINTER.lock().unwrap().remove(&port);
    WHEEL.lock().unwrap().remove(&port);
    // Dropping the kill-on-close job ends this process tree, outside RUNNING.
    drop(gone);
}

#[cfg(windows)]
static RETIRED: LazyLock<Mutex<HashSet<u16>>> = LazyLock::new(Mutex::default);
#[cfg(windows)]
static RESERVED: LazyLock<Mutex<HashSet<u16>>> = LazyLock::new(Mutex::default);

#[cfg(windows)]
struct ReservedPort(u16);
#[cfg(windows)]
impl Drop for ReservedPort {
    fn drop(&mut self) { RESERVED.lock().unwrap().remove(&self.0); }
}

/// Atomically reserve across concurrent launches; the guard lasts until
/// publication or failure, not just until the socket probe completes.
#[cfg(windows)]
fn free_port() -> Option<ReservedPort> {
    let mut reserved = RESERVED.lock().unwrap();
    let used: Vec<u16> = RUNNING.lock().unwrap().iter().map(|i| i.port).collect();
    let retired = RETIRED.lock().unwrap();
    let port = (USER_PORT + 1..USER_PORT + 10_000).find(|p| !retired.contains(p) && !reserved.contains(p) && !used.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())?;
    reserved.insert(port);
    Some(ReservedPort(port))
}

/// What starts Oak's add-on in a Blender Oak runs.
const START: &str = r#"# Oak's start script for a Blender it runs on its hidden desktop (Oak's
# src/mcp/blender_run.rs): loads Oak's add-on from this folder, starts its
# server on OAK_MCP_PORT, and closes the splash screen.
import importlib.util, os, sys, bpy
_dir = os.environ["OAK_MCP_DIR"]
_spec = importlib.util.spec_from_file_location("oak_mcp_run", os.path.join(_dir, "oak_mcp.py"))
_mod = importlib.util.module_from_spec(_spec)
sys.modules["oak_mcp_run"] = _mod
_spec.loader.exec_module(_mod)
_err = _mod.start_on(int(os.environ["OAK_MCP_PORT"]))
if _err:
    print("Oak MCP:", _err)

def _close_splash():
    try:
        win = bpy.context.window_manager.windows[0]
        win.event_simulate('MOUSEMOVE', 'NOTHING', x=win.width // 2, y=win.height // 2)
        win.event_simulate('ESC', 'PRESS')
        win.event_simulate('ESC', 'RELEASE')
    except Exception as e:
        print("Oak MCP: splash:", e)
    return None

bpy.app.timers.register(_close_splash, first_interval=1.0)

# A copy of the add-on installed in the user's Blender settings, from before
# it knew OAK_MCP_RUN, starts its own server on the user's port: this
# Blender would pass for the user's. Stop it, now and as it tries.
_tries = [0]

def _quiet_installed():
    other = sys.modules.get("oak_mcp")
    if other is not None and other is not _mod:
        try:
            if bpy.app.timers.is_registered(other._auto_start):
                bpy.app.timers.unregister(other._auto_start)
        except Exception:
            pass
        try:
            other.stop()
        except Exception as e:
            print("Oak MCP: installed copy:", e)
    _tries[0] += 1
    return 1.0 if _tries[0] < 5 else None

_quiet_installed()
bpy.app.timers.register(_quiet_installed, first_interval=0.3)
"#;

/// Starts only this agent's Blender through the hidden desktop launcher.
#[cfg(windows)]
fn launch(owner: &str, port: u16, settings: PathBuf, file: Option<PathBuf>, exe: PathBuf, c: &Caller) -> Result<usize, String> {
    let dir = c.scratch.join("blender-runtime");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
    for (name, text) in [("oak_mcp.py", super::blender::ADDON), ("oak_start.py", START)] {
        let p = dir.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(text) {
            std::fs::write(&p, text).map_err(|e| format!("Couldn't write {}: {e}", p.display()))?;
        }
    }
    let mut args = vec!["--enable-event-simulate".to_string()];
    if let Some(f) = &file {
        args.push(f.to_string_lossy().into_owned());
    }
    args.extend(["--python".to_string(), dir.join("oak_start.py").to_string_lossy().into_owned()]);
    let mut env = vec![("OAK_MCP_RUN", "1".to_string()), ("OAK_MCP_PORT", port.to_string()), ("OAK_MCP_DIR", dir.to_string_lossy().into_owned())];
    std::fs::create_dir_all(&settings).map_err(|e| format!("Couldn't create private Blender settings: {e}"))?;
    env.push(("BLENDER_USER_RESOURCES", settings.to_string_lossy().into_owned()));
    let folder = if c.folder.is_dir() { c.folder.clone() } else { dir.clone() };
    let logs = &c.scratch;
    let _ = std::fs::create_dir_all(logs);
    let log = logs.join(format!("blender-{port}.log"));
        let oak = if cfg!(test) { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/oak.exe") } else { std::env::current_exe().map_err(|e| format!("Couldn't find Oak's program: {e}"))? };
    let started = crate::window_tool::start(&oak, "Blender", &exe.to_string_lossy(), &args, &env, &folder, &log, (1600, 1000))?;
    let window = started.window;
    let mut started = Some(started);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(90) {
        check_caller(c)?;
        if super::blender::reachable_on(port) {
            RUNNING.lock().unwrap().push(Instance { port, owner: owner.into(), program: exe.clone(), file, started: started.take().expect("once") });
            return Ok(window);
        }
        if let Some(s) = started.as_mut()
            && matches!(s.child.try_wait(), Ok(Some(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if let Some(s) = started {
        crate::window_tool::kill_tree(s.child.id());
    }
    let tail: String = std::fs::read_to_string(&log).unwrap_or_default().lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    Err(format!("Blender ({}) started but its add-on didn't answer on port {port} within 90 seconds, so it was ended. Its output:\n{tail}", exe.display()))
}

/// A picture of the window of the Blender Oak runs on `port`, as Blender
/// draws it (None: it didn't answer within a few seconds).
pub fn window_frame(port: u16) -> Option<image::RgbaImage> {
    let path = std::env::temp_dir().join(format!("oak-blender-window-{port}-{}.png", std::process::id()));
    super::blender::send_to(port, "window_frame", json!({"path": path.to_string_lossy()}), std::time::Duration::from_secs(5)).ok()?;
    let img = image::open(&path).ok()?.to_rgba8();
    let _ = std::fs::remove_file(&path);
    Some(img)
}

/// Modifier keys held with an event.
#[derive(Clone, Copy, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Blender's event type for a character typed, and whether it needs Shift
/// (a US keyboard's); one without a key of its own goes as F24, carrying
/// the character.
fn char_type(c: char) -> (String, bool) {
    const SHIFTED: &[(char, &str)] = &[
        ('!', "ONE"), ('@', "TWO"), ('#', "THREE"), ('$', "FOUR"), ('%', "FIVE"), ('^', "SIX"), ('&', "SEVEN"), ('*', "EIGHT"), ('(', "NINE"), (')', "ZERO"),
        ('_', "MINUS"), ('+', "EQUAL"), (':', "SEMI_COLON"), ('"', "QUOTE"), ('<', "COMMA"), ('>', "PERIOD"), ('?', "SLASH"), ('{', "LEFT_BRACKET"),
        ('}', "RIGHT_BRACKET"), ('|', "BACK_SLASH"), ('~', "ACCENT_GRAVE"),
    ];
    const PLAIN: &[(char, &str)] = &[
        (' ', "SPACE"), ('-', "MINUS"), ('=', "EQUAL"), (';', "SEMI_COLON"), ('\'', "QUOTE"), (',', "COMMA"), ('.', "PERIOD"), ('/', "SLASH"), ('[', "LEFT_BRACKET"),
        (']', "RIGHT_BRACKET"), ('\\', "BACK_SLASH"), ('`', "ACCENT_GRAVE"), ('\n', "RET"), ('\r', "RET"), ('\t', "TAB"),
    ];
    const DIGITS: [&str; 10] = ["ZERO", "ONE", "TWO", "THREE", "FOUR", "FIVE", "SIX", "SEVEN", "EIGHT", "NINE"];
    if c.is_ascii_lowercase() {
        return (c.to_ascii_uppercase().to_string(), false);
    }
    if c.is_ascii_uppercase() {
        return (c.to_string(), true);
    }
    if let Some(d) = c.to_digit(10).filter(|_| c.is_ascii_digit()) {
        return (DIGITS[d as usize].into(), false);
    }
    if let Some((_, t)) = SHIFTED.iter().find(|(k, _)| *k == c) {
        return ((*t).into(), true);
    }
    if let Some((_, t)) = PLAIN.iter().find(|(k, _)| *k == c) {
        return ((*t).into(), false);
    }
    ("F24".into(), false)
}

#[cfg_attr(not(windows), allow(dead_code))]
/// A key as `winctl`'s names say it (enter, ctrl+s) as Blender events.
fn key_events(spec: &str, held: Mods) -> Result<Vec<Value>, String> {
    let lower = spec.trim().to_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    let name = parts.pop().unwrap_or("");
    let mut m = held;
    for p in parts {
        match p {
            "ctrl" | "control" => m.ctrl = true,
            "shift" => m.shift = true,
            "alt" => m.alt = true,
            other => return Err(format!("Unknown modifier {other}")),
        }
    }
    let ty = match name {
        "enter" | "return" => "RET".to_string(),
        "tab" => "TAB".into(),
        "escape" | "esc" => "ESC".into(),
        "backspace" => "BACK_SPACE".into(),
        "delete" | "del" => "DEL".into(),
        "insert" => "INSERT".into(),
        "home" => "HOME".into(),
        "end" => "END".into(),
        "pageup" => "PAGE_UP".into(),
        "pagedown" => "PAGE_DOWN".into(),
        "left" => "LEFT_ARROW".into(),
        "right" => "RIGHT_ARROW".into(),
        "up" => "UP_ARROW".into(),
        "down" => "DOWN_ARROW".into(),
        "space" => "SPACE".into(),
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) => f.to_uppercase(),
        n if n.chars().count() == 1 => {
            let (t, shift) = char_type(n.chars().next().unwrap_or(' '));
            m.shift |= shift;
            t
        }
        other => return Err(format!("Unknown key {other}")),
    };
    let ev = |value: &str| json!({"type": ty, "value": value, "shift": m.shift, "ctrl": m.ctrl, "alt": m.alt});
    Ok(vec![ev("PRESS"), ev("RELEASE")])
}

#[cfg_attr(not(windows), allow(dead_code))]
fn char_events(c: char, held: Mods) -> Vec<Value> {
    let (ty, shift) = char_type(c);
    let shift = shift || held.shift;
    vec![
        json!({"type": ty, "value": "PRESS", "shift": shift, "unicode": c.to_string()}),
        json!({"type": ty, "value": "RELEASE", "shift": shift}),
    ]
}

#[cfg_attr(not(windows), allow(dead_code))]
fn mouse(ty: &str, value: &str, x: i32, y: i32, m: Mods) -> Value {
    json!({"type": ty, "value": value, "x": x, "y": y, "shift": m.shift, "ctrl": m.ctrl, "alt": m.alt})
}

/// Where the pointer last was in each port's Blender: keys go there
/// (Blender sends a key to the editor under the pointer, and a simulated
/// event without a place would be at its corner).
static POINTER: LazyLock<Mutex<HashMap<u16, (i64, i64)>>> = LazyLock::new(Mutex::default);

/// Gives events without a place the pointer's, and remembers it.
#[cfg_attr(not(windows), allow(dead_code))]
fn place(port: u16, events: &mut [Value]) {
    let mut p = POINTER.lock().unwrap();
    for e in events {
        match (e["x"].as_i64(), e["y"].as_i64()) {
            (Some(x), Some(y)) => {
                p.insert(port, (x, y));
            }
            _ => {
                if let Some(&(x, y)) = p.get(&port) {
                    e["x"] = json!(x);
                    e["y"] = json!(y);
                }
            }
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
const BUTTONS: [&str; 3] = ["LEFTMOUSE", "RIGHTMOUSE", "MIDDLEMOUSE"];

#[cfg_attr(not(windows), allow(dead_code))]
/// The wheel's part of a line not sent yet, per port (a touchpad's small
/// steps add up).
static WHEEL: LazyLock<Mutex<HashMap<u16, i32>>> = LazyLock::new(Mutex::default);

#[cfg_attr(not(windows), allow(dead_code))]
fn wheel_events(port: u16, x: i32, y: i32, delta: i32, m: Mods) -> Vec<Value> {
    let mut w = WHEEL.lock().unwrap();
    let total = w.get(&port).copied().unwrap_or(0) + delta;
    let steps = total / 120;
    w.insert(port, total - steps * 120);
    let ty = if steps > 0 { "WHEELUPMOUSE" } else { "WHEELDOWNMOUSE" };
    (0..steps.unsigned_abs().min(20)).map(|_| mouse(ty, "PRESS", x, y, m)).collect()
}

/// The user's input from the live view, as `winctl::Live` says it, to the
/// Blender Oak runs on `port` (in order, without waiting).
#[cfg(windows)]
pub fn live(port: u16, l: &crate::winctl::Live, m: Mods) {
    use crate::winctl::Live;
    let events = match l {
        Live::Move { x, y, .. } => vec![mouse("MOUSEMOVE", "NOTHING", *x, *y, m)],
        Live::Button { x, y, button, down, .. } => vec![mouse(BUTTONS[(*button as usize).min(2)], if *down { "PRESS" } else { "RELEASE" }, *x, *y, m)],
        Live::Wheel { x, y, delta } => wheel_events(port, *x, *y, *delta, m),
        Live::Char(u) => char::from_u32(*u as u32).map(|c| char_events(c, Mods { shift: false, ..m })).unwrap_or_default(),
        Live::Key(spec) => key_events(spec, Mods::default()).unwrap_or_default(),
    };
    let mut events = events;
    place(port, &mut events);
    if !events.is_empty() {
        post(port, events);
    }
}

/// Sends events to `port`'s Blender from one thread per port, those that
/// came meanwhile together.
#[cfg(windows)]
static SENDERS: LazyLock<Mutex<HashMap<u16, std::sync::mpsc::Sender<Vec<Value>>>>> = LazyLock::new(Mutex::default);

#[cfg(windows)]
fn identity(port: u16) -> Option<(String, u32)> {
    RUNNING.lock().unwrap().iter().find(|i| i.port == port).map(|i| (i.owner.clone(), i.started.pid))
}

#[cfg(windows)]
fn post(port: u16, events: Vec<Value>) {
    use std::sync::mpsc::channel;
    let generation = identity(port);
    if generation.is_none() { return }
    let mut s = SENDERS.lock().unwrap();
    let tx = s.entry(port).or_insert_with(|| {
        let (tx, rx) = channel::<Vec<Value>>();
        let _ = std::thread::Builder::new().name("blender-input".into()).spawn(move || {
            while let Ok(mut batch) = rx.recv() {
                if identity(port) != generation { break; }
                while let Ok(more) = rx.try_recv() {
                    batch.extend(more);
                }
                let _ = super::blender::send_to(port, "simulate", json!({"events": batch}), std::time::Duration::from_secs(5));
            }
        });
        tx
    });
    let _ = tx.send(events);
}

/// WINDOW's input to the Blender Oak runs on `port` (waits until Blender
/// took it).
#[cfg(windows)]
pub fn input(port: u16, i: &crate::winctl::Input) -> Result<(), String> {
    use crate::winctl::Input;
    let m = Mods::default();
    let button = |right: bool| BUTTONS[usize::from(right)];
    let events: Vec<Value> = match i {
        Input::Click { x, y, right, double } => {
            let mut v = vec![mouse("MOUSEMOVE", "NOTHING", *x, *y, m)];
            for _ in 0..if *double { 2 } else { 1 } {
                v.push(mouse(button(*right), "PRESS", *x, *y, m));
                v.push(mouse(button(*right), "RELEASE", *x, *y, m));
            }
            v
        }
        Input::Move { x, y } => vec![mouse("MOUSEMOVE", "NOTHING", *x, *y, m)],
        Input::Drag { from, to, right, .. } => {
            let mut v = vec![mouse("MOUSEMOVE", "NOTHING", from.0, from.1, m), mouse(button(*right), "PRESS", from.0, from.1, m)];
            for k in 1..=12 {
                v.push(mouse("MOUSEMOVE", "NOTHING", from.0 + (to.0 - from.0) * k / 12, from.1 + (to.1 - from.1) * k / 12, m));
            }
            v.push(mouse(button(*right), "RELEASE", to.0, to.1, m));
            v
        }
        Input::Scroll { x, y, lines } => {
            let mut v = vec![mouse("MOUSEMOVE", "NOTHING", *x, *y, m)];
            v.extend(wheel_events(port, *x, *y, lines * 120, m));
            v
        }
        Input::Type(text) => text.chars().flat_map(|c| char_events(c, m)).collect(),
        Input::Key(spec) => key_events(spec, m)?,
        Input::Resize { .. } | Input::Close => return Err("Not through Blender's events".into()),
    };
    let mut events = events;
    place(port, &mut events);
    super::blender::send_to(port, "simulate", json!({"events": events}), std::time::Duration::from_secs(30)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_program_paths_are_explicit_and_do_not_fall_back() {
        let exe = std::env::current_exe().unwrap();
        let folder = exe.parent().unwrap();
        assert_eq!(own_program(folder, None).unwrap(), None);
        assert_eq!(own_program(folder, Some(exe.to_str().unwrap())).unwrap(), Some(exe.clone()));
        assert_eq!(own_program(folder, Some(exe.file_name().unwrap().to_str().unwrap())).unwrap(), Some(exe.clone()));
        assert!(own_program(folder, Some("missing-portable-blender/blender.exe")).unwrap_err().contains("did not fall back"));
        assert!(own_program(folder, Some(".")).is_err(), "a directory is not an executable");
    }

    #[test]
    fn keys_and_characters_become_blender_events() {
        assert_eq!(char_type('a'), ("A".into(), false));
        assert_eq!(char_type('A'), ("A".into(), true));
        assert_eq!(char_type('7'), ("SEVEN".into(), false));
        assert_eq!(char_type('?'), ("SLASH".into(), true));
        assert_eq!(char_type('é'), ("F24".into(), false));
        let k = key_events("ctrl+shift+z", Mods::default()).unwrap();
        assert_eq!((k[0]["type"].as_str(), k[0]["ctrl"].as_bool(), k[0]["shift"].as_bool(), k[1]["value"].as_str()), (Some("Z"), Some(true), Some(true), Some("RELEASE")));
        assert_eq!(key_events("enter", Mods::default()).unwrap()[0]["type"], "RET");
        assert_eq!(key_events("f12", Mods::default()).unwrap()[0]["type"], "F12");
        assert!(key_events("hyper+a", Mods::default()).is_err());
        let c = char_events('!', Mods::default());
        assert_eq!((c[0]["type"].as_str(), c[0]["shift"].as_bool(), c[0]["unicode"].as_str()), (Some("ONE"), Some(true), Some("!")));
        // A touchpad's small steps add up to a line.
        assert!(wheel_events(1, 0, 0, 60, Mods::default()).is_empty());
        assert_eq!(wheel_events(1, 0, 0, 60, Mods::default()).len(), 1);
        assert_eq!(wheel_events(1, 0, 0, -240, Mods::default())[0]["type"], "WHEELDOWNMOUSE");
        // Keys go where the pointer last was.
        let mut ev = vec![mouse("MOUSEMOVE", "NOTHING", 5, 6, Mods::default()), char_events('n', Mods::default()).remove(0)];
        place(2, &mut ev);
        assert_eq!((ev[1]["x"].as_i64(), ev[1]["y"].as_i64()), (Some(5), Some(6)));
    }

    #[test]
    fn caller_context_restores_after_nested_calls_and_panics() {
        let outer = Caller { conv: "context-outer".into(), ..Default::default() };
        with_caller(outer.clone(), || {
            let r = std::panic::catch_unwind(|| with_caller(Caller { conv: "context-inner".into(), ..Default::default() }, || panic!("test unwind")));
            assert!(r.is_err());
            assert_eq!(caller().unwrap().conv, outer.conv);
        });
        assert!(!has_caller());
        let c = Caller { conv: "cancel-before-start".into(), ..Default::default() };
        c.cancelled.store(true, Ordering::SeqCst);
        assert!(with_caller(c, ensure).unwrap_err().contains("canceled"));
    }

    #[cfg(windows)]
    #[test]
    fn concurrent_launches_reserve_distinct_ports() {
        let handles: Vec<_> = (0..12).map(|_| std::thread::spawn(|| free_port().unwrap())).collect();
        let guards: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let ports: HashSet<_> = guards.iter().map(|g| g.0).collect();
        assert_eq!(ports.len(), guards.len());
        assert!(!ports.contains(&USER_PORT));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires cargo build, Blender and a Windows desktop"]
    fn private_instances_are_isolated_and_clean_up() {
        let scratch = PathBuf::from(std::env::var_os("OAK_BLENDER_TEST_DIR").expect("set OAK_BLENDER_TEST_DIR for fixtures"));
        let folder = std::env::current_dir().unwrap();
        let a = Caller { conv: "private-live-a".into(), folder: folder.clone(), scratch: scratch.join("a"), ..Default::default() };
        let b = Caller { conv: "private-live-b".into(), folder: folder.clone(), scratch: scratch.join("b"), ..Default::default() };
        struct Cleanup(Vec<String>);
        impl Drop for Cleanup { fn drop(&mut self) { for id in &self.0 { end(id); } } }
        let _cleanup = Cleanup(vec![a.conv.clone(), b.conv.clone()]);
        let handles: Vec<_> = [a.clone(), b.clone()].into_iter().map(|c| std::thread::spawn(move || with_caller(c, ensure))).collect();
        for h in handles { println!("{}", h.join().unwrap().unwrap().unwrap()); }
        let (pa, pb) = (port_of_conv(&a.conv).unwrap(), port_of_conv(&b.conv).unwrap());
        let (wa, wb) = (window_for(&a.conv).unwrap(), window_for(&b.conv).unwrap());
        assert_ne!(pa, pb); assert_ne!(wa, wb);
        assert!(can_use(wa, &a.conv)); assert!(!can_use(wa, &b.conv));
        assert!(crate::window_tool::run(&b.conv, &folder, &b.scratch, "WINDOW_SHOT", &json!({"window": format!("hidden:{wa:#x}")})).unwrap_err().contains("another agent"));
        let list = crate::window_tool::run(&b.conv, &folder, &b.scratch, "WINDOW_LIST", &json!({})).unwrap();
        assert!(!list[0]["text"].as_str().unwrap().contains(&format!("hidden:{wa:#x}")));
        for (c, name) in [(&a, "ONLY_AGENT_A"), (&b, "ONLY_AGENT_B")] {
            let code = format!("import bpy, ctypes, os\nbpy.context.scene.name = '{name}'\nprint(os.environ['BLENDER_USER_RESOURCES'])\nu = ctypes.WinDLL('user32')\nu.GetThreadDesktop.restype = ctypes.c_void_p\nu.GetUserObjectInformationW.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p]\nbuf = ctypes.create_unicode_buffer(256)\nn = ctypes.c_ulong()\nassert u.GetUserObjectInformationW(u.GetThreadDesktop(ctypes.windll.kernel32.GetCurrentThreadId()), 2, buf, ctypes.sizeof(buf), ctypes.byref(n))\nassert buf.value == 'oak-apps', buf.value");
            let out = with_caller(c.clone(), || super::super::blender::execute(&code)).unwrap();
            assert!(out.contains(c.scratch.join("blender-user").to_str().unwrap()));
        }
        for (c, own, other) in [(&a, "ONLY_AGENT_A", "ONLY_AGENT_B"), (&b, "ONLY_AGENT_B", "ONLY_AGENT_A")] {
            let out = with_caller(c.clone(), || super::super::blender::execute("print(bpy.context.scene.name)")).unwrap();
            assert!(out.contains(own)); assert!(!out.contains(other));
        }
        // WINDOW may select another app without changing the Blender preview.
        crate::window_tool::set_current(&a.conv, wb);
        assert_eq!(window_for(&a.conv), Some(wa));
        assert!(window_frame(pa).is_some()); assert!(window_frame(pb).is_some());
        assert!(with_caller(a.clone(), || restart(None, None, false)).unwrap_err().contains("unsaved"));
        assert!(with_caller(a.clone(), || use_own(None, Some("missing-portable/blender.exe"))).is_err());
        assert_eq!(window_for(&a.conv), Some(wa));
        with_caller(a.clone(), || close(true)).unwrap();
        assert!(with_caller(a.clone(), ensure).unwrap_err().contains("closed"));
        let began = Instant::now();
        while crate::winctl::alive(wa) && began.elapsed() < Duration::from_secs(8) { std::thread::sleep(Duration::from_millis(50)); }
        assert!(!crate::winctl::alive(wa));
        assert!(crate::winctl::alive(wb));
        let out = with_caller(b.clone(), || super::super::blender::execute("print(bpy.context.scene.name)")).unwrap();
        assert!(out.contains("ONLY_AGENT_B"));
        end(&b.conv);
        let began = Instant::now();
        while crate::winctl::alive(wb) && began.elapsed() < Duration::from_secs(8) { std::thread::sleep(Duration::from_millis(50)); }
        assert!(!crate::winctl::alive(wb));
        assert!(port_of_conv(&b.conv).is_none());
    }

    #[test]
    fn calls_go_to_their_conversations_blender() {
        assert_eq!(port(), USER_PORT);
        let a = Caller { conv: "own-test-a".into(), ..Default::default() };
        let b = Caller { conv: "own-test-b".into(), ..Default::default() };
        assert_eq!(with_caller(a.clone(), port), 0, "never pick the user's Blender by default");
        OWN.lock().unwrap().insert(a.conv.clone(), 9998);
        OWN.lock().unwrap().insert(b.conv.clone(), 9999);
        assert_eq!(with_caller(a.clone(), port), 9998);
        assert_eq!(with_caller(b.clone(), port), 9999);
        TARGET.lock().unwrap().insert(a.conv.clone(), Target::Users);
        assert_eq!(with_caller(a.clone(), port), USER_PORT);
        assert_eq!(with_caller(b.clone(), port), 9999);
        TARGET.lock().unwrap().remove(&a.conv);
        assert_eq!(with_caller(a.clone(), port), 9998, "attachment preserved the private scene");
        assert_eq!(port(), USER_PORT);
        end(&a.conv);
        end(&b.conv);
        assert_eq!(port_of_conv(&a.conv), None);
        assert!(with_caller(a, ensure).is_err(), "deletion cannot resurrect a pending launch");
    }
}
