//! Plugins: the dynamic libraries Oak loads when it starts, what they
//! registered, and Oak as they reach it.
//!
//! A plugin is a library (`.so`, `.dll`, `.dylib`) in one of `dirs()`: the
//! `plugins` folder in Oak's folder, the one beside the executable, and
//! the folders `OAK_PLUGINS` names. The interface is plain C
//! (`oak_sdk::abi`, `sdk/include/oak_plugin.h`): `load` opens each
//! library, asks its `oak_plugin` who it is, and calls its `init` with
//! Oak's table of functions (`api`), through which it hands over its parts
//! (`parts`). The rest of Oak asks for those here: `parts()`. Libraries
//! are never unloaded.
//!
//! The table only grows at its end, so a plugin built for an older Oak
//! runs in this one; one built for a newer Oak says so and isn't started.
//!
//! `OAK_PLUGINS=off` (or `--no-plugins`) loads none.

mod api;
mod parts;

use crate::{api as accounts, store};
use oak_sdk::abi::{ENTRY_SYMBOL, OAK_ABI, OAK_API_VERSION, OakInit, OakPlugin};
pub(crate) use parts::{CloudBackend, FontDef, HomeTab, Info, NewKind, Parts, SessionKit, TabKind, Tags, Vault};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// A library Oak looked at.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    /// Who it is, once it said (a library that isn't a plugin never does).
    pub(crate) info: Option<Info>,
    /// Why it isn't running: it doesn't fit, it failed to start, or it
    /// panicked since.
    pub(crate) trouble: Option<String>,
}

static PARTS: OnceLock<Parts> = OnceLock::new();
static FOUND: Mutex<Vec<Found>> = Mutex::new(Vec::new());
static OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// What the plugin being started has handed over so far.
static REGISTERING: Mutex<Option<Parts>> = Mutex::new(None);

/// What the plugins registered (nothing, before `load` or with none).
pub(crate) fn parts() -> &'static Parts {
    static NONE: OnceLock<Parts> = OnceLock::new();
    PARTS.get().unwrap_or_else(|| NONE.get_or_init(Parts::default))
}

/// Takes a part from the plugin being started (`api`'s `add_` functions;
/// outside a plugin's `init` they do nothing).
fn registering(f: impl FnOnce(&mut Parts)) {
    if let Some(parts) = REGISTERING.lock().unwrap().as_mut() {
        f(parts);
    }
}

/// The libraries looked at, in the order they were.
pub(crate) fn found() -> Vec<Found> {
    FOUND.lock().unwrap().clone()
}

/// `--no-plugins`: `load` loads none.
pub fn disable() {
    OFF.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The folders plugins are looked for in.
pub(crate) fn dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Some(list) = std::env::var_os("OAK_PLUGINS") {
        dirs.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs.push(store::dir().join("plugins"));
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("plugins"))) {
        dirs.push(beside);
    }
    let mut seen = vec![];
    dirs.retain(|d| {
        let real = d.canonicalize().unwrap_or_else(|_| d.clone());
        let new = !seen.contains(&real);
        seen.push(real);
        new
    });
    dirs
}

/// Whether `path` is named as a dynamic library is on this platform.
fn is_library(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(std::env::consts::DLL_EXTENSION))
}

