//! A plugin and its parts.
//!
//! A plugin implements `Plugin`: who it is (`info`) and what it adds to
//! Oak (`register`, called once when Oak starts, before the window shows).
//! Each part is a trait, implemented by a type of the plugin's and handed
//! to the `Registrar`:
//!
//! - `HomeTab`: a tab in Home's left cell;
//! - `TabKind`: tabs of the plugin's own, by their keys: what they show,
//!   where their messages go, their box in the corner;
//! - `NewKind`: something else the New screen can start;
//! - `CloudBackend`: another service cloud sessions run through, and
//!   `SessionKit`, what a backend's sessions are given (a CLI, files);
//! - `ThemeDef`, `FontDef`: a theme, a font, for Settings to offer;
//! - `Role`: a system prompt;
//! - `Vault`: somewhere else logins and keys are kept;
//! - `SettingsSection`: rows on the Settings page;
//! - `Gate`: a screen that comes before everything (signing in somewhere);
//! - `Hooks`: at startup, every frame, at exit.
//!
//! Parts are `Send + Sync` and take `&self`: a plugin keeps what changes
//! behind its own locks (or in statics; the library is never unloaded).
//! What draws gets the `App`; the rest of Oak is `crate::host()`.
//!
//! Underneath, each part is one of `abi`'s descriptors: the `Registrar`
//! makes it, with functions that turn Oak's calls into the trait's
//! methods. A panic in one stops there (`rt::outer`): Oak is told, and the
//! plugin's parts answer with nothing from then on.

use crate::abi::*;
use crate::cloud::{Account, Call};
use crate::egui::{self, Rect, Ui};
use crate::host::{App, Response, Window};
use crate::rt::{api, bytes_of, give, outer, s, str_of};
use crate::theme::Palette;
use serde_json::{Value, json};
use std::ffi::c_void;
use std::time::Duration;

/// Who a plugin is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    /// Its id: lower case, no spaces ("team-server"). Two plugins with
    /// one id don't both load.
    pub id: String,
    /// Its name, as Settings shows it.
    pub name: String,
    pub version: String,
    /// A line about what it does.
    pub about: String,
    /// The ids of the plugins it can't do without: they register first,
    /// and it doesn't load when one is missing.
    pub needs: Vec<String>,
}

impl Info {
    pub fn new(id: &str, name: &str, version: &str, about: &str) -> Info {
        Info { id: id.into(), name: name.into(), version: version.into(), about: about.into(), needs: vec![] }
    }

