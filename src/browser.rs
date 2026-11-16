//! The browser: a real Chromium per conversation that the model opens and
//! closes (BROWSER action=open and close) and the user watches and uses.
//!
//! Nothing runs until the model opens a conversation's browser: that starts
//! a headless Chromium on the conversation's own profile (cookies, logins
//! and storage stay on disk until the conversation is deleted) and drives
//! its one page over the DevTools protocol. Its viewport is its own size
//! (1920x1080 CSS pixels unless the model resizes it), not the window's:
//! the view (`browser_view.rs`) scales it down to a preview beside the
//! transcript, or to the lightbox the user expands it into. The page's
//! frames stream in (a screencast, at the size shown, only while it is on
//! screen), and the user's pointer, wheel and keys go to the page as input
//! events, as the model's clicks and keys do. Closing ends the browser;
//! the other calls fail while it is closed. Popups and new tabs open in the
//! one page.
//!
//! The model can open it on a named profile instead, shared between
//! conversations (one browser at a time on each), and manage its cookies,
//! site data, emulation and what it records (`state.rs`).

mod state;

pub(crate) use state::model_profiles;

use crate::cdp::{Cdp, Listener};
use base64::Engine as _;
use eframe::egui;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// From launch to the page being ready.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest an action waits for the page to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// A DevTools call's answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// The most text one read returns (the rest with `start`).
const READ_MAX: usize = 30_000;
/// The view counts as on screen this long after it was last drawn; past
/// that the screencast stops until it is drawn again.
const SHOWN_GRACE: Duration = Duration::from_millis(700);
/// Where a search typed in the address bar goes.
const SEARCH_URL: &str = "https://duckduckgo.com/?q=";

/// Why a call fails while the browser is closed.
pub(crate) const CLOSED: &str = "The browser is closed: open it with BROWSER action=open first.";
/// The viewport until the model resizes it, CSS pixels.
pub(crate) const DEFAULT_W: u32 = 1920;
pub(crate) const DEFAULT_H: u32 = 1080;
/// The viewport sizes the model may ask for, CSS pixels.
const MIN_SIDE: u32 = 200;
const MAX_SIDE: u32 = 4096;
/// How long a drag's pointer takes from start to end, and in how many moves.
const DRAG_MOVES: u32 = 16;
const DRAG_TIME: Duration = Duration::from_millis(320);
/// The longest `wait` waits.
const WAIT_MAX_S: u64 = 60;
/// How long the model's pointer takes to glide to where it goes, in the view.
const GLIDE: Duration = Duration::from_millis(260);

// ------------------------------------------------------------------ state

/// How the browser is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Status {
    #[default]
    Starting,
    Ready,
    /// It couldn't start.
    Failed(String),
    /// It was running and ended.
    Quit(String),
}

/// A dialog the page opened (alert, confirm, prompt, beforeunload): the
/// page waits until it is answered.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Dialog {
    pub(crate) kind: String,
    pub(crate) message: String,
    pub(crate) default: String,
}

/// A file chooser the user opened in the page, for Oak to show.
#[derive(Clone, Debug)]
pub(crate) struct Chooser {
    node: i64,
    pub(crate) multiple: bool,
}

/// A `<select>` under the pointer: its options show in Oak, since a
/// headless page draws no dropdowns.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Select {
    pub(crate) options: Vec<String>,
    pub(crate) selected: Option<usize>,
    /// Its box in the viewport, CSS pixels: left, top, width, height.
    pub(crate) rect: [f32; 4],
    pub(crate) disabled: bool,
}

/// The model's pointer (CSS pixels): it glides from `from` to `to`,
/// starting at `at`, over `glide`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AgentPointer {
    pub(crate) from: egui::Pos2,
    pub(crate) to: egui::Pos2,
    pub(crate) at: Instant,
    pub(crate) glide: Duration,
}

impl AgentPointer {
    /// Where it is now, and whether it is still on its way.
    pub(crate) fn now(&self) -> (egui::Pos2, bool) {
        let t = if self.glide.is_zero() { 1.0 } else { (self.at.elapsed().as_secs_f32() / self.glide.as_secs_f32()).min(1.0) };
        // Eased out: quick to leave, gentle to arrive.
        let e = 1.0 - (1.0 - t).powi(3);
        (self.from + (self.to - self.from) * e, t < 1.0)
    }
}

/// Who acted last: a file chooser opens for the user only after their own input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Actor {
    #[default]
    User,
    Agent,
}

/// What the view shows, and what the model is told after its next call.
#[derive(Default)]
pub(crate) struct View {
    pub(crate) status: Status,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) loading: bool,
    pub(crate) back: bool,
    pub(crate) forward: bool,
    pub(crate) dialog: Option<Dialog>,
    /// The newest frame, until the view uploads it.
    pub(crate) frame: Option<egui::ColorImage>,
    /// The page's cursor where the pointer is.
    pub(crate) cursor: egui::CursorIcon,
    /// The `<select>` under the pointer, if any.
    pub(crate) select: Option<Select>,
    /// Where the model last clicked (CSS pixels), and when.
    pub(crate) agent_at: Option<(egui::Pos2, Instant)>,
    /// The model's pointer, which the view draws gliding to where it went.
    pub(crate) agent_ptr: Option<AgentPointer>,
    /// A line over the page for a moment (a download, a crash).
    pub(crate) notice: Option<(String, Instant)>,
    pub(crate) chooser: Option<Chooser>,
    /// Text the user copied from the page, for the clipboard.
    pub(crate) copied: Option<String>,
    /// Things the model hasn't been told yet, with when they happened.
    notes: Vec<(String, Instant)>,
    actor: Actor,
    /// Downloads under way: guid → file name.
    downloads: HashMap<String, String>,
    /// Main-frame navigations so far.
    navs: u64,
}

impl View {
    fn note(&mut self, text: String) {
        self.notes.retain(|(_, at)| at.elapsed() < Duration::from_secs(300));
        if self.notes.len() >= 10 {
            self.notes.remove(0);
        }
        self.notes.push((text, Instant::now()));
    }

    fn notice(&mut self, text: String) {
        self.notice = Some((text, Instant::now()));
    }
}

/// The viewport, as the page lays out: CSS pixels, device pixels per CSS
/// pixel (as many as the expanded view shows, so it is sharp there), and
/// whether it passes for a phone.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Viewport {
    w: u32,
    h: u32,
    scale: f32,
    mobile: bool,
}

/// One conversation's browser.
pub(crate) struct Instance {
    pub(crate) conv: String,
    profile: PathBuf,
    /// The named profile it runs on; None: the conversation's own.
    profile_name: Option<String>,
    /// Chromium's user agent made a plain Chrome's: what emulate resets to.
    user_agent: Mutex<String>,
    /// The model's overrides (emulate), applied again after a restart.
    emulation: Mutex<state::Emulation>,
    /// The console, once the model has asked for it (recording starts then).
    console: Mutex<Option<state::Console>>,
    /// The page's requests.
    net: Mutex<state::NetLog>,
    ctx: egui::Context,
    pub(crate) view: Mutex<View>,
    /// The page's DevTools session, once it is up.
    page: Mutex<Option<Arc<Cdp>>>,
    /// The browser's own (for closing it gently, so its cookies are saved).
    root: Mutex<Option<Arc<Cdp>>>,
    child: Mutex<Option<Child>>,
    #[cfg(windows)]
    job: Mutex<usize>,
    closed: AtomicBool,
    port: AtomicU16,
    target: Mutex<String>,
    main_frame: Mutex<String>,
    viewport: Mutex<Viewport>,
    /// When the view last drew it.
    drawn: Mutex<Instant>,
    /// The largest frames the view wants (device pixels): what it shows them at.
    cast: Mutex<[u32; 2]>,
    /// When it was opened (the view fades it in).
    pub(crate) opened: Instant,
    pub(crate) texture: Mutex<Option<egui::TextureHandle>>,
    /// Pointer positions to read the page's cursor at.
    probe: Mutex<Option<Sender<(f32, f32)>>>,
    /// The model's calls running on it.
    agent: AtomicUsize,
}

static OPEN: LazyLock<Mutex<HashMap<String, Arc<Instance>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Where the conversations' browsers keep their profiles.
fn profiles_dir() -> PathBuf {
    crate::store::dir().join("browser")
}

/// Conversation `conv`'s profile folder.
fn profile_dir(conv: &str) -> PathBuf {
    let name: String = conv.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    profiles_dir().join(name)
}

/// The browser of conversation `conv`, if it is open.
pub(crate) fn get(conv: &str) -> Option<Arc<Instance>> {
    OPEN.lock().unwrap().get(conv).cloned()
}

pub(crate) fn is_open(conv: &str) -> bool {
    OPEN.lock().unwrap().contains_key(conv)
}

/// Profiles whose browser is still ending (closed, not yet quit): a new one
/// on the profile waits for it, since Chromium allows one per profile.
static CLOSING: LazyLock<Mutex<HashMap<PathBuf, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn closing(profile: &Path) -> bool {
    CLOSING.lock().unwrap().get(profile).is_some_and(|n| *n > 0)
}

/// Ends `inst` in the background, its profile counted as closing until it has.
fn end(inst: Arc<Instance>) {
    *CLOSING.lock().unwrap().entry(inst.profile.clone()).or_default() += 1;
    std::thread::spawn(move || {
        inst.shutdown();
        let mut c = CLOSING.lock().unwrap();
        if let Some(n) = c.get_mut(&inst.profile) {
            *n -= 1;
            if *n == 0 {
                c.remove(&inst.profile);
            }
        }
    });
}

/// Opens conversation `conv`'s browser: it starts in the background.
pub(crate) fn open(conv: &str, ctx: &egui::Context) -> Arc<Instance> {
    // (A conversation's own profile is never another's.)
    open_with(conv, ctx, profile_dir(conv), None, Default::default()).expect("the conversation's own profile")
}

/// Opens conversation `conv`'s browser on `profile` (named `name`, or the
/// conversation's own), unless another conversation's browser is on it.
fn open_with(conv: &str, ctx: &egui::Context, profile: PathBuf, name: Option<String>, emulation: state::Emulation) -> Result<Arc<Instance>, String> {
    let mut open = OPEN.lock().unwrap();
    if let Some(i) = open.get(conv) {
        return Ok(i.clone());
    }
    if open.values().any(|i| i.profile == profile) {
        return Err(format!(
            "The profile {} is in use by another conversation's browser: close it there first, or open another profile.",
            name.as_deref().map_or_else(|| profile.display().to_string(), |n| format!("{n:?}"))
        ));
    }
    let inst = Arc::new(Instance {
        conv: conv.to_string(),
        profile,
        profile_name: name,
        user_agent: Mutex::new(String::new()),
        emulation: Mutex::new(emulation),
        console: Mutex::new(None),
        net: Mutex::new(state::NetLog::default()),
        ctx: ctx.clone(),
        view: Mutex::new(View::default()),
        page: Mutex::new(None),
        root: Mutex::new(None),
        child: Mutex::new(None),
        #[cfg(windows)]
        job: Mutex::new(0),
        closed: AtomicBool::new(false),
        port: AtomicU16::new(0),
        target: Mutex::new(String::new()),
        main_frame: Mutex::new(String::new()),
        viewport: Mutex::new(Viewport { w: DEFAULT_W, h: DEFAULT_H, scale: 1.0, mobile: false }),
        drawn: Mutex::new(Instant::now()),
        cast: Mutex::new([DEFAULT_W, DEFAULT_H]),
        opened: Instant::now(),
        texture: Mutex::new(None),
        probe: Mutex::new(None),
        agent: AtomicUsize::new(0),
    });
    open.insert(conv.to_string(), inst.clone());
    drop(open);
    let i = inst.clone();
    let spawned = std::thread::Builder::new().name("browser".into()).spawn(move || run(i));
    if let Err(e) = spawned {
        inst.view.lock().unwrap().status = Status::Failed(format!("Couldn't start a thread for the browser: {e}"));
    }
    Ok(inst)
}

/// Closes conversation `conv`'s browser: it ends (in the background).
pub(crate) fn close(conv: &str) {
    let inst = OPEN.lock().unwrap().remove(conv);
    if let Some(inst) = inst {
        end(inst);
    }
}

/// Starts conversation `conv`'s browser again (after it failed or quit),
/// on the same profile, at the same size, with the same overrides.
pub(crate) fn restart(conv: &str, ctx: &egui::Context) {
    let old = OPEN.lock().unwrap().remove(conv);
    let ctx = ctx.clone();
    let conv = conv.to_string();
    std::thread::spawn(move || {
        let (mut profile, mut name, mut size, mut emulation) = (profile_dir(&conv), None, None, None);
        if let Some(old) = old {
            size = Some(*old.viewport.lock().unwrap());
            emulation = Some(old.emulation.lock().unwrap().clone());
            (profile, name) = (old.profile.clone(), old.profile_name.clone());
            old.shutdown();
        }
        let inst = match open_with(&conv, &ctx, profile, name, emulation.unwrap_or_default()) {
            Ok(i) => i,
            Err(_) => open(&conv, &ctx),
        };
        if let Some(vp) = size {
            inst.resize(vp.w, vp.h, vp.mobile);
        }
        ctx.request_repaint();
    });
}

/// Closes the browsers of conversations `keep` says no to (their tabs closed).
pub(crate) fn retain(keep: impl Fn(&str) -> bool) {
    let gone: Vec<String> = OPEN.lock().unwrap().keys().filter(|c| !keep(c)).cloned().collect();
    for c in gone {
        close(&c);
    }
}