/// Opens the library at `path` and asks its plugin who it is. Err: why it
/// can't run in this Oak.
fn open(path: &Path) -> Result<(Info, OakInit), String> {
    // SAFETY: loading a library runs its initializers; that is what the
    // user asked for by putting it in a plugins folder.
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("it couldn't be loaded: {e}"))?;
    // Never unloaded: what it registers and its threads point into it.
    let lib: &'static libloading::Library = Box::leak(Box::new(lib));
    // SAFETY: the symbol's type is the one the interface gives it.
    let entry = unsafe { lib.get::<unsafe extern "C" fn() -> *const OakPlugin>(ENTRY_SYMBOL) }.map_err(|_| "it isn't an Oak plugin (it exports no oak_plugin)".to_string())?;
    // SAFETY: as above.
    let plugin = unsafe { entry() };
    if plugin.is_null() {
        return Err("it isn't an Oak plugin (oak_plugin gave nothing)".into());
    }
    // SAFETY: `size` and `abi` are the first fields of every version of
    // the descriptor, read before the rest is trusted.
    let (size, abi) = unsafe { ((*plugin).size, (*plugin).abi) };
    if abi != OAK_ABI {
        return Err(format!("it was built for another plugin interface ({abi}; this Oak's is {OAK_ABI})"));
    }
    // (What the first version of the descriptor had, up to its `init`.)
    if (size as usize) < std::mem::offset_of!(OakPlugin, init) + size_of::<Option<OakInit>>() {
        return Err("it isn't an Oak plugin (its descriptor is too small)".into());
    }
    // SAFETY: as much of the descriptor as the plugin's build has, from a
    // library that stays loaded.
    let d: OakPlugin = unsafe { oak_sdk::abi::read(plugin) };
    if d.api > OAK_API_VERSION {
        return Err(format!("it needs a newer Oak (it uses version {} of the plugin interface; this Oak has version {OAK_API_VERSION})", d.api));
    }
    let init = d.init.ok_or("it isn't an Oak plugin (it has no init)")?;
    // SAFETY: the descriptor's texts are the plugin's, valid for good.
    let info = unsafe {
        Info {
            id: api::text(d.id),
            name: api::text(d.name),
            version: api::text(d.version),
            about: api::text(d.about),
            needs: api::text(d.needs).split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect(),
        }
    };
    Ok((info, init))
}

/// Loads every plugin and takes what they register. Once, at startup,
/// before the window.
pub fn load() {
    if PARTS.get().is_some() {
        return;
    }
    let off = OFF.load(std::sync::atomic::Ordering::Relaxed) || std::env::var("OAK_PLUGINS").is_ok_and(|v| v == "off" || v == "0");
    let mut found: Vec<Found> = vec![];
    let mut started: Vec<(usize, Info, OakInit)> = vec![];
    for dir in if off { vec![] } else { dirs() } {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_library(p)).collect();
        files.sort();
        for path in files {
            let at = found.len();
            match open(&path) {
                Ok((info, init)) => {
                    let trouble = if info.id.is_empty() {
                        Some("it has no id".to_string())
                    } else if started.iter().any(|(_, i, _)| i.id == info.id) {
                        Some(format!("a plugin with the id {} is loaded already", info.id))
                    } else {
                        None
                    };
                    found.push(Found { path, info: Some(info.clone()), trouble: trouble.clone() });
                    if trouble.is_none() {
                        started.push((at, info, init));
                    }
                }
                Err(why) => found.push(Found { path, info: None, trouble: Some(why) }),
            }
        }
    }
    *FOUND.lock().unwrap() = found;
    // Those whose needs are met start, the needed first; the rest say
    // what they miss.
    let mut all = Parts::default();
    let mut done: Vec<String> = vec![];
    while let Some(i) = started.iter().position(|(_, info, _)| info.needs.iter().all(|n| done.contains(n))) {
        let (at, info, init) = started.remove(i);
        *REGISTERING.lock().unwrap() = Some(Parts::default());
        // SAFETY: the plugin's `init`, with the table it was built for (or
        // a later one, which starts the same).
        let ok = unsafe { init(&api::TABLE) };
        let parts = REGISTERING.lock().unwrap().take().unwrap_or_default();
        let mut found = FOUND.lock().unwrap();
        if ok && found[at].trouble.is_none() {
            all.absorb(parts);
            done.push(info.id.clone());
        } else if found[at].trouble.is_none() {
            // (One that panicked said why already: `failed`.)
            found[at].trouble = Some("it failed to start".into());
        }
    }
    let mut found = FOUND.lock().unwrap();
    for (at, info, _) in started {
        let missing: Vec<&str> = info.needs.iter().filter(|n| !done.contains(n)).map(|s| s.as_str()).collect();
        found[at].trouble = Some(format!("it needs the plugin{} {}, which {} loaded", oak_sdk::plural(missing.len()), missing.join(", "), if missing.len() == 1 { "isn't" } else { "aren't" }));
    }
    drop(found);
    let _ = PARTS.set(all);
    crate::agent::prompt::add_roles(&parts().roles);
}

