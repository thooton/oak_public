//! Oak, as a plugin reaches it.
//!
//! `Host` is the whole of Oak any thread may use: its folder, HTTP, the
//! usage log, cloud sessions, what plugins share with each other.
//! `crate::host()` gives it, once Oak started the plugin.
//!
//! `App` is the window while a plugin draws in it: its tabs, the message
//! box, the hint over it. It comes as an argument to what draws
//! (`HomeTab::show`, `TabKind::show`, ...), on the UI thread, and isn't
//! kept.
//!
//! Both are thin: each method is one of `abi`'s functions.

use crate::abi::*;
use crate::chat::Chat;
use crate::cloud::Account;
use crate::egui::{self, Ui, with_body};
use crate::rt::{api, bytes, bytes_from, carry, rethrow, s, str_of, text_from};
use crate::theme::Theme;
use crate::usage::Rec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// An HTTP request for `Host::http`.
#[derive(Clone, Debug)]
pub struct Request {
    /// "GET", "POST", ...
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Bounds the whole request, its body's download included.
    pub timeout: Duration,
    /// Follow redirects.
    pub redirects: bool,
    /// Connect straight to the address, whatever proxy is set (a service
    /// on this machine).
    pub direct: bool,
}

impl Request {
    pub fn new(method: &str, url: impl Into<String>) -> Request {
        Request { method: method.to_string(), url: url.into(), headers: vec![], body: None, timeout: Duration::from_secs(30), redirects: true, direct: false }
    }

    pub fn get(url: impl Into<String>) -> Request {
        Request::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Request {
        Request::new("POST", url)
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// `Authorization: Bearer`.
    pub fn bearer(self, token: &str) -> Request {
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// A redirect is the answer, not followed.
    pub fn no_redirects(mut self) -> Request {
        self.redirects = false;
        self
    }

    /// No proxy: straight to the address (a service on this machine).
    pub fn direct(mut self) -> Request {
        self.direct = true;
        self
    }

    /// A JSON body.
    pub fn json(mut self, body: &Value) -> Request {
        self.body = Some(serde_json::to_vec(body).unwrap_or_default());
        self.header("Content-Type", "application/json")
    }

    /// A body of `kind` (its content type).
    pub fn body(mut self, bytes: Vec<u8>, kind: &str) -> Request {
        self.body = Some(bytes);
        self.header("Content-Type", kind)
    }

    pub fn timeout(mut self, timeout: Duration) -> Request {
        self.timeout = timeout;
        self
    }

    /// Sends it (`Host::http`).
    pub fn send(self) -> Result<Response, String> {
        host().http(self)
    }
}

/// Headers as the interface carries them: one `name: value` per line.
fn header_lines(headers: &[(String, String)]) -> String {
    headers.iter().map(|(n, v)| format!("{n}: {}\n", v.replace(['\r', '\n'], " "))).collect()
}

fn headers_from(lines: &str) -> Vec<(String, String)> {
    lines.lines().filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_string(), v.trim().to_string())).collect()
}

/// An answer Oak holds, read through its handle; closed when dropped
/// unless the handle was taken (`Response::into_handle`).
struct Held(Arc<AtomicU64>);

impl Read for Held {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let h = self.0.load(Ordering::Acquire);
        if h == 0 || buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe { (api().http_read)(h, buf.as_mut_ptr(), buf.len()) };
        if n < 0 { Err(std::io::Error::other("the answer's body couldn't be read")) } else { Ok(n as usize) }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let h = self.0.swap(0, Ordering::AcqRel);
        if h != 0 {
            unsafe { (api().http_close)(h) };
        }
    }
}

/// An HTTP answer: its status and headers, and its body as it arrives
/// (an event stream is read line by line while it is open).
pub struct Response {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
    /// Oak's handle of it, when Oak holds it (0 once taken, or never).
    handle: Arc<AtomicU64>,
}

impl Response {
    /// An answer made here.
    pub fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Response {
        Response { status, headers, body: Box::new(std::io::Cursor::new(body)), handle: Arc::new(AtomicU64::new(0)) }
    }

    /// The answer Oak holds under `handle`.
    pub fn from_handle(handle: u64) -> Response {
        let status = unsafe { (api().http_status)(handle) } as u16;
        let headers = headers_from(&text_from(|out, user| unsafe { (api().http_headers)(handle, out, user) }).1);
        let handle = Arc::new(AtomicU64::new(handle));
        Response { status, headers, body: Box::new(Held(handle.clone())), handle }
    }