/// Conversation `conv` was deleted: its browser ends and its profile goes.
pub(crate) fn forget(conv: &str) {
    let inst = OPEN.lock().unwrap().remove(conv);
    let dir = profile_dir(conv);
    if inst.is_none() && !dir.exists() {
        return;
    }
    std::thread::spawn(move || {
        if let Some(inst) = inst {
            inst.shutdown();
        }
        // The browser may hold its files for a moment after it ends.
        for _ in 0..30 {
            if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}

/// Oak is closing: every browser ends, gently, all at once.
pub(crate) fn shutdown_all() {
    let all: Vec<Arc<Instance>> = OPEN.lock().unwrap().drain().map(|(_, i)| i).collect();
    let threads: Vec<_> = all.into_iter().map(|i| std::thread::spawn(move || i.shutdown())).collect();
    for t in threads {
        let _ = t.join();
    }
}

// ----------------------------------------------------------------- launch

/// The browser to run: OAK_BROWSER, else Chrome or another Chromium (as
/// FETCH and SCREENSHOT find them).
fn find_browser() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OAK_BROWSER").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    crate::web::browser()
}

const NO_BROWSER: &str = "No Chrome or Chromium found (looked for Chrome, Chromium, Brave, Edge and Playwright's Chromium). Install one, or set OAK_BROWSER to its path.";

/// A browser left running on the profile (Oak killed, say) holds its lock,
/// and a new one would quit at once: it is ended, by the PID Oak kept, once
/// its command line shows it is on this profile.
fn reap_stale(profile: &Path) {
    let Ok(pid) = std::fs::read_to_string(profile.join("oak.pid")) else { return };
    let Ok(pid) = pid.trim().parse::<i32>() else { return };
    #[cfg(target_os = "linux")]
    {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let want = format!("--user-data-dir={}\0", profile.display());
        if cmdline.windows(want.len()).any(|w| w == want.as_bytes()) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            for _ in 0..50 {
                if !Path::new(&format!("/proc/{pid}")).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let _ = pid;
}

impl Instance {
    fn page(&self) -> Option<Arc<Cdp>> {
        self.page.lock().unwrap().clone()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Starts the browser and readies its page; the page's events, for
    /// the loop.
    fn start(&self) -> Result<Listener, String> {
        let exe = find_browser().ok_or(NO_BROWSER)?;
        std::fs::create_dir_all(&self.profile).map_err(|e| format!("Couldn't make the browser's profile folder: {e}"))?;
        // A browser closed on this profile a moment ago may not have quit yet.
        let t = Instant::now();
        while closing(&self.profile) && t.elapsed() < Duration::from_secs(10) && !self.is_closed() {
            std::thread::sleep(Duration::from_millis(50));
        }
        reap_stale(&self.profile);
        let vp = *self.viewport.lock().unwrap();
        let mut cmd = Command::new(&exe);
        cmd.args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-blink-features=AutomationControlled",
            "--disable-features=Translate,MediaRouter,OptimizationHints",
            "--hide-crash-restore-bubble",
        ])
        .arg(format!("--window-size={},{}", vp.w, vp.h))
        .arg(format!("--user-data-dir={}", self.profile.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        // It ends with Oak, even if Oak is killed: the thread that starts it
        // runs until it is closed.
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {}: {e}", exe.display()))?;
        let _ = std::fs::write(self.profile.join("oak.pid"), child.id().to_string());
        #[cfg(windows)]
        {
            *self.job.lock().unwrap() = crate::cdp::job_object(&child);
        }
        let stderr = child.stderr.take().expect("piped");
        *self.child.lock().unwrap() = Some(child);
        let rx = crate::cdp::devtools_address(stderr, "browser-log", "ws://").map_err(|e| e.to_string())?;
        let ws = self.devtools_ws(&rx)?;
        let port = ws.trim_start_matches("ws://").split(['/', ':']).nth(1).and_then(|p| p.parse::<u16>().ok()).ok_or(format!("Unexpected DevTools address {ws}"))?;
        self.port.store(port, Ordering::SeqCst);
        *self.root.lock().unwrap() = Cdp::connect(&ws).ok();
        let cdp = self.open_page(port)?;
        let events = cdp.listen();
        self.set_up_page(&cdp)?;
        // The viewport may have changed while Chromium was starting. Apply
        // its latest size and publish the page under the same lock, so a
        // draw cannot update the size before there is a page to send it to.
        {
            let vp = self.viewport.lock().unwrap();
            apply_viewport(&cdp, *vp);
            *self.page.lock().unwrap() = Some(cdp.clone());
        }
        // Where it was when it was last closed.
        let last = std::fs::read_to_string(self.profile.join("oak-last-url")).unwrap_or_default();
        let last = last.trim();
        if !last.is_empty() && last != "about:blank" {
            cdp.send("Page.navigate", json!({"url": last}));
        }
        Ok(events)
    }

    /// The DevTools address the starting browser prints (`rx`), once it
    /// does; an error when it quits, takes too long or is closed meanwhile.
    fn devtools_ws(&self, rx: &std::sync::mpsc::Receiver<String>) -> Result<String, String> {
        let t = Instant::now();
        loop {
            if self.is_closed() {
                return Err("Closed.".into());
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ws) => return Ok(ws.trim().to_string()),
                Err(RecvTimeoutError::Disconnected) => return Err("The browser quit as it started (is its profile in use by another browser?).".into()),
                Err(RecvTimeoutError::Timeout) if t.elapsed() > START_TIMEOUT => return Err("The browser didn't start in time.".into()),
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(Ok(Some(status))) = self.child.lock().unwrap().as_mut().map(|c| c.try_wait()) {
                        return Err(format!("The browser quit as it started ({status})."));
                    }
                }
            }
        }
    }

    /// Connects to the browser's one page: the tab it opened with (or a
    /// new one).
    fn open_page(&self, port: u16) -> Result<Arc<Cdp>, String> {
        let targets = http_json(port, "GET", "/json/list")?;
        let page = targets.as_array().into_iter().flatten().find(|t| t["type"] == "page").cloned();
        let page = match page {
            Some(p) => p,
            None => http_json(port, "PUT", "/json/new?about:blank")?,
        };
        let ws = page["webSocketDebuggerUrl"].as_str().ok_or("The browser gave no address for its page.")?;
        *self.target.lock().unwrap() = page["id"].as_str().unwrap_or("").to_string();
        Cdp::connect(ws).map_err(|e| e.to_string())
    }

    /// Readies the page: its events and main frame, a plain Chrome's user
    /// agent (or the model's overrides), one page only, file choosers,
    /// downloads, and the network log.
    fn set_up_page(&self, cdp: &Arc<Cdp>) -> Result<(), String> {
        let call = |m: &str, p: Value| cdp.call(m, p).map_err(|e| e.to_string());
        call("Page.enable", json!({}))?;
        let tree = call("Page.getFrameTree", json!({}))?;
        *self.main_frame.lock().unwrap() = tree["frameTree"]["frame"]["id"].as_str().unwrap_or("").to_string();
        // A plain Chrome's user agent, not HeadlessChrome's.
        if let Ok(v) = call("Browser.getVersion", json!({})) {
            *self.user_agent.lock().unwrap() = v["userAgent"].as_str().unwrap_or("").replace("HeadlessChrome", "Chrome");
        }
        // (Pages can't tell the Network domain is on, unlike Runtime's.)
        let _ = call("Network.enable", json!({"maxPostDataSize": state::POST_MAX}));
        let emulation = self.emulation.lock().unwrap().clone();
        if let Err(e) = self.apply_emulation(cdp, &emulation) {
            self.update_view(|v| v.note(format!("The overrides set with emulate couldn't all be applied again: {e}")));
        }
        let _ = call("Page.addScriptToEvaluateOnNewDocument", json!({"source": ONE_PAGE_JS}));
        let _ = call("Page.setInterceptFileChooserDialog", json!({"enabled": true}));
        let _ = call("Emulation.setFocusEmulationEnabled", json!({"enabled": true}));
        let downloads = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir);
        let _ = call("Page.setDownloadBehavior", json!({"behavior": "allow", "downloadPath": downloads.display().to_string()}));
        Ok(())
    }

    /// Ends the browser: gently (so it saves its cookies), else killed.
    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.probe.lock().unwrap().take();
        if let Some(root) = self.root.lock().unwrap().take() {
            let _ = root.call_for("Browser.close", json!({}), Duration::from_secs(3));
            root.close();
        }
        if let Some(page) = self.page.lock().unwrap().take() {
            page.close();
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(5) && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(self.profile.join("oak.pid"));
        #[cfg(windows)]
        unsafe {
            let job = std::mem::take(&mut *self.job.lock().unwrap());
            if job != 0 {
                windows_sys::Win32::Foundation::CloseHandle(job as _);
            }
        }
        self.texture.lock().unwrap().take();
    }
}

/// A DevTools HTTP endpoint's JSON answer.
fn http_json(port: u16, method: &str, path: &str) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().map_err(|e| e.to_string())?;
    let url = format!("http://127.0.0.1:{port}{path}");
    let req = if method == "PUT" { client.put(&url) } else { client.get(&url) };
    req.send().and_then(|r| r.json()).map_err(|e| format!("The browser didn't answer {path}: {e}"))
}

fn apply_viewport(cdp: &Cdp, vp: Viewport) {
    cdp.send(
        "Emulation.setDeviceMetricsOverride",
        json!({"width": vp.w, "height": vp.h, "deviceScaleFactor": vp.scale, "mobile": vp.mobile, "screenWidth": vp.w, "screenHeight": vp.h}),
    );
    cdp.send("Emulation.setTouchEmulationEnabled", json!({"enabled": vp.mobile}));
}

/// The browser's thread: starts it, then follows its page until it is closed.
fn run(inst: Arc<Instance>) {
    let events = match inst.start() {
        Ok(e) => e,
        Err(e) => {
            if !inst.is_closed() {
                inst.update_view(|v| v.status = Status::Failed(e));
                // What did start goes, but the view stays to say why.
                inst.shutdown_process();
            }
            return;
        }
    };
    inst.update_view(|v| v.status = Status::Ready);
    // The cursor reader: the newest pointer position, one at a time.
    let (ptx, prx) = mpsc::channel::<(f32, f32)>();
    *inst.probe.lock().unwrap() = Some(ptx);
    let i = inst.clone();
    std::thread::spawn(move || {
        while let Ok(mut at) = prx.recv() {
            while let Ok(newer) = prx.try_recv() {
                at = newer;
            }
            if i.is_closed() {
                break;
            }
            i.read_cursor(at);
        }
    });
    inst.refresh_history();
    // The frames' size while they stream.
    let mut casting: Option<[u32; 2]> = None;
    loop {
        if inst.is_closed() {
            return;
        }
        match events.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) if ev["method"] == "__closed__" => break,
            Ok(ev) => inst.event(&ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Frames only while the view is on screen, at the size it shows
        // them (the preview's small, the expanded view's large).
        let shown = inst.drawn.lock().unwrap().elapsed() < SHOWN_GRACE;
        let want = shown.then(|| *inst.cast.lock().unwrap());
        if want != casting
            && let Some(page) = inst.page()
        {
            if casting.is_some() {
                page.send("Page.stopScreencast", json!({}));
            }
            if let Some([w, h]) = want {
                page.send("Page.startScreencast", json!({"format": "jpeg", "quality": 85, "maxWidth": w, "maxHeight": h, "everyNthFrame": 1}));
            }
            casting = want;
        }
    }
    if !inst.is_closed() {
        let code = inst.child.lock().unwrap().as_mut().and_then(|c| c.try_wait().ok().flatten());
        let why = match code {
            Some(s) => format!("The browser quit ({s})."),
            None => "The browser stopped answering.".into(),
        };
        inst.update_view(|v| v.status = Status::Quit(why));
        inst.shutdown_process();
    }
}

impl Instance {
    /// Ends the process without marking it closed (it shows why).
    fn shutdown_process(&self) {
        if let Some(page) = self.page.lock().unwrap().take() {
            page.close();
        }
        if let Some(root) = self.root.lock().unwrap().take() {
            root.close();
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(self.profile.join("oak.pid"));
    }

    /// Changes the view, then repaints it.
    fn update_view(&self, f: impl FnOnce(&mut View)) {
        f(&mut self.view.lock().unwrap());
        self.ctx.request_repaint();
    }

    fn is_main(&self, frame: &Value) -> bool {
        frame.as_str().is_some_and(|f| *self.main_frame.lock().unwrap() == f)
    }

    /// One event from the page.
    fn event(self: &Arc<Self>, ev: &Value) {
        let p = &ev["params"];
        let Some(page) = self.page() else { return };
        match ev["method"].as_str().unwrap_or("") {
            "Page.screencastFrame" => {
                // Acknowledged once decoded, so frames come no faster than
                // they can be shown.
                let image = B64.decode(p["data"].as_str().unwrap_or("")).ok().and_then(|b| image::load_from_memory_with_format(&b, image::ImageFormat::Jpeg).ok());
                if let Some(img) = image {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    self.update_view(|v| v.frame = Some(egui::ColorImage::from_rgba_unmultiplied(size, &rgba)));
                }
                page.send("Page.screencastFrameAck", json!({"sessionId": p["sessionId"]}));
            }
            "Page.frameNavigated" if p["frame"]["parentId"].is_null() => {
                let f = &p["frame"];
                *self.main_frame.lock().unwrap() = f["id"].as_str().unwrap_or("").to_string();
                let url = format!("{}{}", f["url"].as_str().unwrap_or(""), f["urlFragment"].as_str().unwrap_or(""));
                self.set_url(url);
                self.view.lock().unwrap().navs += 1;
            }
            "Page.navigatedWithinDocument" if self.is_main(&p["frameId"]) => {
                self.set_url(p["url"].as_str().unwrap_or("").to_string());
                self.refresh_history();
            }
            "Page.frameStartedLoading" | "Page.frameRequestedNavigation" if self.is_main(&p["frameId"]) => self.update_view(|v| v.loading = true),
            "Page.frameStoppedLoading" if self.is_main(&p["frameId"]) => {
                self.view.lock().unwrap().loading = false;
                self.refresh_history();
            }
            "Page.domContentEventFired" | "Page.loadEventFired" => self.refresh_title(),
            "Page.javascriptDialogOpening" => {
                let d = Dialog {
                    kind: p["type"].as_str().unwrap_or("alert").to_string(),
                    message: p["message"].as_str().unwrap_or("").to_string(),
                    default: p["defaultPrompt"].as_str().unwrap_or("").to_string(),
                };
                self.update_view(|v| {
                    v.note(format!("The page opened a {} dialog: {:?}", d.kind, d.message));
                    v.dialog = Some(d);
                });
            }
            "Page.javascriptDialogClosed" => self.update_view(|v| v.dialog = None),
            "Page.fileChooserOpened" => {
                let chooser = Chooser { node: p["backendNodeId"].as_i64().unwrap_or(0), multiple: p["mode"] == "selectMultiple" };
                self.update_view(|v| match v.actor {
                    Actor::User => v.chooser = Some(chooser),
                    Actor::Agent => v.note("A file chooser opened: set its files with BROWSER action=upload (the file input's ref).".into()),
                });
            }
            "Page.downloadWillBegin" => {
                let name = p["suggestedFilename"].as_str().unwrap_or("a file").to_string();
                let mut v = self.view.lock().unwrap();
                v.notice(format!("Downloading {name}…"));
                v.downloads.insert(p["guid"].as_str().unwrap_or("").to_string(), name);
            }
            "Page.downloadProgress" if p["state"] != "inProgress" => self.update_view(|v| {
                if let Some(name) = v.downloads.remove(p["guid"].as_str().unwrap_or("")) {
                    let dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_default();
                    let text = match p["state"].as_str() {
                        Some("completed") => format!("Downloaded {name} to {}", dir.display()),
                        _ => format!("The download of {name} was canceled"),
                    };
                    v.notice(text.clone());
                    v.note(text);
                }
            }),
            "Page.windowOpen" => {
                // A new window the page script didn't catch: its address
                // opens here, and the window goes.
                let url = p["url"].as_str().unwrap_or("").to_string();
                if !url.is_empty() {
                    page.send("Page.navigate", json!({"url": url}));
                }
                let i = self.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    i.close_other_tabs();
                });
            }
            "Inspector.targetCrashed" => self.update_view(|v| {
                v.notice("The page crashed. Reload it.".into());
                v.note("The page crashed; reload it.".into());
            }),
            // The console and the network log.
            method => self.record(method, p),
        }
    }

    /// Any tab but the page goes (single page per conversation).
    fn close_other_tabs(&self) {
        let port = self.port.load(Ordering::SeqCst);
        let ours = self.target.lock().unwrap().clone();
        let Ok(list) = http_json(port, "GET", "/json/list") else { return };
        for t in list.as_array().into_iter().flatten() {
            if t["type"] == "page" && t["id"].as_str().is_some_and(|id| id != ours) {
                let _ = http_json(port, "GET", &format!("/json/close/{}", t["id"].as_str().unwrap_or("")));
            }
        }
    }

    fn set_url(&self, url: String) {
        if url != self.view.lock().unwrap().url {
            let _ = std::fs::write(self.profile.join("oak-last-url"), &url);
        }
        self.update_view(|v| v.url = url);
    }

    /// Back and forward, and the title, from the page.
    fn refresh_history(&self) {
        let Some(page) = self.page() else { return };
        if let Ok(h) = page.call_for("Page.getNavigationHistory", json!({}), Duration::from_secs(5)) {
            let at = h["currentIndex"].as_i64().unwrap_or(0);
            let n = h["entries"].as_array().map_or(0, |e| e.len()) as i64;
            let mut v = self.view.lock().unwrap();
            v.back = at > 0;
            v.forward = at + 1 < n;
            if let Some(e) = h["entries"].get(at as usize) {
                v.title = e["title"].as_str().unwrap_or("").to_string();
            }
        }
        self.refresh_title();
    }

    fn refresh_title(&self) {
        let Some(page) = self.page() else { return };
        if let Ok(r) = page.call_for("Runtime.evaluate", json!({"expression": "document.title", "returnByValue": true}), Duration::from_secs(3))
            && let Some(t) = r["result"]["value"].as_str()
        {
            self.update_view(|v| v.title = t.to_string());
        }
    }

    /// The page's cursor (and `<select>`) at a point, for the view.
    fn read_cursor(&self, (x, y): (f32, f32)) {
        let Some(page) = self.page() else { return };
        if self.view.lock().unwrap().dialog.is_some() {
            return;
        }
        let js = CURSOR_JS.replace("__X__", &x.to_string()).replace("__Y__", &y.to_string());
        let Ok(r) = page.call_for("Runtime.evaluate", json!({"expression": js, "returnByValue": true}), Duration::from_secs(2)) else { return };
        let v = &r["result"]["value"];
        let cursor = css_cursor(v["cursor"].as_str().unwrap_or("default"));
        let select = v["select"].is_object().then(|| Select {
            options: v["select"]["options"].as_array().into_iter().flatten().map(|o| o.as_str().unwrap_or("").to_string()).collect(),
            selected: v["select"]["selected"].as_i64().filter(|i| *i >= 0).map(|i| i as usize),
            rect: std::array::from_fn(|k| v["select"]["rect"][k].as_f64().unwrap_or(0.0) as f32),
            disabled: v["select"]["disabled"] == true,
        });
        let mut view = self.view.lock().unwrap();
        if view.cursor != cursor || view.select != select {
            view.cursor = cursor;
            view.select = select;
            drop(view);
            self.ctx.request_repaint();
        }
    }
}