    pub fn needs(mut self, ids: &[&str]) -> Info {
        self.needs = ids.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// A plugin. `export_plugin!` hands Oak one.
pub trait Plugin: Send + Sync {
    fn info(&self) -> Info;
    /// Hands Oak the plugin's parts. Called once, on the main thread,
    /// before the window shows; `crate::host()` works from here on.
    fn register(&self, r: &mut Registrar);
}

/// A tab in the heading of Home's left cell, and what the cell shows
/// under it.
pub trait HomeTab: Send + Sync {
    /// Its id ("notes"): what `App::go_home` takes.
    fn id(&self) -> String;
    fn label(&self) -> String;
    /// A Nerd Font symbol before the label (empty: none).
    fn icon(&self) -> String { String::new() }
    /// The icon is a character of the text font (△), not a symbol.
    fn icon_is_text(&self) -> bool { false }
    /// What hovering it says.
    fn tip(&self) -> String { String::new() }
    /// Where it goes among the tabs: lower first. Oak's Conversations
    /// is 0 and Cloud 10; ties go by label.
    fn order(&self) -> i32 { 100 }
    /// Whether the tab is there at all, asked every frame.
    fn shown(&self) -> bool { true }
    /// Draws the cell: `ui.max_rect()` is all of it.
    fn show(&self, ui: &mut Ui, app: &mut dyn App);
}

/// How a tab looks in the tab strip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabLook {
    pub title: String,
    /// It is working: the tab shows a spinner.
    pub busy: bool,
    /// A Nerd Font symbol before the title (empty: none).
    pub icon: String,
    /// The icon is a character of the text font.
    pub icon_is_text: bool,
}

/// Tabs of the plugin's own. A tab is a key (a string the plugin
/// makes up, "note:7"); Oak keeps the open ones across restarts and
/// asks the first kind that owns a key for everything about it.
pub trait TabKind: Send + Sync {
    /// Whether `key` is one of this kind's.
    fn owns(&self, key: &str) -> bool;
    /// Whether the tab can still be shown (false: Oak closes it).
    fn exists(&self, _key: &str) -> bool { true }
    fn look(&self, key: &str) -> TabLook;
    /// Draws the tab, over the message box: `ui.max_rect()` is the area.
    fn show(&self, ui: &mut Ui, app: &mut dyn App, key: &str);
    /// Whether the tab has the message box under it.
    fn composer(&self, _key: &str) -> bool { true }
    /// The message box's text and attachments were sent in this tab.
    /// Err: why they couldn't go; Oak says it and leaves them in the box.
    fn send(&self, _app: &mut dyn App, _key: &str, _text: String, _files: Vec<Value>) -> Result<(), String> {
        Err("This tab takes no messages.".to_string())
    }
    /// Draws the tab's box in the top left corner, over the transcript
    /// (`ui::corner_width`, `ui::corner_area`, `ui::panel_box`).
    fn corner(&self, _ctx: &egui::Context, _app: &mut dyn App, _key: &str, _area: Rect) {}
}

/// Something the New screen can start besides a conversation here and
/// a cloud session: a choice in its "where" row.
pub trait NewKind: Send + Sync {
    /// Its id: what `App::go_new` takes.
    fn id(&self) -> String;
    /// The choice's label ("Notebook").
    fn label(&self) -> String;
    fn icon(&self) -> String { String::new() }
    fn tip(&self) -> String { String::new() }
    /// The screen's heading while it is picked ("New notebook").
    fn heading(&self) -> String;
    /// Draws its rows, under the "where" row.
    fn form(&self, ui: &mut Ui, app: &mut dyn App);
    /// The message box was sent with this picked: starts it, with
    /// that as its first message. Err: why not; Oak says it and leaves
    /// the message in the box.
    fn start(&self, app: &mut dyn App, text: String, files: Vec<Value>) -> Result<(), String>;
}

/// The tags a backend's sessions' first messages are wrapped in, which the
/// transcript hides again: `<x-setup>`, `<x-files>`, `<x-continued>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Tags {
    pub setup: (String, String),
    pub files: (String, String),
    pub carry: (String, String),
}

impl Tags {
    /// `<name-setup>`, `<name-files>` and `<name-continued>`.
    pub fn of(name: &str) -> Tags {
        let pair = |what: &str| (format!("<{name}-{what}>"), format!("</{name}-{what}>"));
        Tags { setup: pair("setup"), files: pair("files"), carry: pair("continued") }
    }
}

/// What a backend's cloud sessions are given: a server of the
/// plugin's that a command line tool in the session talks to, which
/// carries files to and from the user and conversations from one
/// account to the next. Without one a session is a bare Claude Code
/// session: no files, and it can't move to another account.
pub trait SessionKit: Send + Sync {
    /// Whether the server can be used now (connected): while not, the
    /// sessions are bare ones, as without a kit.
    fn ready(&self) -> bool { true }
    /// The server's address and the token sessions use it with.
    /// Err: why there is none now ("Not connected to ...").
    fn server(&self) -> Result<(String, String), String>;
    /// What the server is called in sentences to the session and the
    /// user ("the team server").
    fn server_name(&self) -> String;
    /// What the environment is called at Anthropic ("OAK"): a hash of
    /// the script, server and token follows it.
    fn environment_name(&self) -> String;
    /// The environment's setup script (it installs the tool).
    fn setup_script(&self, server: &str) -> String;
    /// The environment's variables, as a JSON object, from the
    /// server's address and token.
    fn variables(&self, server: &str, token: &str) -> Value;
    /// What a new session is told first (in `tags().setup`).
    fn setup_block(&self) -> String;
    /// The tool's name, as the session runs it ("oak").
    fn cli(&self) -> String;
    fn tags(&self) -> Tags;
    /// Other blocks a session's first message may open with, started
    /// by someone else (a scheduled job): (opening tag, closing tag,
    /// the note the transcript shows instead).
    fn notes(&self) -> Vec<(String, String, String)> { vec![] }
    /// A request to the server: `path` from its root, a body with its
    /// content type. Ok: an answer that succeeded; Err names the
    /// status of one that didn't ("HTTP 409"). Oak asks for
    /// `GET /files` (a JSON list of `{name, size, modified}`),
    /// `PUT`, `GET` and `DELETE /files/<name>` (`PUT` answers 409 for
    /// a name in use), and `POST /conversations` (a conversation as
    /// text; the answer's `id` names it for the tool's `resume`).
    fn raw(&self, method: &str, path: &str, body: Option<(Vec<u8>, String)>, timeout: Duration) -> Result<Response, String>;
}