    /// Gives the answer to Oak (a part's `request` returns one): its handle.
    pub fn into_handle(mut self) -> u64 {
        let h = self.handle.swap(0, Ordering::AcqRel);
        if h != 0 {
            return h;
        }
        let mut body = vec![];
        let _ = self.body.read_to_end(&mut body);
        unsafe { (api().http_make)(self.status as u32, s(&header_lines(&self.headers)), bytes(&body)) }
    }

    /// 200 to 299.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The whole body.
    pub fn bytes(mut self) -> Result<Vec<u8>, String> {
        let mut out = vec![];
        self.body.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// The whole body, as text (what isn't UTF-8 replaced).
    pub fn text(self) -> Result<String, String> {
        self.bytes().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// The whole body, as JSON.
    pub fn json(self) -> Result<Value, String> {
        serde_json::from_slice(&self.bytes()?).map_err(|e| e.to_string())
    }
}

/// A conversation of this machine's, as a plugin's list shows it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    /// Its tab's key.
    pub id: String,
    pub title: String,
    pub folder: String,
    pub model: String,
    /// The role (system prompt) it was started with.
    pub role: String,
    /// When its last message was (ms since 1970).
    pub at: i64,
    pub busy: bool,
}

/// What Oak is doing now (`App::activity`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    /// "home", "chats", "settings", "usage" or "plugin" (a plugin's page).
    pub page: String,
    /// The active Home plugin page, by id, if there is one.
    pub plugin_page: Option<String>,
    /// Home's menu is open.
    pub menu: bool,
    /// Where Home's menu is, when it is open: x0, y0, x1, y1.
    pub menu_rect: Option<[f32; 4]>,
    /// Home's buttons in the bottom right corner.
    pub buttons_rect: Option<[f32; 4]>,
    pub maximized: bool,
    pub fullscreen: bool,
    pub focused: bool,
    /// Background tasks running.
    pub tasks: usize,
    pub conversations: Vec<Doing>,
}

/// A conversation's state, in `Activity`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Doing {
    pub id: String,
    pub title: String,
    pub busy: bool,
    /// "idle", "requesting" (sent, nothing back yet), "thinking", "writing",
    /// "tool" (a tool running, or its call streaming in), "waiting" (on a
    /// wait: ASK, a task, an agent), "limit" (for a usage limit to reset),
    /// "retrying" (after an error) or "compacting".
    pub phase: String,
    /// The line Oak shows about what it is doing ("" when idle).
    pub status: String,
    /// The tool running or being called ("" when none).
    pub tool: String,
    /// ASK is waiting for the user's answers.
    pub asking: bool,
    /// Its last turn ended in an error.
    pub failed: bool,
    pub paused: bool,
    /// Its tab is the one shown.
    pub shown: bool,
    /// When its latest message was (ms since 1970).
    pub at: i64,
}

/// What the open conversation's transcript shows (`App::chat_scene`), in
/// points on the page.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatScene {
    pub conversation: String,
    /// The transcript, clear of its scroll bar and minimap: x0, y0, x1, y1.
    pub view: [f32; 4],
    /// The boxes over it: the session's, notes and tasks.
    pub solids: Vec<[f32; 4]>,
    /// The body text's size.
    pub text_size: f32,
    /// The transcript's units on screen, and where each starts. A unit
    /// keeps its number while the transcript scrolls and grows.
    pub units: Vec<(usize, f32)>,
    /// Each row of text on screen: its unit, then x0, y0, x1, y1 from its
    /// first ink to its last, between the tops and bottoms of its small
    /// letters.
    pub rows: Vec<(usize, [f32; 4])>,
}

/// A conversation to start on this machine (`App::start_conversation`):
/// in the folder and with the model and effort the New screen's rows show
/// (`App::local_form`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NewConversation {
    /// Its title (empty: from its first message).
    pub title: String,
    /// The role (system prompt) it runs with: a `plugin::Role`'s id, or
    /// one of Oak's (empty: the one picked in Settings).
    pub role: String,
    /// Its first message (empty with no files: it waits for one).
    pub text: String,
    /// The message's attachments, as the message box holds them.
    pub files: Vec<Value>,
    /// Where it runs (empty: the folder picked in Settings). A plugin may
    /// name one its own choice of folder didn't, to carry a conversation
    /// on where another was.
    pub folder: String,
    /// The model it runs on, and who serves it: an id `App::conversations`
    /// gives, or empty for the one picked in Settings. Its provider is
    /// worked out from the model unless `provider` says which.
    pub model: String,
    pub provider: String,
    /// Its effort ("high"; empty: the one picked in Settings).
    pub effort: String,
    /// A chat: no folder, no project, only the web's tools.
    pub chat: bool,
}