/// `oak --plugins`: says where plugins are looked for, which were found
/// and which of them run. 1 when one doesn't.
pub fn report() -> i32 {
    println!("Oak {} (plugin interface {OAK_API_VERSION}) looks for plugins in:", crate::update::VERSION);
    for d in dirs() {
        println!("  {}{}", d.display(), if d.is_dir() { "" } else { "  (not there)" });
    }
    let found = found();
    if found.is_empty() {
        println!("None found.");
    }
    for f in &found {
        let who = f.info.as_ref().map_or_else(|| "?".to_string(), |i| format!("{} {} ({})", name_of(i), i.version, i.id));
        match &f.trouble {
            None => println!("ok       {who}\n         {}", f.path.display()),
            Some(why) => println!("NOT RUN  {who}\n         {}\n         {why}", f.path.display()),
        }
    }
    let p = parts();
    println!(
        "Registered: {} Home tab{}, {} tab kind{}, {} New kind{}, {} cloud backend{}, {} session kit{}, {} theme{}, {} font{}, {} role{}, {} settings section{}, {} gate{}, {} hook set{}, {}.",
        p.home_tabs.len(), oak_sdk::plural(p.home_tabs.len()),
        p.tab_kinds.len(), oak_sdk::plural(p.tab_kinds.len()),
        p.new_kinds.len(), oak_sdk::plural(p.new_kinds.len()),
        p.cloud_backends.len(), oak_sdk::plural(p.cloud_backends.len()),
        p.session_kits.len(), oak_sdk::plural(p.session_kits.len()),
        p.themes.len(), oak_sdk::plural(p.themes.len()),
        p.fonts.len(), oak_sdk::plural(p.fonts.len()),
        p.roles.len(), oak_sdk::plural(p.roles.len()),
        p.settings.len(), oak_sdk::plural(p.settings.len()),
        p.gates.len(), oak_sdk::plural(p.gates.len()),
        p.hooks.len(), oak_sdk::plural(p.hooks.len()),
        if p.vault.is_some() { "a vault" } else { "no vault" },
    );
    found.iter().any(|f| f.trouble.is_some()) as i32
}

/// A plugin's name, as Settings and `--plugins` show it.
fn name_of(info: &Info) -> String {
    if info.name.is_empty() { info.id.clone() } else { info.name.clone() }
}

/// The plugin `id` can't go on (it panicked, and told Oak): Settings says
/// so. Its own code stops answering; Oak keeps running.
fn failed(id: &str, why: &str) {
    let line = format!("it stopped: {why} (crash.log in Oak's folder has more)");
    if let Some(f) = FOUND.lock().unwrap().iter_mut().find(|f| f.info.as_ref().is_some_and(|i| i.id == id)) {
        f.trouble = Some(line);
    }
    if let Some(ctx) = context() {
        ctx.request_repaint();
    }
}

/// The window's context (set when the window is up), for plugins' threads.
static CONTEXT: OnceLock<eframe::egui::Context> = OnceLock::new();

thread_local! {
    /// The context of the frame being drawn on this thread.
    static FRAME: std::cell::RefCell<Option<eframe::egui::Context>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn context() -> Option<eframe::egui::Context> {
    FRAME.with(|f| f.borrow().clone()).or_else(|| CONTEXT.get().cloned())
}

/// A frame starts in `ctx`, drawn with `theme`: what plugins draw until
/// the next goes there.
pub(crate) fn begin_frame(ctx: &eframe::egui::Context, theme: &crate::theme::Theme) {
    FRAME.with(|f| *f.borrow_mut() = Some(ctx.clone()));
    api::begin_frame(theme);
}

/// The window is up: plugins' `started` hooks run.
pub(crate) fn started(ctx: &eframe::egui::Context) {
    let _ = CONTEXT.set(ctx.clone());
    for h in &parts().hooks {
        h.started(ctx);
    }
}

/// Oak is closing.
pub(crate) fn exiting() {
    for h in &parts().hooks {
        h.exiting();
    }
}

/// The plugin tab kind that owns `key`, if one does.
pub(crate) fn tab_kind(key: &str) -> Option<&'static Arc<TabKind>> {
    parts().tab_kinds.iter().find(|k| k.owns(key))
}