/// Another service cloud sessions run through (Oak's own run on this
/// machine's Claude logins). Oak's engine does the rest: the list, the
/// stream, the transcript, the box in the corner.
pub trait CloudBackend: Send + Sync {
    /// Its id ("acme"): its accounts' ids start with it and "~".
    fn id(&self) -> String;
    /// Its name, as in "Acme session on a@b.c".
    fn name(&self) -> String;
    fn icon(&self) -> String;
    fn icon_is_text(&self) -> bool { false }
    /// What hovering its choice on the New screen says.
    fn tip(&self) -> String { String::new() }
    /// What the New screen says under it: what a session there can do.
    fn about(&self) -> String { String::new() }
    /// Draws on the New screen under that, while it is picked (a form
    /// to connect with, say).
    fn form(&self, _ui: &mut Ui, _app: &mut dyn App) {}
    /// The accounts sessions can run on now.
    fn accounts(&self) -> Vec<Account>;
    /// Why no session can start now (not connected, no accounts);
    /// None when one can.
    fn unavailable(&self) -> Option<String>;
    /// Account `rid` is out of usage until `until`.
    fn note_limit(&self, rid: &str, until: i64);
    /// Sends a sessions API call on account `rid`'s behalf. Ok: any
    /// answer, error statuses too. Its errors mustn't quote the
    /// answer's body or the request's headers.
    fn request(&self, rid: &str, call: Call) -> Result<Response, String>;
    /// What to say about an answer with error status `status`
    /// (None: Oak's own words, which quote the answer).
    fn problem(&self, _status: u16) -> Option<String> { None }
}

/// A theme for Settings to offer beside Oak's.
pub trait ThemeDef: Send + Sync {
    /// Its id, kept in the settings.
    fn id(&self) -> String;
    fn name(&self) -> String;
    /// The nine colors it is made from.
    fn palette(&self) -> Palette;
}

/// A font for the interface's text, for Settings to offer beside IBM
/// Plex Sans.
pub trait FontDef: Send + Sync {
    /// Its id, kept in the settings.
    fn id(&self) -> String;
    fn name(&self) -> String;
    /// Its regular, bold, italic and bold italic faces (TrueType or
    /// OpenType files), or None when they aren't on this machine yet.
    fn faces(&self) -> Option<Vec<Vec<u8>>>;
    /// Gets the faces onto this machine (a download). Oak calls it off
    /// the UI thread when the font is picked and `faces` has none.
    fn fetch(&self) -> Result<(), String> { Ok(()) }
}

/// A system prompt a conversation can be started with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Role {
    /// Kept in the conversation and the settings.
    pub id: String,
    pub name: String,
    /// The prompt: `{{tools}}` marks where Oak's tool instructions go.
    pub text: String,
    /// The New screen offers it (false: only a plugin starts
    /// conversations with it, `NewConversation::role`).
    pub offered: bool,
    /// A Nerd Font symbol for the tabs of its conversations (empty: none).
    pub icon: String,
}