/// Oak's services, for any thread.
pub struct Host {
    _private: (),
}

static HOST: Host = Host { _private: () };

/// Oak. Panics before Oak started the plugin (`has_host` tells).
pub fn host() -> &'static Host {
    api();
    &HOST
}

/// Whether Oak started the plugin: false in the plugin's own tests.
pub fn has_host() -> bool {
    crate::rt::has_api()
}

/// What a call that can fail answered: its text, or why it failed.
pub(crate) fn checked(call: impl FnOnce(OakSink, *mut c_void) -> bool) -> Result<String, String> {
    let (ok, text) = text_from(call);
    if ok { Ok(text) } else { Err(text) }
}

impl Host {
    /// Oak's version ("0.2.214").
    pub fn version(&self) -> String {
        text_from(|out, user| unsafe { (api().version)(out, user) }).1
    }

    /// Oak's folder (settings, logins, conversations). A plugin keeps its
    /// files there, under names of its own.
    pub fn dir(&self) -> PathBuf {
        PathBuf::from(text_from(|out, user| unsafe { (api().dir)(out, user) }).1)
    }

    /// Appends `text` to crash.log.
    pub fn log_crash(&self, text: &str) {
        unsafe { (api().log_crash)(s(text)) };
    }

    /// Draw the window again soon: after a background job changed what it
    /// shows.
    pub fn repaint(&self) {
        unsafe { (api().repaint)() };
    }

    pub fn repaint_after(&self, after: Duration) {
        unsafe { (api().repaint_after)(after.as_millis() as u64) };
    }

    /// The window, for asking it to repaint from a thread.
    pub fn context(&self) -> Option<egui::Context> {
        Some(egui::Context)
    }

    /// Sends an HTTP request with Oak's client (its proxy settings, TLS
    /// roots and user agent). Err: it couldn't be sent or no answer came;
    /// an answer with an error status is Ok.
    pub fn http(&self, request: Request) -> Result<Response, String> {
        let headers = header_lines(&request.headers);
        let body = request.body.as_deref().unwrap_or(&[]);
        let r = OakRequest {
            size: std::mem::size_of::<OakRequest>() as u32,
            flags: (if request.redirects { 0 } else { OAK_HTTP_NO_REDIRECTS }) | (if request.direct { OAK_HTTP_DIRECT } else { 0 }),
            method: s(&request.method),
            url: s(&request.url),
            headers: s(&headers),
            body: bytes(body),
            timeout_ms: request.timeout.as_millis() as u64,
        };
        let (handle, error) = text_from(|out, user| unsafe { (api().http_send)(&r, out, user) });
        if handle == 0 { Err(error) } else { Ok(Response::from_handle(handle)) }
    }

    /// Writes a file only its owner can read (a login, a token), whole or
    /// not at all.
    pub fn write_private(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        checked(|out, user| unsafe { (api().write_private)(s(&path.to_string_lossy()), bytes(data), out, user) }).map(drop)
    }

    /// A random UUID.
    pub fn uuid(&self) -> String {
        text_from(|out, user| unsafe { (api().uuid)(out, user) }).1
    }

    /// What plugins tell each other: a value under a name (a server's
    /// connection under its plugin's id, say). None: nothing was put there.
    pub fn shared(&self, name: &str) -> Option<Value> {
        let (there, text) = text_from(|out, user| unsafe { (api().shared)(s(name), out, user) });
        if there { serde_json::from_str(&text).ok() } else { None }
    }

    /// Puts `value` under `name` (None takes it away).
    pub fn share(&self, name: &str, value: Option<Value>) {
        let json = value.map(|v| v.to_string()).unwrap_or_default();
        unsafe { (api().share)(s(name), s(&json)) };
    }

    /// Whether any login or key is on this machine (or a vault is
    /// connected): something a conversation could run on.
    pub fn signed_in(&self) -> bool {
        unsafe { (api().signed_in)() }
    }

    /// Starts a browser sign-in that finishes on its own (`quick`) from any
    /// thread, without the window (interface 16): Oak waits for it in the
    /// background and keeps the account as Home's Add account does. A new
    /// one replaces one still waiting. Ok: the URL to load (in a
    /// `browser::Browser`, say).
    pub fn start_background_sign_in(&self, provider: &str) -> Result<String, String> {
        checked(|out, user| unsafe { (api().background_sign_in)(s(provider), out, user) })
    }