/// The vault, when a plugin brought one.
pub(crate) fn vault() -> Option<&'static Arc<Vault>> {
    parts().vault.as_ref()
}

/// What plugins tell each other: a value under a name.
static SHARED: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

fn shared(name: &str) -> Option<Value> {
    SHARED.lock().unwrap().as_ref().and_then(|m| m.get(name).cloned())
}

fn share(name: &str, value: Option<Value>) {
    let mut shared = SHARED.lock().unwrap();
    let map = shared.get_or_insert_with(HashMap::new);
    match value {
        Some(v) => {
            map.insert(name.to_string(), v);
        }
        None => {
            map.remove(name);
        }
    }
}

/// An HTTP answer: its status and headers, and its body as it arrives
/// (an event stream is read line by line while it is open). Oak's own
/// requests and the ones plugins' parts answer are both this.
pub(crate) struct Response {
    pub(crate) status: u16,
    /// Names in lower case.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Box<dyn Read + Send>,
}

impl Response {
    /// 200 to 299.
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The whole body.
    pub(crate) fn bytes(mut self) -> Result<Vec<u8>, String> {
        let mut out = vec![];
        self.body.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// The whole body, as text (what isn't UTF-8 replaced).
    pub(crate) fn text(self) -> Result<String, String> {
        self.bytes().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// The whole body, as JSON.
    pub(crate) fn json(self) -> Result<Value, String> {
        serde_json::from_slice(&self.bytes()?).map_err(|e| e.to_string())
    }
}

/// A reqwest answer as a `Response`: its body is read as it arrives.
pub(crate) fn response(resp: reqwest::blocking::Response) -> Response {
    let headers = resp.headers().iter().map(|(n, v)| (n.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
    Response { status: resp.status().as_u16(), headers, body: Box::new(resp) }
}

/// The answers plugins hold, by their handles. Each has a lock of its
/// own: a read that waits for more of a stream holds only that.
static ANSWERS: Mutex<Option<HashMap<u64, Arc<Mutex<Response>>>>> = Mutex::new(None);
static NEXT_ANSWER: AtomicU64 = AtomicU64::new(1);

/// Keeps an answer for a plugin; its handle.
fn keep_answer(answer: Response) -> u64 {
    let handle = NEXT_ANSWER.fetch_add(1, Ordering::Relaxed);
    ANSWERS.lock().unwrap().get_or_insert_with(HashMap::new).insert(handle, Arc::new(Mutex::new(answer)));
    handle
}

/// Works with the answer under `handle`.
fn with_answer<R>(handle: u64, f: impl FnOnce(&mut Response) -> R) -> Option<R> {
    let answer = ANSWERS.lock().unwrap().as_ref().and_then(|all| all.get(&handle).cloned())?;
    let mut answer = answer.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut answer))
}

/// A body another thread is still reading: read through its lock.
struct SharedBody(Arc<Mutex<Response>>);

impl Read for SharedBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).body.read(buf)
    }
}

/// Takes the answer under `handle` back (a part answered with it, or the
/// plugin closed it).
fn take_answer(handle: u64) -> Option<Response> {
    let answer = ANSWERS.lock().unwrap().as_mut().and_then(|all| all.remove(&handle))?;
    Some(match Arc::try_unwrap(answer) {
        Ok(answer) => answer.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(shared) => {
            let (status, headers) = {
                let a = shared.lock().unwrap_or_else(|e| e.into_inner());
                (a.status, a.headers.clone())
            };
            Response { status, headers, body: Box::new(SharedBody(shared)) }
        }
    })
}