/// Somewhere else logins and keys are kept (a server several machines
/// share): Oak's accounts then include the vault's, it asks the vault
/// for their tokens, and new sign-ins go there. One vault at most.
///
/// `kind` is "claude", "codex", "openrouter", "nodeclub" or
/// "freebuff". What an entry and a login are made of is between Oak's
/// account code and the vault (`docs/PLUGINS.md` lists the fields).
pub trait Vault: Send + Sync {
    /// What it is called in sentences ("the team server").
    fn name(&self) -> String;
    fn connected(&self) -> bool;
    /// New sign-ins go to the vault (connected, and the user didn't
    /// ask for one on this machine).
    fn takes_new(&self) -> bool;
    /// Whether `error` (one of this vault's) is trouble with the vault
    /// itself, not an answer about one account: Oak then sends the
    /// request through another account.
    fn failed(&self, error: &str) -> bool;
    /// Whether `error` says the vault couldn't be reached at all: Oak
    /// then tries again shortly.
    fn unreachable(&self, error: &str) -> bool;
    /// Whether `error` (from `add`) says the vault can't keep logins
    /// of that kind at all: Oak then keeps the login on this machine,
    /// and says why.
    fn cannot_keep(&self, _error: &str) -> bool { false }
    /// Its entries of `kind`. A vault that has none: an empty list.
    fn list(&self, kind: &str) -> Result<Vec<Value>, String>;
    /// Gives it a login; what it says about it (its id there, whose it is).
    fn add(&self, kind: &str, login: &Value) -> Result<Value, String>;
    /// Takes entry `id` out, for every machine.
    fn remove(&self, kind: &str, id: &str) -> Result<(), String>;
    /// An access token for entry `id` (`{"access_token", "expires_at"}`,
    /// and what else the kind needs), a new one when `refused` (turned
    /// down by the service) is still its current one. Ok(Null): the
    /// vault no longer has the entry.
    fn token(&self, kind: &str, id: &str, refused: Option<&str>) -> Result<Value, String>;
}

/// Rows on the Settings page, under a heading.
pub trait SettingsSection: Send + Sync {
    fn title(&self) -> String;
    /// Lower first, after Oak's own rows.
    fn order(&self) -> i32 { 100 }
    fn show(&self, ui: &mut Ui, app: &mut dyn App);
}

/// A screen that comes before everything else (signing in to a
/// server Oak is of no use without).
pub trait Gate: Send + Sync {
    /// Whether the gate is shut now: Oak then shows only `show`.
    fn shut(&self) -> bool;
    fn show(&self, ui: &mut Ui, app: &mut dyn App);
}

/// Calls at the turns of Oak's life.
pub trait Hooks: Send + Sync {
    /// The window is up (before its first frame). Start threads here.
    fn started(&self, _ctx: &egui::Context) {}
    /// Every frame, before anything is drawn.
    fn frame(&self, _ctx: &egui::Context, _app: &mut dyn App) {}
    /// Draws at the top of Home's Providers column (a connection's state).
    fn providers_top(&self, _ui: &mut Ui, _app: &mut dyn App) {}
    /// Oak is closing.
    fn exiting(&self) {}
}


/// What one of a part's named calls answers (`abi::OakCall`).
enum Reply {
    /// It has no such method: Oak does what it does without it.
    Unknown,
    Done(i64),
    Text(String),
    Json(Value),
    /// Several pieces, handed over one by one.
    Parts(Vec<Vec<u8>>),
    Failed(String),
}

impl From<bool> for Reply {
    fn from(b: bool) -> Reply {
        Reply::Done(b as i64)
    }
}

impl From<String> for Reply {
    fn from(t: String) -> Reply {
        Reply::Text(t)
    }
}

impl From<Value> for Reply {
    fn from(v: Value) -> Reply {
        Reply::Json(v)
    }
}

impl From<Result<(), String>> for Reply {
    fn from(r: Result<(), String>) -> Reply {
        match r {
            Ok(()) => Reply::Done(0),
            Err(e) => Reply::Failed(e),
        }
    }
}

impl From<Result<Value, String>> for Reply {
    fn from(r: Result<Value, String>) -> Reply {
        match r {
            Ok(v) => Reply::Json(v),
            Err(e) => Reply::Failed(e),
        }
    }
}