    /// The vault (`plugin::Vault`) connected or disconnected, or its
    /// logins changed: Oak brings its accounts in step, off this thread.
    pub fn vault_changed(&self) {
        unsafe { (api().vault_changed)() };
    }

    /// This machine's usage records from `since` (ms since 1970) on,
    /// oldest first (the other machines' aren't in them). Err: the log
    /// couldn't be saved first.
    pub fn usage_since(&self, since: i64) -> Result<Vec<Rec>, String> {
        let lines = checked(|out, user| unsafe { (api().usage_local)(since, out, user) })?;
        Ok(lines.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
    }

    /// Works with all of this machine's usage records, oldest first.
    pub fn usage_local(&self, f: &mut dyn FnMut(&mut dyn Iterator<Item = &Rec>)) -> Result<(), String> {
        let records = self.usage_since(0)?;
        f(&mut records.iter());
        Ok(())
    }

    /// The records other machines made, from `source` (where they are
    /// shared: an address). They show while `usage_show` names it.
    pub fn usage_set_shared(&self, source: &str, records: Vec<Rec>) {
        let json = serde_json::to_string(&records).unwrap_or_default();
        unsafe { (api().usage_set_shared)(s(source), s(&json)) };
    }

    /// More of `source`'s records. False when Oak holds another source's.
    pub fn usage_add_shared(&self, source: &str, records: &[Rec]) -> bool {
        let json = serde_json::to_string(records).unwrap_or_default();
        unsafe { (api().usage_add_shared)(s(source), s(&json)) }
    }

    /// Which source's shared records the Usage page shows with this
    /// machine's (None: this machine's only), and what it says about the
    /// sharing (a label, and a line of detail).
    pub fn usage_show(&self, source: Option<&str>, label: &str, detail: &str) {
        unsafe { (api().usage_show)(s(source.unwrap_or("")), s(label), s(detail)) };
    }

    /// Drops every record from before `cutoff` (ms since 1970), on disk
    /// too: the shared log was reset then.
    pub fn usage_discard_before(&self, cutoff: i64) -> Result<(), String> {
        checked(|out, user| unsafe { (api().usage_discard_before)(cutoff, out, user) }).map(drop)
    }

    /// Names the cloud session behind tab `key` before it is opened (one
    /// started elsewhere, which no list has named yet).
    pub fn cloud_name(&self, key: &str, title: &str) {
        unsafe { (api().cloud_name)(s(key), s(title)) };
    }

    /// Forgets what Oak holds of backend `id`'s sessions (it disconnected,
    /// or connects to another server).
    pub fn cloud_forget(&self, backend: &str) {
        unsafe { (api().cloud_forget)(s(backend)) };
    }

    /// A cloud session is being started.
    pub fn cloud_starting(&self) -> bool {
        unsafe { (api().cloud_starting)() }
    }

    /// The accounts Oak's own cloud sessions run on.
    pub fn cloud_accounts(&self) -> Vec<Account> {
        serde_json::from_str(&text_from(|out, user| unsafe { (api().cloud_accounts)(out, user) }).1).unwrap_or_default()
    }

    /// A picture's bytes as an image block (scaled to what the API takes).
    pub fn image_block(&self, data: &[u8]) -> Result<Value, String> {
        let json = checked(|out, user| unsafe { (api().image_block)(bytes(data), out, user) })?;
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    /// A file's bytes as the block the message box would attach.
    pub fn file_block(&self, path: &Path, data: &[u8]) -> Result<Value, String> {
        let json = checked(|out, user| unsafe { (api().file_block)(s(&path.to_string_lossy()), bytes(data), out, user) })?;
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    /// Asks a model for a reply through one of Oak's providers, with the
    /// user's logins and keys, and waits for it to end (Oak 9; not on the
    /// UI thread). `request`: `{"model", "messages", "provider"?,
    /// "system"?, "max_tokens"?, "thinking"?, "effort"?, "usage"?}`
    /// (`abi`'s `model_stream`). `event` gets each event of the reply as it
    /// streams in: `{"text"}`, `{"thinking"}`, `{"cost"}`, `{"retry"}`.
    /// Err: why it failed.
    pub fn model_stream(&self, request: &Value, event: &mut dyn FnMut(&Value)) -> Result<(), String> {
        unsafe extern "C" fn take(user: *mut c_void, text: OakStr) {
            let event = unsafe { &mut *(user as *mut &mut dyn FnMut(&Value)) };
            let v: Value = serde_json::from_str(unsafe { crate::rt::str_of(text) }).unwrap_or(Value::Null);
            crate::rt::carry(|| event(&v));
        }
        let json = request.to_string();
        let mut event = event;
        let user = &mut event as *mut &mut dyn FnMut(&Value) as *mut c_void;
        let r = checked(|out, out_user| unsafe { (api().model_stream)(s(&json), take, user, out, out_user) }).map(drop);
        crate::rt::rethrow();
        r
    }
}

/// The window, while a plugin draws in it.
pub trait App {
    /// The theme in use.
    fn theme(&self) -> Theme;
    /// Opens (or shows) the tab `key`: a conversation's id, a cloud
    /// session's key, or a key one of the plugin's `TabKind`s owns.
    fn open(&mut self, key: &str);
    /// Closes the tabs `gone` says to, and forgets them as tabs to reopen.
    fn close_tabs(&mut self, gone: &dyn Fn(&str) -> bool);
    /// The key of the tab shown, when one is (not Home, not the New screen).
    fn shown(&self) -> Option<String>;
    /// Shows Home, on tab `tab` (a `HomeTab`'s id, "local", "cloud").
    fn go_home(&mut self, tab: &str);
    /// Shows the New screen with `kind` picked (a `NewKind`'s id, a cloud
    /// backend's id, "local", "chat", "super").
    fn go_new(&mut self, kind: &str);
    /// Shows Settings; leaving it comes back to what was shown (Oak 5).
    fn go_settings(&mut self);
    /// Shows the page of the `HomeTab` `id` (one whose `page` is true; Oak 5).
    fn go_page(&mut self, id: &str);
    /// What was pasted (pictures) or dropped on the window this frame,
    /// as image and file blocks the message box would attach, and why any
    /// couldn't be. Whoever takes them has them: take them only while
    /// showing somewhere they go (Oak 5).
    fn take_files(&mut self) -> Attached;
    /// Asks for files to attach in the system's dialog, offering those
    /// with `extensions` (named `filter`; none: any file) and any file.
    /// `None`: none were picked (Oak 5).
    fn pick_files(&mut self, filter: &str, extensions: &[&str]) -> Option<Attached>;
    /// Says `text` over the message box, in the error color (None clears it).
    fn hint(&mut self, text: Option<String>);
    /// The message box's text. What is changed in it reaches the box when
    /// the plugin's call returns, or the next time the `App` is used.
    fn input(&mut self) -> &mut String;
    /// Puts `text` in the message box at its cursor, as if typed there
    /// (over what is selected), and gives the box the keyboard; `spaced`:
    /// with a space between it and the text either side where there is
    /// none (interface version 11). By default it is added at the end.
    fn insert_input(&mut self, text: &str, spaced: bool) {
        let input = self.input();
        if spaced && !input.is_empty() && !input.ends_with(char::is_whitespace) && !text.starts_with(char::is_whitespace) {
            input.push(' ');
        }
        input.push_str(text);
    }
    /// What is attached to the message (image and file blocks); changes
    /// reach Oak as `input`'s do.
    fn attachments(&mut self) -> &mut Vec<Value>;
    /// This machine's conversations (not side chats, not subagents).
    fn conversations(&self) -> Vec<Conversation>;
    /// What Oak and its conversations are doing now (interface version 7).
    fn activity(&self) -> Activity;
    /// During `Hooks::chat_scene`, what the transcript shows (interface
    /// version 7); None at other times.
    fn chat_scene(&self) -> Option<ChatScene>;
    /// Starts a conversation on this machine and opens its tab. Err: why
    /// it couldn't (the folder is missing, say), also shown as the hint.
    fn start_conversation(&mut self, new: NewConversation) -> Result<String, String>;
    /// The New screen's rows for a conversation on this machine: the
    /// folder, the model and the effort.
    fn local_form(&mut self, ui: &mut Ui);
    /// Starts Oak's normal browser sign-in on a user action (interface 12).
    /// `provider`: `claude`, `codex`, `antigravity`, `freebuff` or `quick`.
    /// Returns the URL without opening a browser or writing the clipboard.
    /// Oak keeps the sign-in state and stores the account as it would on Home.
    fn start_sign_in(&mut self, _provider: &str) -> Result<String, String> {
        Err("This window cannot start a sign-in.".into())
    }
    /// Shows the pending Claude, Codex or Antigravity sign-in form, if any
    /// (interface 12). Draw it each frame in one place, or use the same form
    /// on Home. Freebuff and Quick finish automatically in the background.
    /// True if the form was active, not whether sign-in succeeded.
    fn sign_in_form(&mut self, _ui: &mut Ui) -> bool { false }
    /// Draws a private browser that the user operates, filling the rest of
    /// this Home plugin page (interface 13). `url` opens once, as an HTTPS
    /// entry point. Draw in a non-scrolling container. Navigation and input
    /// stay in Oak's browser; the session is discarded when leaving the page.
    fn private_browser(&mut self, _ui: &mut Ui, _url: &str) -> Result<(), String> {
        Err("This window cannot show a private browser.".into())
    }
    /// Starts an isolated background Chromium session from a Home plugin
    /// page (interface 14). Start on a user action and do browser operations
    /// on a worker. The host discards the session when the page closes.
    fn open_background_browser(&mut self, _url: &str) -> Result<crate::browser::Browser, String> {
        Err("This window cannot open a background browser.".into())
    }
    /// Draws the live picture of `browser`, a session this Home plugin page
    /// opened (interface 15) or one opened with `browser::open` (interface
    /// 16). Read-only, as wide as the space left and at
    /// most `max_h` tall (0: no limit), its origin over it. Draw it each
    /// frame it should show; the picture streams only while it is drawn.
    /// The user can watch but not send input, and the plugin gets no pixels.
    fn background_browser_view(&mut self, _ui: &mut Ui, _browser: &crate::browser::Browser, _max_h: f32) -> Result<(), String> {
        Err("This window cannot show a background browser.".into())
    }
    /// Draws a background session (one opened with `browser::open`, or this
    /// page's) for the user to work (interface 17): the pointer, wheel and
    /// keys reach that same page, cookies and all, as in a conversation's
    /// browser. As wide as the space left and at most `max_h` tall (0: no
    /// limit). Don't drive the session yourself while it is drawn. Draw it
    /// each frame it should show; the picture streams only while it is.
    fn background_browser_live_view(&mut self, _ui: &mut Ui, _browser: &crate::browser::Browser, _max_h: f32) -> Result<(), String> {
        Err("This window cannot hand a background browser to the user.".into())
    }
    /// Draws a transcript the plugin has, as a conversation's is drawn,
    /// read-only. `key` is its tab; `version` changes when the transcript
    /// does (`make` is called only then); `tail` draws what follows the
    /// last message, and `tail_sig` changes when that would draw otherwise.
    fn transcript(&mut self, ui: &mut Ui, key: &str, version: u64, make: &mut dyn FnMut() -> Chat, tail_sig: u64, tail: &mut dyn FnMut(&mut Ui));
    /// The list of backend `id`'s cloud sessions, as Home's Cloud tab
    /// shows Oak's own: a click opens the session.
    fn cloud_list(&mut self, ui: &mut Ui, backend: &str);
    /// A conversation of this machine's (`Session`), under its key. None:
    /// there is none.
    fn session(&self, key: &str) -> Option<Session>;
    /// Switches conversation `key` to model `model` of provider `provider`,
    /// as its model picker does but without asking. Err: why it couldn't.
    fn set_model(&mut self, key: &str, provider: &str, model: &str) -> Result<(), String>;
    /// Sends conversation `key` a message from the user, as the message box
    /// does. Err: why it couldn't.
    fn send(&mut self, key: &str, text: &str) -> Result<(), String>;
    /// Stops what conversation `key` is doing, as its interrupt does. Err:
    /// why it couldn't.
    fn interrupt(&mut self, key: &str) -> Result<(), String>;
    /// Asks conversation `key` to answer its last turn, as resuming a paused
    /// one does (a transcript that ends in a message from the user that
    /// nothing woke to answer). Err: why it couldn't.
    fn resume(&mut self, key: &str) -> Result<(), String>;
}

/// A conversation of this machine's, as `App::session` shows it: what it
/// is, what it is doing, and how its last turn went.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub folder: String,
    pub model: String,
    /// Who serves `model`.
    pub provider: String,
    pub effort: String,
    pub role: String,
    /// A chat: no folder, no project.
    pub chat: bool,
    /// A turn is under way.
    pub busy: bool,
    /// What it is doing now ("Retry 2/4 in 8s after: a safety classifier
    /// declined the request (category cyber: …)", say).
    pub status: String,
    /// What went wrong in its last turn, when it failed.
    pub error: Option<String>,
    /// The text of its last message, when that is the model's.
    pub reply: Option<String>,
}

/// Files to attach (`App::take_files`, `App::pick_files`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Attached {
    /// Image and file blocks, as `App::attachments` holds them.
    #[serde(default)]
    pub files: Vec<Value>,
    /// Why each that couldn't be attached wasn't.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Oak's window: the `App` a part is given. What the plugin changed in
/// the message box goes back to Oak when this is dropped.
#[derive(Default)]
pub struct Window {
    /// The message box's text: as Oak had it, and as the plugin has it.
    input: Option<(String, String)>,
    attachments: Option<(Vec<Value>, Vec<Value>)>,
}

impl Window {
    /// Hands Oak what the plugin changed in the message box.
    fn flush(&mut self) {
        if let Some((was, now)) = &mut self.input
            && was != now
        {
            unsafe { (api().app_set_input)(s(now)) };
            *was = now.clone();
        }
        if let Some((was, now)) = &mut self.attachments
            && was != now
        {
            unsafe { (api().app_set_attachments)(s(&Value::from(now.clone()).to_string())) };
            *was = now.clone();
        }
    }

    /// Oak may change the message box in what follows: asks it again after.
    fn settle(&mut self) {
        self.flush();
        (self.input, self.attachments) = (None, None);
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if crate::rt::has_api() {
            self.flush();
        }
    }
}

impl App for Window {
    fn theme(&self) -> Theme {
        let mut t = OakTheme { size: std::mem::size_of::<OakTheme>() as u32, ..Default::default() };
        unsafe { (api().app_theme)(&mut t) };
        Theme::from_raw(&t)
    }

    fn open(&mut self, key: &str) {
        self.settle();
        unsafe { (api().app_open)(s(key)) };
    }

    fn close_tabs(&mut self, gone: &dyn Fn(&str) -> bool) {
        unsafe extern "C" fn ask(user: *mut c_void, key: OakStr) -> bool {
            let gone = unsafe { &*(user as *const &dyn Fn(&str) -> bool) };
            carry(|| gone(unsafe { str_of(key) })).unwrap_or(false)
        }
        self.settle();
        unsafe { (api().app_close_tabs)(ask, &gone as *const &dyn Fn(&str) -> bool as *mut c_void) };
        rethrow();
    }

    fn shown(&self) -> Option<String> {
        let (there, key) = text_from(|out, user| unsafe { (api().app_shown)(out, user) });
        there.then_some(key)
    }

    fn go_home(&mut self, tab: &str) {
        self.settle();
        unsafe { (api().app_go_home)(s(tab)) };
    }

    fn go_new(&mut self, kind: &str) {
        self.settle();
        unsafe { (api().app_go_new)(s(kind)) };
    }

    fn go_settings(&mut self) {
        self.settle();
        unsafe { (api().app_go_settings)() };
    }

    fn go_page(&mut self, id: &str) {
        self.settle();
        unsafe { (api().app_go_page)(s(id)) };
    }

    fn take_files(&mut self) -> Attached {
        let (some, json) = text_from(|out, user| unsafe { (api().take_files)(out, user) });
        if some { serde_json::from_str(&json).unwrap_or_default() } else { Attached::default() }
    }

    fn pick_files(&mut self, filter: &str, extensions: &[&str]) -> Option<Attached> {
        let (some, json) = text_from(|out, user| unsafe { (api().pick_files)(s(filter), s(&extensions.join("\n")), out, user) });
        some.then(|| serde_json::from_str(&json).unwrap_or_default())
    }

    fn hint(&mut self, text: Option<String>) {
        unsafe { (api().app_hint)(s(text.as_deref().unwrap_or(""))) };
    }

    fn input(&mut self) -> &mut String {
        let (_, now) = self.input.get_or_insert_with(|| {
            let text = text_from(|out, user| unsafe { (api().app_input)(out, user) }).1;
            (text.clone(), text)
        });
        now
    }

    fn insert_input(&mut self, text: &str, spaced: bool) {
        // (What the plugin changed goes first; Oak's box is asked again after.)
        self.settle();
        unsafe { (api().app_insert_input)(s(text), if spaced { OAK_INSERT_SPACED } else { 0 }) };
    }

    fn attachments(&mut self) -> &mut Vec<Value> {
        let (_, now) = self.attachments.get_or_insert_with(|| {
            let list: Vec<Value> = serde_json::from_str(&text_from(|out, user| unsafe { (api().app_attachments)(out, user) }).1).unwrap_or_default();
            (list.clone(), list)
        });
        now
    }

    fn conversations(&self) -> Vec<Conversation> {
        serde_json::from_str(&text_from(|out, user| unsafe { (api().app_conversations)(out, user) }).1).unwrap_or_default()
    }

    fn activity(&self) -> Activity {
        serde_json::from_str(&text_from(|out, user| unsafe { (api().app_activity)(out, user) }).1).unwrap_or_default()
    }

    fn chat_scene(&self) -> Option<ChatScene> {
        serde_json::from_str(&text_from(|out, user| unsafe { (api().app_chat_scene)(out, user) }).1).ok()
    }

    fn start_conversation(&mut self, new: NewConversation) -> Result<String, String> {
        self.settle();
        let json = serde_json::to_string(&new).unwrap_or_default();
        checked(|out, user| unsafe { (api().app_start_conversation)(s(&json), out, user) })
    }

    fn local_form(&mut self, ui: &mut Ui) {
        self.settle();
        unsafe { (api().app_local_form)(ui.as_raw()) };
    }

    fn start_sign_in(&mut self, provider: &str) -> Result<String, String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_start_sign_in)(s(provider), out, user) })
    }

    fn sign_in_form(&mut self, ui: &mut Ui) -> bool {
        self.settle();
        unsafe { (api().app_sign_in_form)(ui.as_raw()) }
    }

    fn private_browser(&mut self, ui: &mut Ui, url: &str) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_private_browser)(ui.as_raw(), s(url), out, user) }).map(|_| ())
    }

    fn open_background_browser(&mut self, url: &str) -> Result<crate::browser::Browser, String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_open_background_browser)(s(url), out, user) }).map(crate::browser::Browser::new)
    }

    fn background_browser_view(&mut self, ui: &mut Ui, browser: &crate::browser::Browser, max_h: f32) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_background_browser_view)(ui.as_raw(), s(browser.handle()), max_h, out, user) }).map(|_| ())
    }

    fn background_browser_live_view(&mut self, ui: &mut Ui, browser: &crate::browser::Browser, max_h: f32) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_background_browser_live_view)(ui.as_raw(), s(browser.handle()), max_h, out, user) }).map(|_| ())
    }

    fn transcript(&mut self, ui: &mut Ui, key: &str, version: u64, make: &mut dyn FnMut() -> Chat, tail_sig: u64, tail: &mut dyn FnMut(&mut Ui)) {
        unsafe extern "C" fn made(user: *mut c_void, out: OakSink, out_user: *mut c_void) {
            let make = unsafe { &mut *(user as *mut &mut dyn FnMut() -> Chat) };
            if let Some(chat) = carry(make) {
                let marks: Vec<Value> = chat.marks.iter().map(|m| json!({"at": m.at, "failed": m.failed, "text": m.text, "time": m.time})).collect();
                let text = json!({"messages": chat.messages, "marks": marks, "busy": chat.busy, "status": chat.status}).to_string();
                unsafe { out(out_user, text.as_ptr(), text.len()) };
            }
        }
        self.settle();
        let raw = ui.as_raw();
        let mut make = make;
        let make_user = &mut make as *mut &mut dyn FnMut() -> Chat as *mut c_void;
        with_body(
            |ui| tail(ui),
            |body, user| unsafe { (api().app_transcript)(raw, s(key), version, made, make_user, tail_sig, Some(body), user) },
        );
    }

    fn cloud_list(&mut self, ui: &mut Ui, backend: &str) {
        self.settle();
        unsafe { (api().app_cloud_list)(ui.as_raw(), s(backend)) };
    }

    fn session(&self, key: &str) -> Option<Session> {
        let (there, json) = text_from(|out, user| unsafe { (api().app_session)(s(key), out, user) });
        if !there {
            return None;
        }
        serde_json::from_str(&json).ok()
    }

    fn set_model(&mut self, key: &str, provider: &str, model: &str) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_set_model)(s(key), s(provider), s(model), out, user) }).map(drop)
    }

    fn send(&mut self, key: &str, text: &str) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_send)(s(key), s(text), out, user) }).map(drop)
    }

    fn interrupt(&mut self, key: &str) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_interrupt)(s(key), out, user) }).map(drop)
    }

    fn resume(&mut self, key: &str) -> Result<(), String> {
        self.settle();
        checked(|out, user| unsafe { (api().app_resume)(s(key), out, user) }).map(drop)
    }
}

/// What `bytes_from` is for outside this crate: the bytes a call hands its sink.
pub fn collect<R>(call: impl FnOnce(OakSink, *mut c_void) -> R) -> (R, Vec<u8>) {
    bytes_from(call)
}