/// Oak's HTTP client for a plugin's request: its own, or one like it that
/// answers a redirect instead of following it (`redirects` false), or
/// goes around the proxy (`direct`).
fn http_client(redirects: bool, direct: bool) -> reqwest::blocking::Client {
    static CLIENTS: [OnceLock<reqwest::blocking::Client>; 4] = [const { OnceLock::new() }; 4];
    if redirects && !direct {
        return accounts::http();
    }
    CLIENTS[redirects as usize * 2 + direct as usize]
        .get_or_init(|| {
            let mut b = reqwest::blocking::Client::builder()
                .timeout(None)
                .connect_timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(60))
                .tcp_keepalive(Duration::from_secs(30));
            if !redirects {
                b = b.redirect(reqwest::redirect::Policy::none());
            }
            if direct {
                b = b.no_proxy();
            }
            b.build().unwrap()
        })
        .clone()
}

/// Sends a plugin's request with Oak's client. Err: it couldn't be sent or
/// no answer came; an answer with an error status is Ok.
fn http(method: &str, url: &str, headers: &[(String, String)], body: &[u8], timeout: Duration, redirects: bool, direct: bool) -> Result<Response, String> {
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| format!("{method} isn't an HTTP method"))?;
    let client = http_client(redirects, direct);
    let build = || {
        let mut req = client.request(method.clone(), url).timeout(timeout);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        if body.is_empty() { req } else { req.body(body.to_vec()) }
    };
    crate::provider::send(build).map(response).map_err(|e| accounts::error_chain(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{Headless, click, painted, rect_of};
    use crate::{Page, Screen};
    use eframe::egui::PointerButton::Primary;

    /// Builds the C example (`plugins/hello-c`) with the system's C
    /// compiler (`CC`, else `cc`) into a scratch folder; the folder.
    fn built_c_example() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = std::env::temp_dir().join(format!("oak-hello-c-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let lib = out.join(format!("liboak_hello_c.{}", std::env::consts::DLL_EXTENSION));
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        // (Windows has no use for -fPIC, and its compilers warn about it.)
        let pic: &[&str] = if cfg!(windows) { &[] } else { &["-fPIC"] };
        let done = std::process::Command::new(&cc)
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-shared"])
            .args(pic)
            .arg("-I")
            .arg(root.join("sdk/include"))
            .arg("-o")
            .arg(&lib)
            .arg(root.join("plugins/hello-c/hello.c"))
            .output()
            .unwrap_or_else(|e| panic!("the C compiler {cc} didn't run: {e}"));
        assert!(done.status.success(), "the C example doesn't compile:\n{}", String::from_utf8_lossy(&done.stderr));
        out
    }

    /// Points Oak at the scratch folder `Headless::start(name, ..)` will
    /// use, before the plugins load: what they read as they start (a
    /// saved connection, say) is the scratch folder's, never the user's.
    /// `OAK_PLUGIN_SEED` names a folder whose files are copied there first
    /// (a connection to a test server).
    fn scratch_folder(name: &str) {
        let dir = std::env::temp_dir().join(format!("oak-{name}-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(seed) = std::env::var_os("OAK_PLUGIN_SEED") {
            for file in std::fs::read_dir(&seed).unwrap_or_else(|e| panic!("OAK_PLUGIN_SEED can't be read: {e}")).flatten().filter(|f| f.path().is_file()) {
                std::fs::copy(file.path(), dir.join(file.file_name())).unwrap();
            }
        }
        unsafe { std::env::set_var("OAK_DIR", &dir) };
    }

    /// The example plugins, loaded into the whole app, headless: the Rust
    /// one's tab on Home draws and takes clicks, its theme and its row are
    /// on Settings, its role on the New screen; the C one (built here from
    /// the header alone) draws its tab, counts clicks and takes typing.
    /// Needs the Rust plugin built, and no other test in the process
    /// (plugins are loaded once, for all of it):
    /// `cargo build -p oak-hello && OAK_PLUGINS=target/debug cargo test --lib the_example_plugins -- --ignored`
    #[test]
    #[ignore]
    fn the_example_plugins_draw_in_the_window() {
        let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built example plugin");
        // (Absolute: the scratch OAK folder moves nothing, but be sure.)
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
        let c_dir = built_c_example();
        dirs.push(c_dir.clone());
        unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
        scratch_folder("plugin");
        load();
        let found = found();
        let hello = found.iter().find(|f| f.info.as_ref().is_some_and(|i| i.id == "hello")).unwrap_or_else(|| panic!("the example plugin wasn't found: {found:?}"));
        assert_eq!(hello.trouble, None);
        assert_eq!(crate::agent::prompt::role("haiku").name, "Haiku");

        let (dir, mut h, mut app) = Headless::start("plugin", &[]);
        let ctx = h.ctx.clone();
        let mut frames = |app: &mut crate::App, events: Vec<eframe::egui::Event>| {
            let mut out = h.events(app, events);
            for _ in 0..2 {
                out = h.events(app, vec![]);
            }
            painted(&out)
        };
        // Home has its tab; picked, the plugin draws the cell.
        app.page = Page::Home;
        let shown = frames(&mut app, vec![]);
        let tab = rect_of(&shown, "Hello");
        let shown = frames(&mut app, click(tab.center(), Primary));
        assert_eq!(app.home_list, "hello");
        let texts = |shown: &[(String, eframe::egui::Rect)]| shown.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().join("|");
        assert!(texts(&shown).contains("Hello from a plugin") && texts(&shown).contains("clicked 0 times"), "{}", texts(&shown));
        // A click in it reaches the plugin's widget.
        let shown = frames(&mut app, click(rect_of(&shown, "Count").center(), Primary));
        assert!(texts(&shown).contains("clicked 1 times"), "{}", texts(&shown));
        // What it does to the window goes through `app`.
        let shown = frames(&mut app, click(rect_of(&shown, "New conversation").center(), Primary));
        assert!(matches!(app.screen, Screen::New) && app.page == Page::Chats);
        assert!(texts(&shown).contains("Haiku"), "its role is offered: {}", texts(&shown));

        // Settings: its theme, its row, and itself among the plugins.
        app.page = Page::Settings;
        let shown = frames(&mut app, vec![]);
        let all = texts(&shown);
        for want in ["Moss", "A row the example plugin adds.", "Hello 0.1.0"] {
            assert!(all.contains(want), "{want} isn't on Settings: {all}");
        }
        let before = ctx.global_style().visuals.panel_fill;
        frames(&mut app, click(rect_of(&shown, "Moss").center(), Primary));
        assert_eq!(app.settings.theme, "moss");
        let after = ctx.global_style().visuals.panel_fill;
        assert_ne!(before, after, "the theme is applied");
        // Its row's button goes to its tab.
        app.home_list = String::new();
        let shown = frames(&mut app, vec![]);
        frames(&mut app, click(rect_of(&shown, "Open its tab").center(), Primary));
        assert!(app.page == Page::Home && app.home_list == "hello");

        // The C plugin: found, running, and drawing through the same table.
        let c = found.iter().find(|f| f.info.as_ref().is_some_and(|i| i.id == "hello-c")).unwrap_or_else(|| panic!("the C example wasn't found: {found:?}"));
        assert_eq!(c.trouble, None);
        assert_eq!(c.info.as_ref().unwrap().name, "Hello C");
        let shown = frames(&mut app, vec![]);
        let shown = frames(&mut app, click(rect_of(&shown, "Hello C").center(), Primary));
        assert_eq!(app.home_list, "hello-c");
        assert!(texts(&shown).contains("Hello, world, from C") && texts(&shown).contains("clicked 0 times in C"), "{}", texts(&shown));
        let shown = frames(&mut app, click(rect_of(&shown, "Count in C").center(), Primary));
        assert!(texts(&shown).contains("clicked 1 times in C"), "{}", texts(&shown));
        // Typing in its field reaches its buffer (the field shows "world").
        let field = rect_of(&shown, "world");
        frames(&mut app, click(field.center(), Primary));
        let shown = frames(&mut app, vec![eframe::egui::Event::Text("ly ".into())]);
        assert!(texts(&shown).contains("from C") && !texts(&shown).contains("Hello, world, from C"), "{}", texts(&shown));
        assert_eq!(super::found().iter().filter_map(|f| f.trouble.clone()).collect::<Vec<_>>(), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(c_dir);
    }

    /// Whatever plugins `OAK_PLUGINS` holds, loaded into the whole app,
    /// headless: each loads, and each part that draws does so without
    /// panicking (every Home tab, every theme, Settings, every choice on
    /// the New screen, and the tabs `OAK_PLUGIN_TABS` names, separated by
    /// commas), in a scratch Oak folder (`scratch_folder`). For a plugin's
    /// own repository to run against its build:
    /// `OAK_PLUGINS=<folder> cargo test --lib every_plugin_draws -- --ignored`
    #[test]
    #[ignore]
    fn every_plugin_draws() {
        let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built plugins");
        let dirs: Vec<PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
        unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
        scratch_folder("plugins");
        load();
        let troubles = || found().iter().filter_map(|f| f.trouble.as_ref().map(|t| format!("{}: {t}", f.path.display()))).collect::<Vec<_>>();
        assert!(!found().is_empty(), "no plugin was found");
        assert_eq!(troubles(), Vec::<String>::new());

        let (dir, mut h, mut app) = Headless::start("plugins", &[]);
        let ctx = h.ctx.clone();
        // `OAK_PLUGIN_WAIT_MS`: how long each view is given to fetch what it
        // shows (plugins that talk to a server); `OAK_PLUGIN_SHOW=1` prints
        // the text each view drew.
        let wait = std::env::var("OAK_PLUGIN_WAIT_MS").ok().and_then(|ms| ms.parse().ok()).unwrap_or(0);
        let show = std::env::var_os("OAK_PLUGIN_SHOW").is_some();
        let mut frames = |app: &mut crate::App, what: &str| {
            let mut out = h.events(app, vec![]);
            for round in 0..2 {
                if round == 1 && wait > 0 {
                    std::thread::sleep(Duration::from_millis(wait));
                }
                for _ in 0..3 {
                    out = h.events(app, vec![]);
                }
            }
            if show {
                let texts: Vec<String> = painted(&out).into_iter().map(|(s, _)| s.replace('\n', " ")).filter(|s| !s.trim().is_empty()).collect();
                println!("== {what}\n{}", texts.join(" | "));
            }
            assert_eq!(troubles(), Vec::<String>::new(), "after drawing {what}");
        };
        let p = parts();
        frames(&mut app, "the first screen");
        for theme in std::iter::once(crate::theme::OAK.to_string()).chain(p.themes.iter().map(|t| t.id())).rev() {
            app.settings.theme = theme.clone();
            crate::theme::apply_theme(&ctx, &crate::theme::theme(&theme));
            for tab in &p.home_tabs {
                app.page = Page::Home;
                app.home_list = tab.id();
                frames(&mut app, &format!("Home's {} tab in the {theme} theme", tab.id()));
            }
        }
        app.page = Page::Settings;
        frames(&mut app, "Settings");
        let kinds = p.new_kinds.iter().map(|k| k.id()).chain(p.cloud_backends.iter().map(|b| b.id()));
        for kind in kinds {
            app.plugin_go_new(&kind);
            frames(&mut app, &format!("the New screen's {kind}"));
        }
        for key in std::env::var("OAK_PLUGIN_TABS").unwrap_or_default().split(',').map(str::trim).filter(|k| !k.is_empty()) {
            assert!(tab_kind(key).is_some(), "no plugin has the tab {key}");
            app.open(key.to_string());
            assert!(matches!(&app.screen, Screen::Chat(k) if k == key), "the tab {key} didn't open");
            frames(&mut app, &format!("the tab {key}"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