/// An answer for Oak to read: its handle.
impl From<Result<Response, String>> for Reply {
    fn from(r: Result<Response, String>) -> Reply {
        match r {
            Ok(r) => Reply::Done(r.into_handle() as i64),
            Err(e) => Reply::Failed(e),
        }
    }
}

const STOPPED: &str = "The plugin behind this has stopped (see Settings).";

/// One of a part's named calls: `f` gets the part, the method, its
/// arguments and the bytes that came with them.
///
/// # Safety
/// `user` must be the part's (a leaked `Box<T>`), and the rest as Oak gave them.
unsafe fn called<T>(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void, f: impl FnOnce(&T, &str, &Value, &[u8]) -> Reply) -> i64 {
    let (part, method, args, data) = unsafe { (&*(user as *const T), str_of(method), str_of(args), bytes_of(data)) };
    let reply = outer(
        || Reply::Failed(STOPPED.into()),
        || {
            let args: Value = if args.is_empty() { Value::Null } else { serde_json::from_str(args).unwrap_or(Value::Null) };
            f(part, method, &args, data)
        },
    );
    match reply {
        Reply::Unknown => OAK_CALL_UNKNOWN,
        Reply::Done(n) => n,
        Reply::Text(t) => {
            unsafe { give(out, out_user, t.as_bytes()) };
            0
        }
        Reply::Json(v) => {
            unsafe { give(out, out_user, v.to_string().as_bytes()) };
            0
        }
        Reply::Parts(parts) => {
            for p in &parts {
                unsafe { give(out, out_user, p) };
            }
            parts.len() as i64
        }
        Reply::Failed(e) => {
            unsafe { give(out, out_user, e.as_bytes()) };
            -1
        }
    }
}

fn text<'a>(args: &'a Value, name: &str) -> &'a str {
    args[name].as_str().unwrap_or("")
}

/// A part, kept for good: what its descriptor's `user` points to.
fn keep<T>(part: T) -> *mut c_void {
    Box::into_raw(Box::new(part)) as *mut c_void
}

/// # Safety
/// `user` must come from `keep::<T>`.
unsafe fn part<'a, T>(user: *mut c_void) -> &'a T {
    unsafe { &*(user as *const T) }
}

type Home = Box<dyn HomeTab>;
type Tabs = Box<dyn TabKind>;
type New = Box<dyn NewKind>;
type Cloud = Box<dyn CloudBackend>;
type Kit = Box<dyn SessionKit>;
type Font = Box<dyn FontDef>;
type Safe = Box<dyn Vault>;
type Section = Box<dyn SettingsSection>;
type Door = Box<dyn Gate>;
type Life = Box<dyn Hooks>;

unsafe extern "C" fn home_shown(user: *mut c_void) -> bool {
    outer(|| false, || unsafe { part::<Home>(user) }.shown())
}

unsafe extern "C" fn home_show(user: *mut c_void, ui: *mut OakUi) {
    outer(|| (), || unsafe { part::<Home>(user) }.show(&mut Ui::raw(ui), &mut Window::default()))
}

unsafe extern "C" fn tabs_owns(user: *mut c_void, key: OakStr) -> bool {
    outer(|| false, || unsafe { part::<Tabs>(user).owns(str_of(key)) })
}

unsafe extern "C" fn tabs_show(user: *mut c_void, ui: *mut OakUi, key: OakStr) {
    outer(|| (), || unsafe { part::<Tabs>(user).show(&mut Ui::raw(ui), &mut Window::default(), str_of(key)) })
}

unsafe extern "C" fn tabs_corner(user: *mut c_void, key: OakStr, area: *const OakRect) {
    outer(|| (), || unsafe { part::<Tabs>(user).corner(&egui::Context, &mut Window::default(), str_of(key), egui::rect_out(*area)) })
}