/// The egui cursor for a CSS one.
fn css_cursor(css: &str) -> egui::CursorIcon {
    use egui::CursorIcon as C;
    match css {
        "pointer" => C::PointingHand,
        "text" | "vertical-text" => C::Text,
        "move" | "all-scroll" => C::Move,
        "grab" => C::Grab,
        "grabbing" => C::Grabbing,
        "not-allowed" | "no-drop" => C::NotAllowed,
        "wait" => C::Wait,
        "progress" => C::Progress,
        "crosshair" => C::Crosshair,
        "help" => C::Help,
        "cell" => C::Cell,
        "copy" => C::Copy,
        "alias" => C::Alias,
        "context-menu" => C::ContextMenu,
        "col-resize" => C::ResizeColumn,
        "row-resize" => C::ResizeRow,
        "ew-resize" | "e-resize" | "w-resize" => C::ResizeHorizontal,
        "ns-resize" | "n-resize" | "s-resize" => C::ResizeVertical,
        "nwse-resize" | "nw-resize" | "se-resize" => C::ResizeNwSe,
        "nesw-resize" | "ne-resize" | "sw-resize" => C::ResizeNeSw,
        "zoom-in" => C::ZoomIn,
        "zoom-out" => C::ZoomOut,
        "none" => C::None,
        _ => C::Default,
    }
}

// -------------------------------------------------------- the user's input

/// A mouse button, as the page names it.
pub(crate) fn button_name(b: egui::PointerButton) -> &'static str {
    match b {
        egui::PointerButton::Primary => "left",
        egui::PointerButton::Secondary => "right",
        egui::PointerButton::Middle => "middle",
        egui::PointerButton::Extra1 => "back",
        egui::PointerButton::Extra2 => "forward",
    }
}

/// The page's modifier bits: Alt 1, Ctrl 2, Meta 4, Shift 8.
pub(crate) fn modifier_bits(m: egui::Modifiers) -> i64 {
    (m.alt as i64) | (m.ctrl as i64) << 1 | (m.mac_cmd as i64) << 2 | (m.shift as i64) << 3
}

impl Instance {
    /// The view drew it this frame, its frames `cast` device pixels at
    /// most; `scale`: the device pixels per CSS pixel the expanded view
    /// would show (the page renders at that many, at least 1).
    pub(crate) fn shown(&self, scale: f32, cast: [u32; 2]) {
        *self.drawn.lock().unwrap() = Instant::now();
        // Steps of a quarter, so a window being resized doesn't re-render the page each frame.
        let scale = ((scale * 4.0).round() / 4.0).clamp(1.0, 3.0);
        // Steps of 64 pixels, for the same reason.
        let cast = cast.map(|s| (s.div_ceil(64) * 64).clamp(64, 8192));
        *self.cast.lock().unwrap() = cast;
        self.set_viewport(|v| v.scale = scale);
    }

    /// The viewport's size in CSS pixels, and whether it passes for a phone.
    pub(crate) fn viewport_css(&self) -> (u32, u32) {
        let v = self.viewport.lock().unwrap();
        (v.w, v.h)
    }

    /// Changes the viewport (and the page, if it is up, follows).
    fn set_viewport(&self, f: impl FnOnce(&mut Viewport)) {
        let mut cur = self.viewport.lock().unwrap();
        let mut vp = *cur;
        f(&mut vp);
        if *cur != vp {
            *cur = vp;
            // (Under the lock, so start() can't publish its page between.)
            if let Some(page) = self.page() {
                apply_viewport(&page, vp);
            }
            drop(cur);
            self.ctx.request_repaint();
        }
    }

    /// The model sets the viewport: `w` x `h` CSS pixels.
    fn resize(&self, w: u32, h: u32, mobile: bool) {
        self.set_viewport(|v| {
            (v.w, v.h, v.mobile) = (w.clamp(MIN_SIDE, MAX_SIDE), h.clamp(MIN_SIDE, MAX_SIDE), mobile);
        });
    }

    fn user(&self) -> Option<Arc<Cdp>> {
        self.view.lock().unwrap().actor = Actor::User;
        self.page()
    }

    /// A mouse event from the user: `kind` is mouseMoved, mousePressed or
    /// mouseReleased; `buttons` the buttons held (1 left, 2 right, 4 middle).
    pub(crate) fn mouse(&self, kind: &str, at: egui::Pos2, button: &str, buttons: i64, clicks: i64, mods: i64) {
        let page = if kind == "mouseMoved" { self.page() } else { self.user() };
        if let Some(page) = page {
            page.send("Input.dispatchMouseEvent", json!({"type": kind, "x": at.x, "y": at.y, "button": button, "buttons": buttons, "clickCount": clicks, "modifiers": mods}));
        }
        if kind == "mouseMoved"
            && let Some(tx) = self.probe.lock().unwrap().as_ref()
        {
            let _ = tx.send((at.x, at.y));
        }
    }