unsafe extern "C" fn tabs_call(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64 {
    unsafe {
        called::<Tabs>(user, method, args, data, out, out_user, |t, method, a, _| {
            let key = text(a, "key");
            match method {
                "exists" => t.exists(key).into(),
                "composer" => t.composer(key).into(),
                "look" => {
                    let l = t.look(key);
                    json!({"title": l.title, "busy": l.busy, "icon": l.icon, "icon_is_text": l.icon_is_text}).into()
                }
                "send" => t.send(&mut Window::default(), key, text(a, "text").to_string(), a["files"].as_array().cloned().unwrap_or_default()).into(),
                _ => Reply::Unknown,
            }
        })
    }
}

unsafe extern "C" fn new_form(user: *mut c_void, ui: *mut OakUi) {
    outer(|| (), || unsafe { part::<New>(user) }.form(&mut Ui::raw(ui), &mut Window::default()))
}

unsafe extern "C" fn new_call(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64 {
    unsafe {
        called::<New>(user, method, args, data, out, out_user, |n, method, a, _| match method {
            "start" => n.start(&mut Window::default(), text(a, "text").to_string(), a["files"].as_array().cloned().unwrap_or_default()).into(),
            _ => Reply::Unknown,
        })
    }
}

unsafe extern "C" fn cloud_form(user: *mut c_void, ui: *mut OakUi) {
    outer(|| (), || unsafe { part::<Cloud>(user) }.form(&mut Ui::raw(ui), &mut Window::default()))
}

unsafe extern "C" fn cloud_call(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64 {
    unsafe {
        called::<Cloud>(user, method, args, data, out, out_user, |c, method, a, _| match method {
            "accounts" => Value::from(c.accounts().iter().map(|a| json!({"rid": a.rid, "label": a.label, "blocked_until": a.blocked_until})).collect::<Vec<_>>()).into(),
            "unavailable" => c.unavailable().map_or(Reply::Done(0), Reply::Text),
            "note_limit" => {
                c.note_limit(text(a, "rid"), a["until"].as_i64().unwrap_or(0));
                Reply::Done(0)
            }
            "request" => c.request(text(a, "rid"), Call::from_json(a)).into(),
            "problem" => c.problem(a["status"].as_u64().unwrap_or(0) as u16).map_or(Reply::Done(0), Reply::Text),
            _ => Reply::Unknown,
        })
    }
}

unsafe extern "C" fn kit_call(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64 {
    unsafe {
        called::<Kit>(user, method, args, data, out, out_user, |k, method, a, data| match method {
            "ready" => k.ready().into(),
            "server" => k.server().map(|(url, token)| json!({"url": url, "token": token})).into(),
            "server_name" => k.server_name().into(),
            "environment_name" => k.environment_name().into(),
            "setup_script" => k.setup_script(text(a, "server")).into(),
            "variables" => k.variables(text(a, "server"), text(a, "token")).into(),
            "setup_block" => k.setup_block().into(),
            "cli" => k.cli().into(),
            "tags" => {
                let t = k.tags();
                json!({"setup": [t.setup.0, t.setup.1], "files": [t.files.0, t.files.1], "carry": [t.carry.0, t.carry.1]}).into()
            }
            "notes" => Value::from(k.notes().into_iter().map(|(a, b, c)| json!([a, b, c])).collect::<Vec<_>>()).into(),
            "raw" => {
                let body = a["content_type"].as_str().map(|kind| (data.to_vec(), kind.to_string()));
                k.raw(text(a, "method"), text(a, "path"), body, Duration::from_millis(a["timeout_ms"].as_u64().unwrap_or(30_000))).into()
            }
            _ => Reply::Unknown,
        })
    }
}

unsafe extern "C" fn font_call(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64 {
    unsafe {
        called::<Font>(user, method, args, data, out, out_user, |f, method, _, _| match method {
            "faces" => f.faces().map_or(Reply::Done(0), Reply::Parts),
            "fetch" => f.fetch().into(),
            _ => Reply::Unknown,
        })
    }
}

unsafe extern "C" fn vault_call(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64 {
    unsafe {
        called::<Safe>(user, method, args, data, out, out_user, |v, method, a, _| {
            let (kind, id, error) = (text(a, "kind"), text(a, "id"), text(a, "error"));
            match method {
                "name" => v.name().into(),
                "connected" => v.connected().into(),
                "takes_new" => v.takes_new().into(),
                "failed" => v.failed(error).into(),
                "unreachable" => v.unreachable(error).into(),
                "cannot_keep" => v.cannot_keep(error).into(),
                "list" => v.list(kind).map(Value::from).into(),
                "add" => v.add(kind, &a["login"]).into(),
                "remove" => v.remove(kind, id).into(),
                "token" => v.token(kind, id, a["refused"].as_str()).into(),
                _ => Reply::Unknown,
            }
        })
    }
}

unsafe extern "C" fn section_show(user: *mut c_void, ui: *mut OakUi) {
    outer(|| (), || unsafe { part::<Section>(user) }.show(&mut Ui::raw(ui), &mut Window::default()))
}

unsafe extern "C" fn gate_shut(user: *mut c_void) -> bool {
    outer(|| false, || unsafe { part::<Door>(user) }.shut())
}

unsafe extern "C" fn gate_show(user: *mut c_void, ui: *mut OakUi) {
    outer(|| (), || unsafe { part::<Door>(user) }.show(&mut Ui::raw(ui), &mut Window::default()))
}

unsafe extern "C" fn hooks_started(user: *mut c_void) {
    outer(|| (), || unsafe { part::<Life>(user) }.started(&egui::Context))
}

unsafe extern "C" fn hooks_frame(user: *mut c_void) {
    outer(|| (), || unsafe { part::<Life>(user) }.frame(&egui::Context, &mut Window::default()))
}

unsafe extern "C" fn hooks_providers_top(user: *mut c_void, ui: *mut OakUi) {
    outer(|| (), || unsafe { part::<Life>(user) }.providers_top(&mut Ui::raw(ui), &mut Window::default()))
}

unsafe extern "C" fn hooks_exiting(user: *mut c_void) {
    outer(|| (), || unsafe { part::<Life>(user) }.exiting())
}

fn size_of<T>() -> u32 {
    std::mem::size_of::<T>() as u32
}

fn icon_flags(is_text: bool) -> u32 {
    if is_text { OAK_ICON_IS_TEXT } else { 0 }
}

/// Where a plugin puts its parts (`Plugin::register`).
#[derive(Default)]
pub struct Registrar {
    _private: (),
}

impl Registrar {
    pub(crate) fn new() -> Registrar {
        Registrar::default()
    }

    /// A tab on Home.
    pub fn home_tab(&mut self, tab: impl HomeTab + 'static) {
        let (id, label, icon, tip) = (tab.id(), tab.label(), tab.icon(), tab.tip());
        let d = OakHomeTab {
            size: size_of::<OakHomeTab>(),
            flags: icon_flags(tab.icon_is_text()),
            id: s(&id),
            label: s(&label),
            icon: s(&icon),
            tip: s(&tip),
            order: tab.order(),
            shown: Some(home_shown),
            show: Some(home_show),
            user: keep::<Home>(Box::new(tab)),
        };
        unsafe { (api().add_home_tab)(&d) };
    }

    /// Tabs of the plugin's own.
    pub fn tab_kind(&mut self, kind: impl TabKind + 'static) {
        let d = OakTabKind { size: size_of::<OakTabKind>(), flags: 0, user: keep::<Tabs>(Box::new(kind)), owns: Some(tabs_owns), show: Some(tabs_show), corner: Some(tabs_corner), call: Some(tabs_call) };
        unsafe { (api().add_tab_kind)(&d) };
    }

    /// A choice on the New screen.
    pub fn new_kind(&mut self, kind: impl NewKind + 'static) {
        let (id, label, icon, tip, heading) = (kind.id(), kind.label(), kind.icon(), kind.tip(), kind.heading());
        let d = OakNewKind {
            size: size_of::<OakNewKind>(),
            flags: 0,
            id: s(&id),
            label: s(&label),
            icon: s(&icon),
            tip: s(&tip),
            heading: s(&heading),
            form: Some(new_form),
            call: Some(new_call),
            user: keep::<New>(Box::new(kind)),
        };
        unsafe { (api().add_new_kind)(&d) };
    }

    /// A service cloud sessions run through.
    pub fn cloud_backend(&mut self, backend: impl CloudBackend + 'static) {
        let (id, name, icon, tip, about) = (backend.id(), backend.name(), backend.icon(), backend.tip(), backend.about());
        let d = OakCloudBackend {
            size: size_of::<OakCloudBackend>(),
            flags: icon_flags(backend.icon_is_text()),
            id: s(&id),
            name: s(&name),
            icon: s(&icon),
            tip: s(&tip),
            about: s(&about),
            form: Some(cloud_form),
            call: Some(cloud_call),
            user: keep::<Cloud>(Box::new(backend)),
        };
        unsafe { (api().add_cloud_backend)(&d) };
    }

    /// What the cloud sessions of backend `backend` are given ("": Oak's
    /// own, on this machine's and the vault's Claude logins).
    pub fn session_kit(&mut self, backend: &str, kit: impl SessionKit + 'static) {
        let d = OakSessionKit { size: size_of::<OakSessionKit>(), flags: 0, user: keep::<Kit>(Box::new(kit)), backend: s(backend), call: Some(kit_call) };
        unsafe { (api().add_session_kit)(&d) };
    }

    /// A theme.
    pub fn theme(&mut self, theme: impl ThemeDef + 'static) {
        let (id, name, p) = (theme.id(), theme.name(), theme.palette());
        let d = OakThemeDef {
            size: size_of::<OakThemeDef>(),
            line_alpha: p.line_alpha as u32,
            id: s(&id),
            name: s(&name),
            bg: p.bg,
            panel: p.panel,
            panel2: p.panel2,
            ink: p.ink,
            ink2: p.ink2,
            ink3: p.ink3,
            ink4: p.ink4,
            accent: p.accent,
            error: p.error,
        };
        unsafe { (api().add_theme)(&d) };
    }

    /// A font.
    pub fn font(&mut self, font: impl FontDef + 'static) {
        let (id, name) = (font.id(), font.name());
        let d = OakFontDef { size: size_of::<OakFontDef>(), flags: 0, id: s(&id), name: s(&name), call: Some(font_call), user: keep::<Font>(Box::new(font)) };
        unsafe { (api().add_font)(&d) };
    }

    /// A system prompt.
    pub fn role(&mut self, role: Role) {
        let d = OakRole { size: size_of::<OakRole>(), flags: if role.offered { OAK_ROLE_OFFERED } else { 0 }, id: s(&role.id), name: s(&role.name), text: s(&role.text), icon: s(&role.icon) };
        unsafe { (api().add_role)(&d) };
    }

    /// Where logins are kept besides this machine (the last one registered).
    pub fn vault(&mut self, vault: impl Vault + 'static) {
        let d = OakVault { size: size_of::<OakVault>(), flags: 0, user: keep::<Safe>(Box::new(vault)), call: Some(vault_call) };
        unsafe { (api().add_vault)(&d) };
    }

    /// Rows on Settings.
    pub fn settings(&mut self, section: impl SettingsSection + 'static) {
        let title = section.title();
        let d = OakSettings { size: size_of::<OakSettings>(), flags: 0, title: s(&title), order: section.order(), show: Some(section_show), user: keep::<Section>(Box::new(section)) };
        unsafe { (api().add_settings)(&d) };
    }

    /// A screen before everything.
    pub fn gate(&mut self, gate: impl Gate + 'static) {
        let d = OakGate { size: size_of::<OakGate>(), flags: 0, user: keep::<Door>(Box::new(gate)), shut: Some(gate_shut), show: Some(gate_show) };
        unsafe { (api().add_gate)(&d) };
    }

    /// Calls at startup, every frame, at exit.
    pub fn hooks(&mut self, hooks: impl Hooks + 'static) {
        let d = OakHooks { size: size_of::<OakHooks>(), flags: 0, user: keep::<Life>(Box::new(hooks)), started: Some(hooks_started), frame: Some(hooks_frame), providers_top: Some(hooks_providers_top), exiting: Some(hooks_exiting) };
        unsafe { (api().add_hooks)(&d) };
    }
}