    pub(crate) fn wheel(&self, at: egui::Pos2, dx: f32, dy: f32, mods: i64) {
        if let Some(page) = self.page() {
            page.send("Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": at.x, "y": at.y, "deltaX": dx, "deltaY": dy, "modifiers": mods}));
        }
    }

    /// A key from the user (egui's name for it), down or up.
    pub(crate) fn key(&self, name: &str, down: bool, mods: i64) {
        let Some(k) = key_info(name, mods & 8 != 0) else { return };
        let Some(page) = self.user() else { return };
        if down {
            page.send("Input.dispatchKeyEvent", k.event("rawKeyDown", mods, false));
            // Enter types a return; other text comes as its own event.
            if k.key == "Enter" && mods & 7 == 0 {
                page.send("Input.dispatchKeyEvent", json!({"type": "char", "text": "\r", "unmodifiedText": "\r", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": mods}));
            }
        } else {
            page.send("Input.dispatchKeyEvent", k.event("keyUp", mods, false));
        }
    }

    /// Text the user typed.
    pub(crate) fn text(&self, text: &str) {
        let Some(page) = self.user() else { return };
        for c in text.chars() {
            let s = c.to_string();
            page.send("Input.dispatchKeyEvent", json!({"type": "char", "text": s, "unmodifiedText": s, "key": s}));
        }
    }

    pub(crate) fn paste(&self, text: &str) {
        if let Some(page) = self.user() {
            page.send("Input.insertText", json!({"text": text}));
        }
    }

    /// Copies the page's selection to the clipboard (Ctrl+C); `cut`
    /// deletes it too.
    pub(crate) fn copy(self: &Arc<Self>, cut: bool) {
        let i = self.clone();
        std::thread::spawn(move || {
            let Some(page) = i.user() else { return };
            let Ok(r) = page.call_for("Runtime.evaluate", json!({"expression": SELECTION_JS, "returnByValue": true}), Duration::from_secs(3)) else { return };
            let text = r["result"]["value"].as_str().unwrap_or("").to_string();
            if text.is_empty() {
                return;
            }
            if cut {
                let _ = page.call_for("Runtime.evaluate", json!({"expression": "document.execCommand('delete')"}), Duration::from_secs(3));
            }
            i.update_view(|v| v.copied = Some(text));
        });
    }

    /// The address bar: goes to what was typed (a search unless it reads as an address).
    pub(crate) fn navigate(&self, typed: &str) {
        if let Some(page) = self.user() {
            page.send("Page.navigate", json!({"url": address(typed, true)}));
        }
    }

    pub(crate) fn reload(&self) {
        if let Some(page) = self.user() {
            page.send("Page.reload", json!({}));
        }
    }

    pub(crate) fn stop_loading(&self) {
        if let Some(page) = self.user() {
            page.send("Page.stopLoading", json!({}));
        }
    }

    /// Back (-1) or forward (1) in the page's history.
    pub(crate) fn go(self: &Arc<Self>, by: i64) {
        let i = self.clone();
        std::thread::spawn(move || {
            i.view.lock().unwrap().actor = Actor::User;
            let _ = i.history(by);
        });
    }

    fn history(&self, by: i64) -> Result<(), String> {
        let page = self.page().ok_or("The browser isn't running.")?;
        let h = page.call("Page.getNavigationHistory", json!({})).map_err(|e| e.to_string())?;
        let at = h["currentIndex"].as_i64().unwrap_or(0) + by;
        let entry = h["entries"].get(at.max(-1) as usize).filter(|_| at >= 0).ok_or(if by < 0 { "There is no page to go back to." } else { "There is no page to go forward to." })?;
        page.call("Page.navigateToHistoryEntry", json!({"entryId": entry["id"]})).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The user's answer to the page's dialog.
    pub(crate) fn answer_dialog(&self, accept: bool, text: &str) {
        if let Some(page) = self.user() {
            page.send("Page.handleJavaScriptDialog", json!({"accept": accept, "promptText": text}));
        }
        self.view.lock().unwrap().dialog = None;
    }

    /// The file chooser the user opened, to show (once).
    pub(crate) fn take_chooser(&self) -> Option<Chooser> {
        self.view.lock().unwrap().chooser.take()
    }

    /// The files the user picked for a chooser (none: canceled).
    pub(crate) fn choose_files(&self, c: &Chooser, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        if let Some(page) = self.page() {
            let files: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
            page.send("DOM.setFileInputFiles", json!({"files": files, "backendNodeId": c.node}));
        }
    }

    /// The user picked option `index` of the `<select>` under the pointer.
    pub(crate) fn pick_option(&self, at: egui::Pos2, index: usize) {
        if let Some(page) = self.user() {
            let js = PICK_OPTION_JS.replace("__X__", &at.x.to_string()).replace("__Y__", &at.y.to_string()).replace("__I__", &index.to_string());
            page.send("Runtime.evaluate", json!({"expression": js, "userGesture": true}));
        }
    }

    /// Whether one of the model's calls is running on it.
    pub(crate) fn agent_busy(&self) -> bool {
        self.agent.load(Ordering::SeqCst) > 0
    }
}

/// What an address bar or a `goto` means: an address as typed (a scheme
/// added: http for this machine, https otherwise), or, when `search` and
/// it doesn't read as one, a search.
pub(crate) fn address(typed: &str, search: bool) -> String {
    let s = typed.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    let lower = s.to_lowercase();
    let local = ["localhost", "127.", "0.0.0.0", "[::1]"].iter().any(|h| lower.starts_with(h));
    if lower.contains("://") || ["about:", "data:", "file:", "chrome:", "view-source:", "javascript:"].iter().any(|p| lower.starts_with(p)) {
        return s.to_string();
    }
    if local {
        return format!("http://{s}");
    }
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let looks = !s.contains(char::is_whitespace) && host.contains('.') && !host.starts_with('.') && !host.ends_with('.');
    if looks || !search {
        return format!("https://{s}");
    }
    let q: String = s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else if b == b' ' { "+".into() } else { format!("%{b:02X}") }).collect();
    format!("{SEARCH_URL}{q}")
}

// ------------------------------------------------------------------- keys

/// A key as the page gets it.
#[derive(Debug, PartialEq)]
struct KeyInfo {
    key: String,
    code: String,
    vk: i64,
    /// What it types without Ctrl, Alt or Meta, if anything.
    text: Option<String>,
}

impl KeyInfo {
    fn event(&self, kind: &str, mods: i64, with_text: bool) -> Value {
        let mut e = json!({"type": kind, "key": self.key, "code": self.code, "windowsVirtualKeyCode": self.vk, "nativeVirtualKeyCode": self.vk, "modifiers": mods});
        if with_text && let Some(t) = &self.text {
            e["text"] = json!(t);
            e["unmodifiedText"] = json!(t);
        }
        // Ctrl+A (Meta+A) as the editing command too, which a headless
        // page doesn't always map the key to by itself.
        if kind != "keyUp" && self.code == "KeyA" && (mods == 2 || mods == 4) {
            e["commands"] = json!(["selectAll"]);
        }
        e
    }
}

/// A key by name: egui's (A, Num1, Enter, Up, Semicolon…) or the page's
/// (a, 1, Enter, ArrowUp, ;…). `shift` makes letters capitals and
/// punctuation its shifted sign (US layout).
fn key_info(name: &str, shift: bool) -> Option<KeyInfo> {
    let named = |key: &str, code: &str, vk: i64, text: Option<&str>| Some(KeyInfo { key: key.into(), code: code.into(), vk, text: text.map(Into::into) });
    let lower = name.to_lowercase();
    match lower.as_str() {
        "enter" | "return" => return named("Enter", "Enter", 13, Some("\r")),
        "tab" => return named("Tab", "Tab", 9, None),
        "backspace" => return named("Backspace", "Backspace", 8, None),
        "delete" | "del" => return named("Delete", "Delete", 46, None),
        "escape" | "esc" => return named("Escape", "Escape", 27, None),
        "space" | " " => return named(" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => return named("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => return named("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => return named("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => return named("ArrowRight", "ArrowRight", 39, None),
        "home" => return named("Home", "Home", 36, None),
        "end" => return named("End", "End", 35, None),
        "pageup" => return named("PageUp", "PageUp", 33, None),
        "pagedown" => return named("PageDown", "PageDown", 34, None),
        "insert" => return named("Insert", "Insert", 45, None),
        _ => {}
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<i64>().ok()).filter(|n| (1..=24).contains(n)) {
        let k = format!("F{n}");
        return named(&k, &k, 111 + n, None);
    }
    // egui's names for the signs, and digits.
    let sign = match name {
        "Num0" | "0" => '0', "Num1" | "1" => '1', "Num2" | "2" => '2', "Num3" | "3" => '3', "Num4" | "4" => '4',
        "Num5" | "5" => '5', "Num6" | "6" => '6', "Num7" | "7" => '7', "Num8" | "8" => '8', "Num9" | "9" => '9',
        "Comma" => ',', "Minus" => '-', "Period" => '.', "Equals" => '=', "Semicolon" => ';', "Backslash" => '\\',
        "Slash" => '/', "OpenBracket" => '[', "CloseBracket" => ']', "Backtick" => '`', "Quote" => '\'',
        _ => {
            let mut cs = name.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) => c,
                _ => return None,
            }
        }
    };
    const SIGNS: [(char, char, &str, i64); 21] = [
        ('0', ')', "Digit0", 48), ('1', '!', "Digit1", 49), ('2', '@', "Digit2", 50), ('3', '#', "Digit3", 51), ('4', '$', "Digit4", 52),
        ('5', '%', "Digit5", 53), ('6', '^', "Digit6", 54), ('7', '&', "Digit7", 55), ('8', '*', "Digit8", 56), ('9', '(', "Digit9", 57),
        (';', ':', "Semicolon", 186), ('=', '+', "Equal", 187), (',', '<', "Comma", 188), ('-', '_', "Minus", 189), ('.', '>', "Period", 190),
        ('/', '?', "Slash", 191), ('`', '~', "Backquote", 192), ('[', '{', "BracketLeft", 219), ('\\', '|', "Backslash", 220), (']', '}', "BracketRight", 221),
        ('\'', '"', "Quote", 222),
    ];
    if sign.is_ascii_alphabetic() {
        let up = sign.to_ascii_uppercase();
        let c = if shift { up } else { sign.to_ascii_lowercase() };
        return Some(KeyInfo { key: c.to_string(), code: format!("Key{up}"), vk: up as i64, text: Some(c.to_string()) });
    }
    if let Some(&(plain, shifted, code, vk)) = SIGNS.iter().find(|s| s.0 == sign || s.1 == sign) {
        let c = if shift || sign == shifted { shifted } else { plain };
        return Some(KeyInfo { key: c.to_string(), code: code.into(), vk, text: Some(c.to_string()) });
    }
    // Any other character: typed as it is.
    Some(KeyInfo { key: sign.to_string(), code: String::new(), vk: 0, text: Some(sign.to_string()) })
}

/// A key the model names, with modifiers: "Enter", "Control+a",
/// "Shift+Tab", "Meta+ArrowLeft". The key and the modifier bits.
fn parse_combo(combo: &str) -> Result<(KeyInfo, i64), String> {
    let combo = combo.trim();
    // "+" alone, or ending a combo ("Control++"), is the plus key.
    let (mods_part, key) = match combo.strip_suffix("++") {
        Some(m) => (m, "+"),
        None if combo == "+" => ("", "+"),
        None => combo.rsplit_once('+').unwrap_or(("", combo)),
    };
    let mut mods = 0;
    for m in mods_part.split('+').filter(|m| !m.is_empty()) {
        mods |= match m.trim().to_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" | "super" | "win" => 4,
            "shift" => 8,
            other => return Err(format!("Unknown modifier `{other}` in `{combo}`: use Control, Shift, Alt or Meta.")),
        };
    }
    let shift = mods & 8 != 0;
    let k = key_info(key, shift).ok_or(format!("Unknown key `{key}`: use a name like Enter, Tab, Escape, Backspace, ArrowDown, PageDown, F5, or one character."))?;
    Ok((k, mods))
}

// ------------------------------------------------------------ page scripts

/// Each frame of the page: new windows and other targets open in the page
/// itself, as there is only the one.
const ONE_PAGE_JS: &str = r#"(() => {
  if (window.__oakOnePage) return;
  window.__oakOnePage = true;
  const top_ = window === window.top;
  const go = (u) => {
    if (u === undefined || u === null || String(u) === '') return window;
    const href = new URL(String(u), location.href).href;
    try { if (top_) location.href = href; else window.top.location.href = href; } catch (e) { location.href = href; }
    return window;
  };
  try { window.open = function (u) { return go(u); }; } catch (e) {}
  const keep = (t) => !t || /^_(self|top|parent)$/i.test(t);
  const fix = (e) => {
    const a = e.target && e.target.closest && e.target.closest('a[target], area[target]');
    if (a && !keep(a.target)) a.target = top_ ? '_self' : '_top';
  };
  document.addEventListener('click', fix, true);
  document.addEventListener('auxclick', fix, true);
  document.addEventListener('submit', (e) => { const f = e.target; if (f && !keep(f.target)) f.target = top_ ? '_self' : '_top'; }, true);
})();"#;

/// The selection (in a text field or the page), for copying.
const SELECTION_JS: &str = r#"(() => {
  let d = document, a = d.activeElement;
  while (a && a.contentDocument) { try { d = a.contentDocument; a = d.activeElement; } catch (e) { break; } }
  if (a && (a.localName === 'textarea' || a.localName === 'input') && typeof a.selectionStart === 'number') return a.value.slice(a.selectionStart, a.selectionEnd);
  return String(d.getSelection ? d.getSelection() : '');
})()"#;

/// The cursor at (__X__, __Y__), and the `<select>` there if any.
const CURSOR_JS: &str = r#"(() => {
  let x = __X__, y = __Y__, d = document, el = d.elementFromPoint(x, y), ox = 0, oy = 0;
  while (el && (el.localName === 'iframe' || el.localName === 'frame')) {
    let inner = null;
    try { const r = el.getBoundingClientRect(); const cd = el.contentDocument; if (cd) { ox += r.left; oy += r.top; inner = cd.elementFromPoint(x - ox, y - oy); d = cd; } } catch (e) {}
    if (!inner) break;
    el = inner;
  }
  if (!el) return { cursor: 'default' };
  const sel = el.closest('select');
  if (sel && !sel.multiple && sel.size <= 1) {
    const r = sel.getBoundingClientRect();
    return { cursor: 'default', select: { options: Array.from(sel.options).map(o => o.label || o.text), selected: sel.selectedIndex, rect: [r.left + ox, r.top + oy, r.width, r.height], disabled: sel.disabled } };
  }
  const c = getComputedStyle(el).cursor;
  if (c && c !== 'auto') return { cursor: c };
  const t = el.localName;
  if (el.isContentEditable || t === 'textarea' || (t === 'input' && !['button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'image', 'range', 'color'].includes(el.type))) return { cursor: 'text' };
  if (el.closest('a[href]')) return { cursor: 'pointer' };
  return { cursor: 'default' };
})()"#;

/// Picks option __I__ of the `<select>` at (__X__, __Y__), as a person would.
const PICK_OPTION_JS: &str = r#"(() => {
  let x = __X__, y = __Y__, d = document, el = d.elementFromPoint(x, y);
  while (el && (el.localName === 'iframe' || el.localName === 'frame')) {
    let inner = null;
    try { const r = el.getBoundingClientRect(); const cd = el.contentDocument; if (cd) { x -= r.left; y -= r.top; inner = cd.elementFromPoint(x, y); } } catch (e) {}
    if (!inner) break;
    el = inner;
  }
  const s = el && el.closest('select');
  if (!s || s.disabled) return false;
  s.focus();
  s.selectedIndex = __I__;
  s.dispatchEvent(new Event('input', { bubbles: true }));
  s.dispatchEvent(new Event('change', { bubbles: true }));
  return true;
})()"#;

/// What the model's scripts share: describing elements as `read` lists them.
const HELPERS_JS: &str = r#"
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const clip = (s, n) => s.length > n ? s.slice(0, n - 1) + '…' : s;
  const q = (s, n) => JSON.stringify(clip(clean(s), n || 80));
  const short = (h) => { try { const u = new URL(h, location.href); return u.origin === location.origin ? (u.pathname + u.search + u.hash) : u.href; } catch (e) { return h; } };
  const ROLES = new Set('button link checkbox radio switch tab menuitem menuitemcheckbox menuitemradio option treeitem combobox textbox searchbox slider spinbutton listbox'.split(' '));
  const ATOMIC = new Set('button link checkbox radio switch tab menuitem menuitemcheckbox menuitemradio option treeitem slider spinbutton combobox textbox searchbox'.split(' '));
  const styleOf = (el) => { try { return el.ownerDocument.defaultView.getComputedStyle(el); } catch (e) { return null; } };
  const labelOf = (el) => {
    const a = el.getAttribute('aria-label'); if (clean(a)) return a;
    const lb = el.getAttribute('aria-labelledby');
    if (lb) { const t = lb.split(/\s+/).map((id) => { const e = el.ownerDocument.getElementById(id); return e ? (e.innerText || e.textContent) : ''; }).join(' '); if (clean(t)) return t; }
    if (el.labels && el.labels.length) { const t = Array.from(el.labels).map((l) => l.innerText).join(' '); if (clean(t)) return t; }
    return '';
  };
  const textOf = (el) => {
    let t = clean(el.innerText || el.textContent);
    if (!t && el.querySelector) { const img = el.querySelector('img[alt]'); if (img) t = clean(img.alt); }
    if (!t) t = clean(el.getAttribute('title') || '');
    return t;
  };
  const kindOf = (el, cursor, parentCursor) => {
    const t = el.localName;
    if (t === 'a' && el.hasAttribute('href')) return 'link';
    if (t === 'button') return 'button';
    if (t === 'input') return el.type === 'hidden' ? null : 'input';
    if (t === 'select') return 'select';
    if (t === 'textarea') return 'textarea';
    if (t === 'summary') return 'summary';
    const role = (el.getAttribute('role') || '').trim();
    if (ROLES.has(role)) return role;
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'editable';
    if (el.hasAttribute('onclick') || (el.hasAttribute('tabindex') && el.tabIndex >= 0)) return 'clickable';
    if (cursor === 'pointer' && parentCursor !== 'pointer') return 'clickable';
    return null;
  };
  // [text, atomic]: an atomic element's content is in its description.
  const describe = (el, kind) => {
    const t = el.localName;
    const dis = (el.disabled || el.getAttribute('aria-disabled') === 'true') ? ' disabled' : '';
    const long = clean(el.innerText).length > 150;
    let state = '';
    const ac = el.getAttribute('aria-checked'); if (ac === 'true') state += ' checked'; else if (ac === 'mixed') state += ' mixed';
    if (el.getAttribute('aria-selected') === 'true') state += ' selected';
    if (el.getAttribute('aria-pressed') === 'true') state += ' pressed';
    const ex = el.getAttribute('aria-expanded'); if (ex === 'true') state += ' expanded'; else if (ex === 'false') state += ' collapsed';
    if (kind === 'link') {
      const href = el.getAttribute('href') || '';
      const to = href && !/^javascript:/i.test(href) ? ' → ' + short(el.href) : '';
      if (long) return ['link' + to, false];
      return ['link ' + q(labelOf(el) || textOf(el)) + to + state, true];
    }
    if (t === 'input') {
      const type = el.type || 'text';
      const lab = labelOf(el) || el.getAttribute('placeholder') || el.name || el.title || '';
      if (type === 'checkbox' || type === 'radio') return [type + ' ' + q(lab) + (el.checked ? ' checked' : '') + dis, true];
      if (type === 'button' || type === 'submit' || type === 'reset' || type === 'image') return ['button ' + q(el.value || lab || el.alt || type) + dis, true];
      if (type === 'file') return ['file input ' + q(lab) + (el.files && el.files.length ? ' files=' + q(Array.from(el.files).map((f) => f.name).join(', ')) : '') + dis, true];
      let s = 'input' + (type !== 'text' ? ' type=' + type : '') + ' ' + q(lab);
      if (el.value) s += ' value=' + (type === 'password' ? '"••••"' : q(el.value, 200));
      const ph = el.getAttribute('placeholder');
      if (ph && ph !== lab) s += ' placeholder=' + q(ph);
      return [s + (el.readOnly ? ' readonly' : '') + dis, true];
    }
    if (t === 'select') {
      const opts = Array.from(el.options);
      const sel = opts.filter((o) => o.selected).map((o) => clean(o.text)).join(', ');
      let list = opts.slice(0, 15).map((o) => clean(o.text)).join(' | ');
      if (opts.length > 15) list += ' | … ' + (opts.length - 15) + ' more';
      return ['select ' + q(labelOf(el) || el.name || '') + ' = ' + q(sel) + ' (options: ' + list + ')' + dis, true];
    }
    if (t === 'textarea') return ['textarea ' + q(labelOf(el) || el.getAttribute('placeholder') || el.name || '') + (el.value ? ' value=' + q(el.value, 300) : '') + dis, true];
    if (kind === 'editable') return ['editable ' + q(labelOf(el)) + ' text=' + q(el.innerText, 300), true];
    if (kind === 'combobox' || kind === 'textbox' || kind === 'searchbox') return [kind + ' ' + q(labelOf(el)) + ((el.value || el.innerText) ? ' value=' + q(el.value || el.innerText, 200) : '') + state + dis, true];
    if (t === 'summary') return ['summary ' + q(textOf(el)) + (el.parentElement && el.parentElement.open ? ' expanded' : ' collapsed'), true];
    if (ATOMIC.has(kind) || kind === 'button') {
      if (long) return [kind + state + dis, false];
      return [kind + ' ' + q(labelOf(el) || textOf(el) || el.value || '') + state + dis, true];
    }
    const lab = labelOf(el) || el.getAttribute('title') || '';
    return [kind + (lab ? ' ' + q(lab) : '') + state + dis, false];
  };
  const brief = (el) => {
    const kind = kindOf(el, 'auto', 'auto');
    if (!kind) return el.localName + (textOf(el) ? ' ' + q(textOf(el), 60) : '');
    return describe(el, kind)[0];
  };
  const refEl = (n) => {
    const r = window.__oakRefs;
    const el = r && r[n];
    if (!el) throw new Error('No element ' + n + ': refs come from the latest read; read the page again.');
    if (!el.isConnected) throw new Error('Element ' + n + ' is no longer on the page (it changed since the read); read it again.');
    return el;
  };
  // The middle of `el` in the top page's viewport, scrolled into view
  // (only when it isn't in view: a visible element stays where it is).
  const pointOf = (el) => {
    const b = el.getBoundingClientRect();
    const seen = el.ownerDocument === document && b.width > 0 && b.height > 0 && b.top >= 0 && b.left >= 0 && b.bottom <= innerHeight && b.right <= innerWidth;
    if (!seen) el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    const rs = Array.from(el.getClientRects()).filter((r) => r.width > 0 && r.height > 0);
    if (!rs.length) return null;
    const r = rs[0];
    let x = r.left + r.width / 2, y = r.top + r.height / 2, w = el.ownerDocument.defaultView;
    while (w && w.frameElement) { const f = w.frameElement.getBoundingClientRect(); x += f.left + w.frameElement.clientLeft; y += f.top + w.frameElement.clientTop; w = w.parent; }
    return { x, y };
  };
  const deepActive = () => {
    let a = document.activeElement;
    for (;;) {
      if (a && a.shadowRoot && a.shadowRoot.activeElement) { a = a.shadowRoot.activeElement; continue; }
      if (a && (a.localName === 'iframe' || a.localName === 'frame')) { try { const d = a.contentDocument; if (d && d.activeElement) { a = d.activeElement; continue; } } catch (e) {} }
      return a;
    }
  };
"#;

/// The page as text, its interactive elements numbered (kept for refs).
const READ_JS: &str = r#"(() => {
  __HELPERS__
  const refs = [null];
  const parts = [];
  const push = (s) => parts.push(s);
  const nl = () => parts.push('\n');
  const SKIP = new Set('script style noscript template head meta link title svg canvas audio video source track math object embed datalist'.split(' '));
  const walk = (node, pre, parentCursor) => {
    if (node.nodeType === 3) { const t = pre ? node.data : node.data.replace(/\s+/g, ' '); if (t) push(t); return; }
    if (node.nodeType === 11) { for (const c of node.childNodes) walk(c, pre, parentCursor); return; }
    if (node.nodeType !== 1) return;
    const el = node, tag = el.localName;
    if (SKIP.has(tag)) return;
    if (tag === 'slot') { const a = el.assignedNodes({ flatten: true }); for (const c of (a.length ? a : el.childNodes)) walk(c, pre, parentCursor); return; }
    const st = styleOf(el);
    if (st && (st.display === 'none' || st.visibility === 'hidden' || st.visibility === 'collapse')) return;
    const disp = st ? st.display : 'block';
    const block = !(disp.startsWith('inline') || disp === 'contents' || disp.startsWith('table-cell') || disp === 'ruby');
    const cell = disp === 'table-cell' || tag === 'td' || tag === 'th';
    if (tag === 'br') { nl(); return; }
    if (block) nl();
    const h = /^h([1-6])$/.exec(tag);
    if (h) push('#'.repeat(+h[1]) + ' ');
    if (tag === 'li') push('- ');
    if (tag === 'hr') { push('---'); nl(); return; }
    if (tag === 'img') { const alt = clean(el.getAttribute('alt')); if (alt) push('[image: ' + clip(alt, 100) + ']'); }
    if (tag === 'iframe' || tag === 'frame') {
      let d = null;
      try { d = el.contentDocument; } catch (e) {}
      if (d && d.body) { nl(); walk(d.body, false, 'auto'); nl(); } else push('[frame ' + short(el.src || '') + ']');
      return;
    }
    const cursor = st ? st.cursor : 'auto';
    const kind = kindOf(el, cursor, parentCursor);
    if (kind) {
      refs.push(el);
      const [text, atomic] = describe(el, kind);
      push(' [' + (refs.length - 1) + ' ' + text + '] ');
      if (atomic) { if (cell) push(' | '); if (block) nl(); return; }
    }
    const kids = el.shadowRoot ? el.shadowRoot.childNodes : el.childNodes;
    const p = pre || (st && st.whiteSpace.startsWith('pre'));
    for (const c of kids) walk(c, p, cursor);
    if (cell) push(' | ');
    if (block) nl();
  };
  if (document.body) walk(document.body, false, 'auto');
  window.__oakRefs = refs;
  let text = parts.join('')
    .replace(/[ \t\u00a0]+/g, ' ')
    .replace(/ ?\| *(\n|$)/g, '\n')
    .split('\n').map((l) => l.trim()).join('\n')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
  const a = deepActive();
  const focused = refs.indexOf(a);
  const se = document.scrollingElement || document.documentElement;
  return { title: document.title, url: location.href, text, n: refs.length - 1, focused: focused > 0 ? '[' + focused + ' ' + describe(a, kindOf(a, 'auto', 'auto') || a.localName)[0] + ']' : '',
           scrollY: Math.round(scrollY), height: Math.round(se ? se.scrollHeight : 0), vw: innerWidth, vh: innerHeight };
})()"#;

/// Element __N__'s middle, scrolled into view, and what it is.
const POINT_JS: &str = r#"(() => {
  __HELPERS__
  try {
    const el = refEl(__N__);
    const p = pointOf(el);
    if (!p) return { error: 'Element __N__ (' + brief(el) + ') takes up no space on the page (hidden?), so it can\'t be clicked.' };
    return { x: p.x, y: p.y, desc: brief(el) };
  } catch (e) { return { error: e.message }; }
})()"#;

/// What is at (__X__, __Y__).
const AT_POINT_JS: &str = r#"(() => {
  __HELPERS__
  let x = __X__, y = __Y__, el = document.elementFromPoint(x, y);
  while (el && (el.localName === 'iframe' || el.localName === 'frame')) {
    let inner = null;
    try { const r = el.getBoundingClientRect(); const cd = el.contentDocument; if (cd) { x -= r.left; y -= r.top; inner = cd.elementFromPoint(x, y); } } catch (e) {}
    if (!inner) break;
    el = inner;
  }
  if (!el) return 'nothing';
  let k = el;
  while (k && !kindOf(k, 'auto', 'auto')) k = k.parentElement;
  return brief(k || el);
})()"#;

/// Focuses element __N__ for typing, its content selected so typing
/// replaces it; a `<select>` instead picks the option reading __TEXT__.
const FOCUS_JS: &str = r#"(() => {
  __HELPERS__
  try {
    const el = refEl(__N__);
    const want = __TEXT__;
    if (el.localName === 'select') {
      const opts = Array.from(el.options);
      const w = clean(want).toLowerCase();
      const o = opts.find((o) => clean(o.text).toLowerCase() === w) || opts.find((o) => o.value === want) || opts.find((o) => clean(o.text).toLowerCase().includes(w));
      if (!o) return { error: 'Element __N__ is a select without an option reading ' + JSON.stringify(want) + ' (it has: ' + opts.map((o) => clean(o.text)).slice(0, 30).join(' | ') + ').' };
      el.focus();
      el.value = o.value;
      o.selected = true;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return { picked: true, desc: brief(el) };
    }
    el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    el.focus();
    if (typeof el.select === 'function' && (el.localName === 'input' || el.localName === 'textarea')) el.select();
    else if (el.isContentEditable) { const d = el.ownerDocument, r = d.createRange(); r.selectNodeContents(el); const s = d.defaultView.getSelection(); s.removeAllRanges(); s.addRange(r); }
    const a = deepActive();
    if (a !== el && !(el.contains && el.contains(a))) return { error: 'Element __N__ (' + brief(el) + ') didn\'t take the focus, so it can\'t be typed into; click it first, or pick another.' };
    return { desc: brief(el) };
  } catch (e) { return { error: e.message }; }
})()"#;

/// The focused element, as read lists it (after typing).
const FOCUSED_JS: &str = r#"(() => {
  __HELPERS__
  const a = deepActive();
  if (!a || a === document.body || a === document.documentElement) return '';
  const i = (window.__oakRefs || []).indexOf(a);
  return (i > 0 ? '[' + i + ' ' : '[') + describe(a, kindOf(a, 'auto', 'auto') || a.localName)[0] + ']';
})()"#;

/// Element __N__, checked to be a file input.
const FILE_INPUT_JS: &str = r#"(() => {
  __HELPERS__
  const el = refEl(__N__);
  if (!(el.localName === 'input' && el.type === 'file')) throw new Error('Element __N__ (' + brief(el) + ') isn\'t a file input.');
  return el;
})()"#;

/// Whether __TEXT__ is in the page's text (the top document's, frames it can reach included).
const WAIT_JS: &str = r#"(() => {
  const want = __TEXT__;
  const has = (d) => { try { if (d.body && d.body.innerText.includes(want)) return true; for (const f of d.querySelectorAll('iframe, frame')) { const cd = f.contentDocument; if (cd && has(cd)) return true; } } catch (e) {} return false; };
  return has(document);
})()"#;

const SCROLL_JS: &str = r#"(() => { const se = document.scrollingElement || document.documentElement; return [Math.round(scrollY), Math.round(se ? se.scrollHeight : 0), innerHeight]; })()"#;

/// `s` with its first letter a capital.
fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn script(js: &str) -> String {
    js.replace("__HELPERS__", HELPERS_JS)
}

/// A call's `ref`: a number, or one as read shows it ("[12]").
fn ref_of(input: &Value) -> Option<u64> {
    input["ref"].as_u64().or_else(|| input["ref"].as_str().and_then(|s| s.trim().trim_matches(['[', ']']).parse().ok()))
}

// ---------------------------------------------------------- the model's calls

impl Instance {
    /// Waits for the browser to be ready (it may still be starting).
    fn ready(&self) -> Result<Arc<Cdp>, String> {
        let t = Instant::now();
        loop {
            if self.is_closed() {
                return Err(CLOSED.into());
            }
            // Do not hold the view lock while waiting: the startup
            // thread needs it to publish Ready (or its failure).
            let status = self.view.lock().unwrap().status.clone();
            match status {
                Status::Ready => {}
                Status::Starting if t.elapsed() < START_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Status::Starting => return Err("The browser is still starting; try again in a moment.".into()),
                Status::Failed(e) | Status::Quit(e) => return Err(format!("{e} Close it and open it again (or the user can restart it from the expanded view).")),
            }
            return self.page().ok_or_else(|| "The browser isn't running.".into());
        }
    }

    /// The open dialog, as a reason to stop waiting.
    fn dialog_open(&self) -> Option<String> {
        self.view.lock().unwrap().dialog.as_ref().map(|d| format!("The page opened a {} dialog ({:?}), which it waits on: answer it with BROWSER action=dialog.", d.kind, d.message))
    }

    /// Runs `js` (an expression) in the page: its value.
    fn eval(&self, page: &Cdp, js: &str, timeout: Duration) -> Result<Value, String> {
        let r = page
            .call_until("Runtime.evaluate", json!({"expression": js, "returnByValue": true, "awaitPromise": true, "userGesture": true, "replMode": true}), timeout, &|| self.dialog_open())
            .map_err(|e| e.to_string())?;
        if let Some(e) = r.get("exceptionDetails") {
            let text = e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or("error");
            let text = text.strip_prefix("Error: ").unwrap_or(text);
            return Err(text.lines().take(6).collect::<Vec<_>>().join("\n"));
        }
        Ok(r["result"]["value"].clone())
    }

    /// The model's script: its value, a promise it evaluates to awaited.
    /// (REPL mode, which lets it use top-level await and redeclare `let`s,
    /// hands back such a promise unawaited, as `{}` by value: the object
    /// is awaited and read out with a second call.)
    fn eval_model(&self, page: &Cdp, js: &str, timeout: Duration) -> Result<Value, String> {
        let stop = || self.dialog_open();
        let error = |r: &Value| -> Option<String> {
            let e = r.get("exceptionDetails")?;
            let text = e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or("error");
            let text = text.strip_prefix("Error: ").unwrap_or(text);
            Some(text.lines().take(6).collect::<Vec<_>>().join("\n"))
        };
        let group = "oak-eval";
        let r = page
            .call_until("Runtime.evaluate", json!({"expression": js, "objectGroup": group, "awaitPromise": true, "userGesture": true, "replMode": true}), timeout, &stop)
            .map_err(|e| e.to_string())?;
        if let Some(e) = error(&r) {
            return Err(e);
        }
        let Some(id) = r["result"]["objectId"].as_str() else {
            // A primitive comes by value (undefined has none).
            return Ok(r["result"]["value"].clone());
        };
        let v = page.call_until(
            "Runtime.callFunctionOn",
            json!({"functionDeclaration": "async function () { return await this; }", "objectId": id, "awaitPromise": true, "returnByValue": true, "userGesture": true}),
            timeout,
            &stop,
        );
        page.send("Runtime.releaseObjectGroup", json!({"objectGroup": group}));
        let v = v.map_err(|e| e.to_string())?;
        if let Some(e) = error(&v) {
            return Err(e);
        }
        Ok(v["result"]["value"].clone())
    }

    /// After an action: waits for what it started (a navigation, a load)
    /// to settle, up to LOAD_TIMEOUT.
    fn settle(&self, navs: u64) {
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(150));
        loop {
            let v = self.view.lock().unwrap();
            if v.dialog.is_some() || t.elapsed() > LOAD_TIMEOUT || (!v.loading && t.elapsed() > Duration::from_millis(250)) {
                break;
            }
            drop(v);
            std::thread::sleep(Duration::from_millis(50));
        }
        // A new page: a moment for its scripts to draw it.
        if self.view.lock().unwrap().navs != navs {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn navs(&self) -> u64 {
        self.view.lock().unwrap().navs
    }

    /// Where the page is, and what the model hasn't been told yet.
    fn report(&self, done: &str) -> String {
        let mut v = self.view.lock().unwrap();
        let title = if v.title.is_empty() { String::new() } else { format!("{:?} ", v.title) };
        let mut out = format!("{done}\nPage: {title}{}", v.url);
        if v.loading {
            out += " (still loading)";
        }
        for (n, _) in v.notes.drain(..) {
            out += &format!("\n{n}");
        }
        if let Some(d) = &v.dialog {
            out += &format!("\nA {} dialog is open ({:?}): the page waits until you answer it with BROWSER action=dialog.", d.kind, d.message);
        }
        out
    }

    /// The model's pointer goes to (x, y), CSS pixels: the page gets the
    /// move (with `buttons` held), and the view glides it there over `glide`
    /// (from the middle the first time).
    fn agent_move(&self, page: &Cdp, x: f64, y: f64, buttons: i64, glide: Duration) -> Result<(), String> {
        let to = egui::pos2(x as f32, y as f32);
        let (w, h) = self.viewport_size();
        self.update_view(|v| {
            v.actor = Actor::Agent;
            let from = v.agent_ptr.map_or(egui::pos2(w as f32 / 2.0, h as f32 / 2.0), |p| p.now().0);
            v.agent_ptr = Some(AgentPointer { from, to, at: Instant::now(), glide });
        });
        let button = if buttons & 1 != 0 { "left" } else if buttons & 2 != 0 { "right" } else if buttons & 4 != 0 { "middle" } else { "none" };
        // Chromium routes input asynchronously (hit-testing, then the
        // renderer). A later Runtime.evaluate can overtake it: sleeping in
        // settle is not a delivery barrier, especially on a busy renderer.
        // The dispatch reply acknowledges input processing. User pointer
        // streaming remains nonblocking; model actions must wait for it.
        page.call_until("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y, "button": button, "buttons": buttons}), CALL_TIMEOUT, &|| self.dialog_open())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The model clicks `button` (left, right or middle) at (x, y), CSS
    /// pixels, `clicks` times in a row (2: a double click).
    fn click_at(&self, page: &Cdp, x: f64, y: f64, button: &str, clicks: i64) -> Result<(), String> {
        self.agent_move(page, x, y, 0, GLIDE)?;
        // The ring shows as the pointer arrives.
        self.update_view(|v| v.agent_at = Some((egui::pos2(x as f32, y as f32), Instant::now() + GLIDE / 2)));
        let bit = match button {
            "right" => 2,
            "middle" => 4,
            _ => 1,
        };
        for n in 1..=clicks {
            page.send("Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": x, "y": y, "button": button, "buttons": bit, "clickCount": n}));
            page.send("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": x, "y": y, "button": button, "buttons": 0, "clickCount": n}));
        }
        Ok(())
    }

    /// A point the model names: element `ref_key` (from read; scrolled into
    /// view) or the point `x_key`,`y_key`. The point, and what is there.
    fn target(&self, page: &Cdp, input: &Value, ref_key: &str, x_key: &str, y_key: &str) -> Result<(f64, f64, String), String> {
        let n = input[ref_key].as_u64().or_else(|| input[ref_key].as_str().and_then(|s| s.trim().trim_matches(['[', ']']).parse().ok()));
        match (n, input[x_key].as_f64(), input[y_key].as_f64()) {
            (Some(n), _, _) => {
                let r = self.eval(page, &script(POINT_JS).replace("__N__", &n.to_string()), CALL_TIMEOUT)?;
                if let Some(e) = r["error"].as_str() {
                    return Err(e.to_string());
                }
                Ok((r["x"].as_f64().unwrap_or(0.0), r["y"].as_f64().unwrap_or(0.0), format!("[{n} {}]", r["desc"].as_str().unwrap_or(""))))
            }
            (None, Some(x), Some(y)) => {
                let (w, h) = self.viewport_size();
                if x < 0.0 || y < 0.0 || x >= w as f64 || y >= h as f64 {
                    return Err(format!("({x}, {y}) is outside the viewport ({w}x{h}); scroll first, or use a ref."));
                }
                let js = script(AT_POINT_JS).replace("__X__", &x.to_string()).replace("__Y__", &y.to_string());
                let what = self.eval(page, &js, CALL_TIMEOUT).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                Ok((x, y, format!("({x}, {y}), on {what}")))
            }
            _ => Err(format!("Give `{ref_key}` (from read), or `{x_key}` and `{y_key}`.")),
        }
    }

    fn viewport_size(&self) -> (u32, u32) {
        let vp = self.viewport.lock().unwrap();
        (vp.w, vp.h)
    }

    /// What the model hasn't been told yet (`report` without the page
    /// line), after a blank line; empty when there is nothing.
    fn notes(&self) -> String {
        let notes = self.report("");
        let notes: Vec<&str> = notes.lines().skip(2).collect();
        if notes.is_empty() { String::new() } else { format!("\n\n{}", notes.join("\n")) }
    }

    /// One of the model's calls (all but screenshot).
    fn call(&self, name: &str, input: &Value, folder: &Path) -> Result<String, String> {
        let page = self.ready()?;
        if name != "BROWSER_DIALOG"
            && let Some(d) = self.dialog_open()
        {
            return Err(d);
        }
        let navs = self.navs();
        let str_of = |k: &str| input[k].as_str().unwrap_or("");
        self.view.lock().unwrap().actor = Actor::Agent;
        match name {
            "BROWSER_GOTO" => {
                let url = address(str_of("url"), false);
                let r = page.call("Page.navigate", json!({"url": url})).map_err(|e| e.to_string())?;
                if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                    return Err(format!("Couldn't open {url}: {e}"));
                }
                self.view.lock().unwrap().loading = true;
                self.settle(navs);
                Ok(self.report("Opened."))
            }
            "BROWSER_BACK" | "BROWSER_FORWARD" => {
                self.history(if name == "BROWSER_BACK" { -1 } else { 1 })?;
                self.settle(navs);
                Ok(self.report(if name == "BROWSER_BACK" { "Went back." } else { "Went forward." }))
            }
            "BROWSER_RELOAD" => {
                page.call("Page.reload", json!({})).map_err(|e| e.to_string())?;
                self.view.lock().unwrap().loading = true;
                self.settle(navs);
                Ok(self.report("Reloaded."))
            }
            "BROWSER_READ" => self.read(&page, input["start"].as_u64().unwrap_or(0) as usize),
            "BROWSER_CLICK" => self.click(&page, input, navs),
            "BROWSER_TYPE" => self.type_text(&page, input, navs),
            "BROWSER_KEY" => {
                let (k, mods) = parse_combo(str_of("key"))?;
                // Text only without Ctrl, Alt and Meta (those are shortcuts).
                let typing = mods & 7 == 0 && k.text.is_some();
                page.send("Input.dispatchKeyEvent", k.event(if typing { "keyDown" } else { "rawKeyDown" }, mods, typing));
                page.send("Input.dispatchKeyEvent", k.event("keyUp", mods, false));
                self.settle(navs);
                Ok(self.report(&format!("Pressed {}.", str_of("key").trim())))
            }
            "BROWSER_SCROLL" => {
                // To an element: into view.
                if ref_of(input).is_some() {
                    let (_, _, what) = self.target(&page, input, "ref", "x", "y")?;
                    std::thread::sleep(Duration::from_millis(150));
                    let s = self.eval(&page, SCROLL_JS, CALL_TIMEOUT)?;
                    return Ok(self.report(&format!("Scrolled {what} into view. The page is at {} of {} px (viewport height {}).", s[0], s[1], s[2])));
                }
                let (w, h) = self.viewport_size();
                let dx = input["dx"].as_f64().unwrap_or(0.0);
                let dy = input["dy"].as_f64().unwrap_or(if dx == 0.0 { h as f64 * 0.8 } else { 0.0 });
                let x = input["x"].as_f64().unwrap_or(w as f64 / 2.0);
                let y = input["y"].as_f64().unwrap_or(h as f64 / 2.0);
                self.agent_move(&page, x, y, 0, GLIDE)?;
                page.send("Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}));
                std::thread::sleep(Duration::from_millis(350));
                let s = self.eval(&page, SCROLL_JS, CALL_TIMEOUT)?;
                let by = if dx == 0.0 { format!("{dy} px") } else { format!("{dx} px across, {dy} px down") };
                Ok(self.report(&format!("Scrolled {by}. The page is at {} of {} px (viewport height {}).", s[0], s[1], s[2])))
            }
            "BROWSER_HOVER" => self.hover(&page, input, navs),
            "BROWSER_DRAG" => self.drag(&page, input, navs),
            "BROWSER_WAIT" => self.wait(&page, input, navs),
            "BROWSER_RESIZE" => {
                let (w, h) = self.viewport_size();
                let nw = input["width"].as_u64().map_or(w, |v| v as u32);
                let nh = input["height"].as_u64().map_or(h, |v| v as u32);
                let mobile = input["mobile"].as_bool().unwrap_or(false);
                self.resize(nw, nh, mobile);
                // A moment for the page to lay itself out again.
                std::thread::sleep(Duration::from_millis(300));
                let (w, h) = self.viewport_size();
                Ok(self.report(&format!("The viewport is {w}x{h} CSS px{}.", if mobile { ", as a phone (touch, mobile layout)" } else { "" })))
            }
            "BROWSER_EVAL" => {
                let text = match self.eval_model(&page, str_of("script"), CALL_TIMEOUT)? {
                    Value::Null => "undefined or null".to_string(),
                    Value::String(s) => s,
                    v => serde_json::to_string_pretty(&v).unwrap_or_default(),
                };
                Ok(crate::tools::clip(&text) + &self.notes())
            }
            "BROWSER_DIALOG" => {
                if self.view.lock().unwrap().dialog.is_none() {
                    return Err("No dialog is open.".into());
                }
                let accept = input["accept"].as_bool().unwrap_or(true);
                page.call("Page.handleJavaScriptDialog", json!({"accept": accept, "promptText": str_of("text")})).map_err(|e| e.to_string())?;
                self.view.lock().unwrap().dialog = None;
                self.settle(navs);
                Ok(self.report(if accept { "Accepted the dialog." } else { "Dismissed the dialog." }))
            }
            "BROWSER_UPLOAD" => self.upload(&page, input, folder, navs),
            "BROWSER_COOKIES" => self.cookies(&page, input, folder),
            "BROWSER_SET_COOKIES" => self.set_cookies(&page, input, folder),
            "BROWSER_DELETE_COOKIES" => self.delete_cookies(&page, input),
            "BROWSER_CLEAR_DATA" => self.clear_data(&page, input),
            "BROWSER_EMULATE" => self.emulate(&page, input),
            "BROWSER_CONSOLE" => self.console(&page),
            "BROWSER_NETWORK" => self.network(&page, input, folder),
            _ => Err(format!("Unknown tool {name}")),
        }
    }

    /// BROWSER read: the page's text from character `start`, with its
    /// interactive elements as refs.
    fn read(&self, page: &Cdp, start: usize) -> Result<String, String> {
        let r = self.eval(page, &script(READ_JS), CALL_TIMEOUT)?;
        let text = r["text"].as_str().unwrap_or("");
        let total = text.chars().count();
        let part: String = text.chars().skip(start).take(READ_MAX).collect();
        let shown = part.chars().count();
        let mut out = format!(
            "Title: {}\nURL: {}\nScrolled to {} of {} px (viewport {}x{}){}\n{} interactive elements, as [ref kind \"label\"]; the refs hold until the page changes.\n\n",
            r["title"].as_str().unwrap_or(""),
            r["url"].as_str().unwrap_or(""),
            r["scrollY"],
            r["height"],
            r["vw"],
            r["vh"],
            match r["focused"].as_str().filter(|f| !f.is_empty()) {
                Some(f) => format!("\nFocused: {f}"),
                None => String::new(),
            },
            r["n"],
        );
        if start > 0 {
            out += &format!("[… from character {start} of {total}]\n");
        }
        out += if part.is_empty() && start == 0 { "(no text)" } else { &part };
        if start + shown < total {
            out += &format!("\n[… {} more characters; read on with start={}]", total - start - shown, start + shown);
        }
        Ok(out + &self.notes())
    }

    /// BROWSER click, on a ref or at a point: `button`, `clicks` times.
    fn click(&self, page: &Cdp, input: &Value, navs: u64) -> Result<String, String> {
        let (x, y, what) = self.target(page, input, "ref", "x", "y")?;
        let button = input["button"].as_str().unwrap_or("left");
        if !["left", "right", "middle"].contains(&button) {
            return Err(format!("Unknown button `{button}`: use left, right or middle."));
        }
        let clicks = input["clicks"].as_i64().unwrap_or(1).clamp(1, 3);
        self.click_at(page, x, y, button, clicks)?;
        self.settle(navs);
        let how = match (button, clicks) {
            ("left", 1) => "Clicked".to_string(),
            ("left", 2) => "Double-clicked".to_string(),
            ("left", 3) => "Triple-clicked".to_string(),
            (b, 1) => format!("{}-clicked", capitalized(b)),
            (b, n) => format!("{}-clicked {n} times", capitalized(b)),
        };
        Ok(self.report(&format!("{how} {what}.")))
    }

    /// BROWSER hover: the pointer rests on a ref or a point (menus and
    /// tooltips that open on hover).
    fn hover(&self, page: &Cdp, input: &Value, navs: u64) -> Result<String, String> {
        let (x, y, what) = self.target(page, input, "ref", "x", "y")?;
        self.agent_move(page, x, y, 0, GLIDE)?;
        self.settle(navs);
        Ok(self.report(&format!("The pointer is over {what}.")))
    }

    /// BROWSER drag: the left button pressed on one ref or point, moved in
    /// steps to another, and let go there (sliders, sortable lists, drag
    /// and drop, selecting text).
    fn drag(&self, page: &Cdp, input: &Value, navs: u64) -> Result<String, String> {
        let (_, _, from) = self.target(page, input, "ref", "x", "y")?;
        let (x1, y1, to) = self.target(page, input, "to_ref", "to_x", "to_y")?;
        // Bringing the end into view may have moved the start: again (it
        // only scrolls when out of view).
        let (x0, y0, _) = self.target(page, input, "ref", "x", "y")?;
        self.agent_move(page, x0, y0, 0, GLIDE)?;
        page.send("Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": x0, "y": y0, "button": "left", "buttons": 1, "clickCount": 1}));
        std::thread::sleep(Duration::from_millis(60));
        self.update_view(|v| v.agent_ptr = Some(AgentPointer { from: egui::pos2(x0 as f32, y0 as f32), to: egui::pos2(x1 as f32, y1 as f32), at: Instant::now(), glide: DRAG_TIME }));
        for i in 1..=DRAG_MOVES {
            let k = f64::from(i) / f64::from(DRAG_MOVES);
            let (x, y) = (x0 + (x1 - x0) * k, y0 + (y1 - y0) * k);
            page.send("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y, "button": "left", "buttons": 1}));
            std::thread::sleep(DRAG_TIME / DRAG_MOVES);
        }
        page.send("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": x1, "y": y1, "button": "left", "buttons": 0, "clickCount": 1}));
        self.settle(navs);
        Ok(self.report(&format!("Dragged from {from} to {to}.")))
    }

    /// BROWSER wait: until `text` shows on the page (or, with `gone`, no
    /// longer does), or without it until the page has loaded; at most
    /// `timeout` seconds.
    fn wait(&self, page: &Cdp, input: &Value, navs: u64) -> Result<String, String> {
        let timeout = Duration::from_secs(input["timeout"].as_u64().unwrap_or(10).clamp(1, WAIT_MAX_S));
        let t = Instant::now();
        let Some(text) = input["text"].as_str().filter(|s| !s.is_empty()) else {
            while self.view.lock().unwrap().loading && t.elapsed() < timeout && self.dialog_open().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
            self.settle(navs);
            return Ok(self.report(if self.view.lock().unwrap().loading { "Still loading." } else { "The page has loaded." }));
        };
        let gone = input["gone"].as_bool().unwrap_or(false);
        let js = WAIT_JS.replace("__TEXT__", &serde_json::to_string(text).unwrap_or_default());
        loop {
            if let Some(d) = self.dialog_open() {
                return Err(d);
            }
            // (A page between documents has no body for a moment: not there.)
            let there = self.eval(page, &js, Duration::from_secs(5)).ok().and_then(|v| v.as_bool()).unwrap_or(false);
            if there != gone {
                let done = if gone { format!("{text:?} is gone, after {:.1}s.", t.elapsed().as_secs_f32()) } else { format!("{text:?} is on the page, after {:.1}s.", t.elapsed().as_secs_f32()) };
                return Ok(self.report(&done));
            }
            if t.elapsed() >= timeout {
                return Err(format!("{text:?} {} after {}s.", if gone { "is still on the page" } else { "didn't show on the page" }, timeout.as_secs()) + &self.notes());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// BROWSER type: text into the focused element, or into a ref (which
    /// for a drop-down picks the option).
    fn type_text(&self, page: &Cdp, input: &Value, navs: u64) -> Result<String, String> {
        let text = input["text"].as_str().ok_or("Missing `text`.")?;
        let mut target = String::new();
        if let Some(n) = ref_of(input) {
            let js = script(FOCUS_JS).replace("__N__", &n.to_string()).replace("__TEXT__", &serde_json::to_string(text).unwrap_or_default());
            let r = self.eval(page, &js, CALL_TIMEOUT)?;
            if let Some(e) = r["error"].as_str() {
                return Err(e.to_string());
            }
            if r["picked"] == true {
                self.settle(navs);
                return Ok(self.report(&format!("Picked {text:?} in [{n} {}].", r["desc"].as_str().unwrap_or(""))));
            }
            target = format!(" into [{n}]");
        }
        page.call("Input.insertText", json!({"text": text})).map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(100));
        let now = self.eval(page, &script(FOCUSED_JS), CALL_TIMEOUT).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        self.settle(navs);
        let now = if now.is_empty() { " (nothing had the focus, so it may have gone nowhere)".to_string() } else { format!("; the focus is {now}") };
        Ok(self.report(&format!("Typed {} characters{target}{now}.", text.chars().count())))
    }

    /// BROWSER upload: sets files on a file input.
    fn upload(&self, page: &Cdp, input: &Value, folder: &Path, navs: u64) -> Result<String, String> {
        let n = ref_of(input).ok_or("Give `ref`: the file input's number from read.")?;
        let paths: Vec<PathBuf> = input["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).map(|p| crate::tools::resolve(folder, p.trim())).collect();
        if paths.is_empty() {
            return Err("Give `paths`: the files to set.".into());
        }
        if let Some(missing) = paths.iter().find(|p| !p.is_file()) {
            return Err(format!("No file {}.", missing.display()));
        }
        let r = page
            .call("Runtime.evaluate", json!({"expression": script(FILE_INPUT_JS).replace("__N__", &n.to_string())}))
            .map_err(|e| e.to_string())?;
        if let Some(e) = r.get("exceptionDetails") {
            let text = e["exception"]["description"].as_str().unwrap_or("error");
            return Err(text.strip_prefix("Error: ").unwrap_or(text).lines().next().unwrap_or("").to_string());
        }
        let files: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        page.call("DOM.setFileInputFiles", json!({"files": files, "objectId": r["result"]["objectId"]})).map_err(|e| e.to_string())?;
        self.settle(navs);
        Ok(self.report(&format!("Set {} file{} on [{n}].", files.len(), crate::plural(files.len()))))
    }

    /// The viewport as the model sees it: CSS pixels, so its points are
    /// those `click` takes.
    fn screenshot(&self) -> Result<Vec<Value>, String> {
        let page = self.ready()?;
        if let Some(d) = self.dialog_open() {
            return Err(d);
        }
        let r = page.call_until("Page.captureScreenshot", json!({"format": "png"}), CALL_TIMEOUT, &|| self.dialog_open()).map_err(|e| e.to_string())?;
        let bytes = B64.decode(r["data"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
        let (w, h) = self.viewport_size();
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).map_err(|e| e.to_string())?;
        let png = if img.width() != w || img.height() != h {
            let small = img.resize_exact(w, h, image::imageops::FilterType::Triangle);
            let mut out = std::io::Cursor::new(vec![]);
            small.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
            out.into_inner()
        } else {
            bytes
        };
        let img = crate::images::from_bytes(&png)?;
        let note = self.report(&format!("Screenshot of the viewport, {w}x{h} CSS px (click x,y are in these pixels{}).", if img.width != w { format!("; sent at {}x{}", img.width, img.height) } else { String::new() }));
        Ok(vec![json!({"type": "text", "text": note}), img.block()])
    }
}

/// BROWSER open for conversation `conv`: starts its browser (the view fades
/// it in), at `width` x `height` if given, and goes to `url` if given, once
/// it is ready.
/// With `profile`, on that named profile (if the browser is open on another,
/// it ends and starts again on this one); without, on the one it is open
/// on, else the conversation's own.
pub(crate) fn model_open(conv: &str, ctx: &egui::Context, input: &Value, folder: &Path) -> Result<String, String> {
    let named = match input["profile"].as_str().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Some(state::profile_name(p)?),
        None => None,
    };
    let mut was_open = is_open(conv);
    let mut switched = None;
    if let Some(name) = &named
        && let Some(cur) = get(conv)
        && cur.profile_name.as_ref() != Some(name)
    {
        // Another conversation's browser on it: refused before this one goes.
        if OPEN.lock().unwrap().values().any(|i| i.profile == state::named_dir(name)) {
            return Err(format!("The profile {name:?} is in use by another conversation's browser: close it there first, or open another profile."));
        }
        OPEN.lock().unwrap().remove(conv);
        switched = Some((cur.profile_name.clone(), *cur.viewport.lock().unwrap()));
        end(cur);
        was_open = false;
    }
    let inst = match (&named, get(conv)) {
        (_, Some(i)) => i,
        (Some(n), None) => open_with(conv, ctx, state::named_dir(n), named.clone(), Default::default())?,
        (None, None) => open(conv, ctx),
    };
    let _busy = Busy::new(&inst);
    let mut opened = match &inst.profile_name {
        Some(n) => format!("Opened the browser on profile {n:?}."),
        None => "Opened the browser.".to_string(),
    };
    if let Some((from, vp)) = switched {
        inst.resize(vp.w, vp.h, vp.mobile);
        let from = from.map_or("the conversation's own profile".to_string(), |f| format!("profile {f:?}"));
        opened = format!("Closed the browser on {from}. {opened}");
    }
    if was_open {
        opened = match &inst.profile_name {
            Some(n) => format!("It was open already, on profile {n:?}."),
            None => "It was open already.".to_string(),
        };
    }
    if input["width"].is_u64() || input["height"].is_u64() || input["mobile"].is_boolean() {
        let (w, h) = inst.viewport_size();
        let (w, h) = (input["width"].as_u64().map_or(w, |v| v as u32), input["height"].as_u64().map_or(h, |v| v as u32));
        inst.resize(w, h, input["mobile"].as_bool().unwrap_or(false));
    }
    inst.ready()?;
    if input["url"].as_str().is_some_and(|u| !u.trim().is_empty()) {
        let before = if opened == "Opened the browser." { String::new() } else { format!("{opened} ") };
        return inst.call("BROWSER_GOTO", input, folder).map(|r| format!("{before}{r}"));
    }
    let (w, h) = inst.viewport_size();
    Ok(inst.report(&format!("{opened} The viewport is {w}x{h} CSS px.")))
}

/// BROWSER close for conversation `conv`: its browser ends (the view fades
/// it out).
pub(crate) fn model_close(conv: &str) -> Result<String, String> {
    if !is_open(conv) {
        return Ok("The browser was closed already.".into());
    }
    close(conv);
    Ok("Closed the browser.".into())
}

/// Runs a BROWSER call (but open, close and screenshot) for conversation `conv`.
pub(crate) fn tool(conv: &str, name: &str, input: &Value, folder: &Path) -> Result<String, String> {
    let inst = get(conv).ok_or(CLOSED)?;
    let _busy = Busy::new(&inst);
    inst.call(name, input, folder)
}

/// BROWSER screenshot for conversation `conv`.
pub(crate) fn screenshot(conv: &str) -> Result<Vec<Value>, String> {
    let inst = get(conv).ok_or(CLOSED)?;
    let _busy = Busy::new(&inst);
    inst.screenshot()
}

/// Counts a call of the model's as running, for the view, while it lives.
struct Busy(Arc<Instance>);

impl Busy {
    fn new(i: &Arc<Instance>) -> Busy {
        i.agent.fetch_add(1, Ordering::SeqCst);
        i.ctx.request_repaint();
        Busy(i.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.agent.fetch_sub(1, Ordering::SeqCst);
        self.0.ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(address("example.com", true), "https://example.com");
        assert_eq!(address("  https://x.org/a b ", true), "https://x.org/a b");
        assert_eq!(address("localhost:3000/x", true), "http://localhost:3000/x");
        assert_eq!(address("127.0.0.1:8080", true), "http://127.0.0.1:8080");
        assert_eq!(address("about:blank", true), "about:blank");
        assert_eq!(address("rust egui panel", true), "https://duckduckgo.com/?q=rust+egui+panel");
        assert_eq!(address("a&b", true), "https://duckduckgo.com/?q=a%26b");
        assert_eq!(address("intranet", false), "https://intranet");
        assert_eq!(address("", true), "about:blank");
    }

    #[test]
    fn keys() {
        let (k, m) = parse_combo("Enter").unwrap();
        assert_eq!((k.key.as_str(), k.vk, m), ("Enter", 13, 0));
        let (k, m) = parse_combo("Control+a").unwrap();
        assert_eq!((k.key.as_str(), k.code.as_str(), k.vk, m), ("a", "KeyA", 65, 2));
        let (k, m) = parse_combo("Shift+Tab").unwrap();
        assert_eq!((k.key.as_str(), m), ("Tab", 8));
        let (k, _) = parse_combo("Shift+a").unwrap();
        assert_eq!(k.text.as_deref(), Some("A"));
        let (k, m) = parse_combo("Control++").unwrap();
        assert_eq!((k.key.as_str(), k.code.as_str(), m), ("+", "Equal", 2));
        let (k, _) = parse_combo("?").unwrap();
        assert_eq!((k.key.as_str(), k.code.as_str()), ("?", "Slash"));
        assert_eq!(parse_combo("F5").unwrap().0.vk, 116);
        assert_eq!(parse_combo("ArrowDown").unwrap().0.vk, 40);
        assert!(parse_combo("Hyper+x").is_err());
        assert!(parse_combo("Bogus").is_err());
        // egui's names.
        assert_eq!(key_info("Num7", false).unwrap().code, "Digit7");
        assert_eq!(key_info("Semicolon", true).unwrap().key, ":");
        assert_eq!(key_info("Up", false).unwrap().key, "ArrowUp");
        assert_eq!(key_info("Q", false).unwrap().key, "q");
    }

    /// Resizing during launch must not leave Chromium at the launch size.
    /// Later resizes (the model's) and scale changes (the view's) keep CSS
    /// input coordinates and the viewport in sync too.
    #[test]
    fn viewport_follows_resizes_during_startup_and_after() {
        if find_browser().is_none() {
            eprintln!("no browser: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("oak-browser-viewport-test-{}", std::process::id()));
        let ctx = egui::Context::default();
        let conv = format!("viewport-test-{}", std::process::id());
        let inst = open_with(&conv, &ctx, dir.join("profile"), None, Default::default()).unwrap();
        // start() has taken its initial viewport once the child exists.
        let start = Instant::now();
        while inst.child.lock().unwrap().is_none() {
            assert!(start.elapsed() < START_TIMEOUT, "browser did not launch");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(inst.viewport_css(), (DEFAULT_W, DEFAULT_H));
        inst.resize(600, 900, false);
        inst.shown(1.25, [300, 450]);
        while inst.view.lock().unwrap().status != Status::Ready {
            assert!(start.elapsed() < START_TIMEOUT, "browser did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let page = inst.page().unwrap();
        let check = |w: u32, h: u32, scale: f32| {
            let got = page.call("Runtime.evaluate", json!({
                "expression": "[innerWidth, innerHeight, devicePixelRatio]", "returnByValue": true
            })).unwrap();
            let value = &got["result"]["value"];
            assert_eq!(value[0].as_u64(), Some(u64::from(w)));
            assert_eq!(value[1].as_u64(), Some(u64::from(h)));
            assert_eq!(value[2].as_f64(), Some(f64::from(scale)));
        };
        check(600, 900, 1.25);
        for (w, h, scale) in [(800, 600, 1.0), (450, 750, 1.5), (640, 480, 2.0)] {
            inst.resize(w, h, false);
            inst.shown(scale, [w, h]);
            check(w, h, scale);
        }
        // A CSS point near the bottom right must still hit its DOM target.
        inst.eval(&page, "document.body.innerHTML = `<button style='position:fixed;right:0;bottom:0;width:80px;height:50px' onclick='document.title=\"clicked\"'>Hit</button>`", CALL_TIMEOUT).unwrap();
        inst.mouse("mousePressed", egui::pos2(620.0, 460.0), "left", 1, 1, 0);
        inst.mouse("mouseReleased", egui::pos2(620.0, 460.0), "left", 0, 1, 0);
        // The events are sent without waiting for Chromium to dispatch them,
        // so the click lands a moment later, longer under load.
        let deadline = std::time::Instant::now() + CALL_TIMEOUT;
        let mut title = inst.eval(&page, "document.title", CALL_TIMEOUT).unwrap();
        while title != json!("clicked") && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            title = inst.eval(&page, "document.title", CALL_TIMEOUT).unwrap();
        }
        assert_eq!(title, json!("clicked"));
        OPEN.lock().unwrap().remove(&conv);
        inst.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The page with the things the model's calls act on.
    const PAGE: &str = r#"<title>Test page</title><h1>Hello</h1><p>Some <b>text</b> and a <a href="/next">link</a>.</p>
      <form onsubmit="event.preventDefault(); document.getElementById('out').textContent = 'sent ' + this.q.value">
      <label>Query <input name="q" value="old"></label>
      <select id="s"><option>One</option><option>Two</option></select>
      <button>Go</button></form><p id="out">waiting</p>
      <div style="display:none">hidden words</div>
      <button onclick="confirm('Sure?') ? out.textContent = 'yes' : out.textContent = 'no'">Ask</button>
      <input type="file" id="f">"#;

    /// A whole session against a real browser: read, click, type, key,
    /// select, dialog, eval, upload, screenshot. Skipped without one.
    #[test]
    fn drives_a_real_page() {
        if find_browser().is_none() {
            eprintln!("no browser: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("oak-browser-test-{}", std::process::id()));
        let ctx = egui::Context::default();
        let conv = format!("test-{}", std::process::id());
        let inst = open_with(&conv, &ctx, dir.join("profile"), None, Default::default()).unwrap();
        let url = format!("data:text/html,{PAGE}");
        let folder = dir.clone();
        // The page's text, past READ's header (whose URL line holds the
        // data URL, so the page's source), and the ref of its element
        // whose `[ref kind "label"]` has `what`.
        let page_text = |read: &str| read.split_once("\n\n").map_or(read, |(_, t)| t).to_string();
        let ref_of = |read: &str, what: &str| -> u64 {
            let text = page_text(read);
            text.split('[').find(|s| s.contains(what)).and_then(|s| s.split(' ').next()).and_then(|r| r.parse().ok()).unwrap_or_else(|| panic!("no {what}: {text}"))
        };
        let call = |name: &str, input: Value| {
            let r = tool(&conv, name, &input, &folder);
            println!("== {name} {input}\n{}", match &r { Ok(s) => s.clone(), Err(e) => format!("ERR {e}") });
            r
        };
        call("BROWSER_GOTO", json!({"url": url})).unwrap();
        let read = call("BROWSER_READ", json!({})).unwrap();
        assert!(read.contains("Title: Test page"), "{read}");
        assert!(read.contains("# Hello"));
        assert!(read.contains("[1 link \"link\" → "));
        assert!(read.contains("input \"Query\" value=\"old\""), "{read}");
        assert!(read.contains("select \"\" = \"One\" (options: One | Two)"), "{read}");
        let text = page_text(&read);
        assert!(text.contains("Some text") && !text.contains("hidden words"), "{text}");
        // A select picks an option; typing into a field replaces its value
        // (last, so the field has the focus for Enter: Enter on a select
        // doesn't submit).
        let (input_ref, select_ref) = (ref_of(&read, "input \"Query\""), ref_of(&read, "select "));
        let r = call("BROWSER_TYPE", json!({"ref": select_ref, "text": "two"})).unwrap();
        assert!(r.contains("Picked"));
        call("BROWSER_TYPE", json!({"ref": input_ref, "text": "new words"})).unwrap();
        call("BROWSER_KEY", json!({"key": "Enter"})).unwrap();
        // Enter in the field submitted the form.
        let v = call("BROWSER_EVAL", json!({"script": "document.getElementById('out').textContent + ' ' + document.getElementById('s').value"})).unwrap();
        assert_eq!(v, "sent new words Two");
        // A confirm: the page waits, and the model answers.
        let read = call("BROWSER_READ", json!({})).unwrap();
        let ask = ref_of(&read, "button \"Ask\"");
        let r = call("BROWSER_CLICK", json!({"ref": ask})).unwrap();
        assert!(r.contains("confirm dialog"), "{r}");
        assert!(call("BROWSER_READ", json!({})).unwrap_err().contains("action=dialog"));
        call("BROWSER_DIALOG", json!({"accept": false})).unwrap();
        assert_eq!(call("BROWSER_EVAL", json!({"script": "out.textContent"})).unwrap(), "no");
        // Upload.
        std::fs::write(dir.join("up.txt"), "x").unwrap();
        let read = call("BROWSER_READ", json!({})).unwrap();
        let file = ref_of(&read, "file input");
        call("BROWSER_UPLOAD", json!({"ref": file, "paths": ["up.txt"]})).unwrap();
        assert_eq!(call("BROWSER_EVAL", json!({"script": "document.getElementById('f').files[0].name"})).unwrap(), "up.txt");
        // A click by point, a scroll, a screenshot.
        call("BROWSER_CLICK", json!({"x": 10, "y": 10})).unwrap();
        call("BROWSER_SCROLL", json!({"dy": 100})).unwrap();
        let shot = screenshot(&conv).unwrap();
        assert_eq!(shot[1]["type"], "image");
        // Stale refs say so.
        call("BROWSER_GOTO", json!({"url": "about:blank"})).unwrap();
        assert!(call("BROWSER_CLICK", json!({"ref": 1})).unwrap_err().contains("read"));
        // The user's view followed along.
        assert_eq!(inst.view.lock().unwrap().url, "about:blank");
        // Closed: the calls fail.
        close(&conv);
        assert_eq!(tool(&conv, "BROWSER_READ", &json!({}), &folder).unwrap_err(), CLOSED);
        std::thread::sleep(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The page with what the newer actions act on: a hover menu, a
    /// slider, a box dragged by mouse events, a drag and drop target,
    /// double and right clicks, text that shows later, a far element.
    const PAGE2: &str = r#"<title>Actions</title><style>#menu{display:none}#hov:hover+#menu{display:block}body{margin:0}</style>
      <button id="hov" style="position:absolute;left:20px;top:20px;width:100px;height:30px">Menu</button><div id="menu" style="position:absolute;left:20px;top:52px">Opened menu</div>
      <input id="r" type="range" min="0" max="100" value="0" style="position:absolute;left:200px;top:20px;width:300px">
      <div id="box" style="position:absolute;left:20px;top:120px;width:40px;height:40px;background:red"></div>
      <div id="src" draggable="true" style="position:absolute;left:20px;top:220px;width:60px;height:40px;background:blue">src</div>
      <div id="dst" style="position:absolute;left:300px;top:220px;width:100px;height:60px;background:#eee">dst</div>
      <button id="dbl" style="position:absolute;left:20px;top:320px" ondblclick="out.textContent='double'" oncontextmenu="event.preventDefault();out.textContent='right'">Twice</button>
      <p id="out" style="position:absolute;left:20px;top:360px">none</p>
      <div id="far" style="position:absolute;left:20px;top:3000px">Far away</div>
      <script>
        let drag = null;
        box.onmousedown = e => drag = [e.clientX - box.offsetLeft, e.clientY - box.offsetTop];
        onmousemove = e => { if (drag) { box.style.left = (e.clientX - drag[0]) + 'px'; box.style.top = (e.clientY - drag[1]) + 'px'; } };
        onmouseup = () => drag = null;
        dst.ondragover = e => e.preventDefault();
        dst.ondrop = e => { e.preventDefault(); dst.textContent = 'dropped'; };
        setTimeout(() => { const p = document.createElement('p'); p.textContent = 'Arrived later'; document.body.append(p); }, 800);
      </script>"#;

    /// Hover, drag (a slider, a box moved by mouse events, HTML drag and
    /// drop), double and right clicks, wait, scrolling to a ref, resize and
    /// a phone's viewport, on a real browser. Skipped without one.
    #[test]
    fn newer_actions_on_a_real_page() {
        if find_browser().is_none() {
            eprintln!("no browser: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("oak-browser-actions-test-{}", std::process::id()));
        let ctx = egui::Context::default();
        let conv = format!("actions-test-{}", std::process::id());
        let inst = open_with(&conv, &ctx, dir.join("profile"), None, Default::default()).unwrap();
        let folder = dir.clone();
        let call = |name: &str, input: Value| {
            let r = tool(&conv, name, &input, &folder);
            println!("== {name} {input}\n{}", match &r { Ok(s) => s.clone(), Err(e) => format!("ERR {e}") });
            r
        };
        let eval = |js: &str| call("BROWSER_EVAL", json!({"script": js})).unwrap();
        assert_eq!(inst.viewport_size(), (DEFAULT_W, DEFAULT_H));
        call("BROWSER_GOTO", json!({"url": format!("data:text/html,{}", PAGE2.replace('#', "%23"))})).unwrap();
        // Text that shows later; text that is gone.
        let r = call("BROWSER_WAIT", json!({"text": "Arrived later", "timeout": 5})).unwrap();
        assert!(r.contains("is on the page"), "{r}");
        assert!(call("BROWSER_WAIT", json!({"text": "Arrived later", "gone": true, "timeout": 1})).unwrap_err().contains("still on the page"));
        // Record input delivery as well as CSS state: a failed hover must
        // distinguish a missed input event from layout or style readiness.
        eval("document.addEventListener('mousemove', e => window.lastTestMove = {x:e.clientX, y:e.clientY, target:e.target.id})");
        // Runtime.evaluate can overtake input queued on Chromium's other
        // channels while its renderer is busy. A mouse action must wait for
        // the input acknowledgement, not assume a fixed delay delivers it.
        let page = inst.page().unwrap();
        eval("setTimeout(() => { const end = performance.now() + 600; while (performance.now() < end) {} }, 0); true");
        inst.agent_move(&page, 70.0, 35.0, 0, GLIDE).unwrap();
        assert_eq!(eval("getComputedStyle(menu).display"), "block", "mouse move returned before delivery to a busy renderer");
        inst.agent_move(&page, 700.0, 350.0, 0, GLIDE).unwrap();
        assert_eq!(eval("getComputedStyle(menu).display"), "none");
        // Hover opens the menu, and the model's pointer is there.
        call("BROWSER_HOVER", json!({"x": 70, "y": 35})).unwrap();
        let display = eval("getComputedStyle(menu).display");
        assert_eq!(display, "block", "{}", eval("JSON.stringify({move:window.lastTestMove, hit:document.elementFromPoint(70,35)?.outerHTML, hovered:[...document.querySelectorAll(':hover')].map(e=>e.id||e.tagName), scroll:[scrollX,scrollY], viewport:[innerWidth,innerHeight,devicePixelRatio], focus:document.hasFocus(), browser:navigator.userAgent})"));
        assert_eq!(inst.view.lock().unwrap().agent_ptr.map(|p| p.to), Some(egui::pos2(70.0, 35.0)));
        // Drags: the slider to its end, the box by 200 px, src onto dst.
        call("BROWSER_DRAG", json!({"x": 205, "y": 30, "to_x": 520, "to_y": 30})).unwrap();
        assert_eq!(eval("r.value"), "100");
        call("BROWSER_DRAG", json!({"x": 40, "y": 140, "to_x": 240, "to_y": 160})).unwrap();
        assert_eq!(eval("box.style.left + ' ' + box.style.top"), "220px 140px");
        call("BROWSER_DRAG", json!({"x": 50, "y": 240, "to_x": 350, "to_y": 250})).unwrap();
        let dropped = eval("dst.textContent");
        println!("HTML drag and drop: {dropped}");
        // Double and right clicks.
        call("BROWSER_CLICK", json!({"x": 40, "y": 330, "clicks": 2})).unwrap();
        assert_eq!(eval("out.textContent"), "double");
        call("BROWSER_CLICK", json!({"x": 40, "y": 330, "button": "right"})).unwrap();
        assert_eq!(eval("out.textContent"), "right");
        // Scrolling across, and to a ref far down.
        let read = call("BROWSER_READ", json!({})).unwrap();
        assert!(read.contains("Far away"));
        let r = call("BROWSER_SCROLL", json!({"dx": 0, "dy": 0})).unwrap();
        assert!(r.contains("Scrolled"));
        // Resize, and a phone.
        let r = call("BROWSER_RESIZE", json!({"width": 800, "height": 600})).unwrap();
        assert!(r.contains("800x600"), "{r}");
        assert_eq!(eval("innerWidth + 'x' + innerHeight"), "800x600");
        call("BROWSER_RESIZE", json!({"width": 390, "height": 844, "mobile": true})).unwrap();
        // (Without a viewport meta tag a phone lays the page out 980 px wide, as Chrome on one does.)
        assert_eq!(eval("screen.width + 'x' + screen.height + ' ' + navigator.maxTouchPoints + ' ' + matchMedia('(pointer: coarse)').matches"), "390x844 1 true");
        assert!(call("BROWSER_RESIZE", json!({"width": 50, "height": 99999})).unwrap().contains("200x4096"));
        assert!(dropped == "dropped", "HTML drag and drop didn't drop: {dropped}");
        close(&conv);
        std::thread::sleep(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
