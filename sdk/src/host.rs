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
fn checked(call: impl FnOnce(OakSink, *mut c_void) -> bool) -> Result<String, String> {
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
    /// backend's id, "local").
    fn go_new(&mut self, kind: &str);
    /// Says `text` over the message box, in the error color (None clears it).
    fn hint(&mut self, text: Option<String>);
    /// The message box's text. What is changed in it reaches the box when
    /// the plugin's call returns, or the next time the `App` is used.
    fn input(&mut self) -> &mut String;
    /// What is attached to the message (image and file blocks); changes
    /// reach Oak as `input`'s do.
    fn attachments(&mut self) -> &mut Vec<Value>;
    /// This machine's conversations (not side chats, not subagents).
    fn conversations(&self) -> Vec<Conversation>;
    /// Starts a conversation on this machine and opens its tab. Err: why
    /// it couldn't (the folder is missing, say), also shown as the hint.
    fn start_conversation(&mut self, new: NewConversation) -> Result<String, String>;
    /// The New screen's rows for a conversation on this machine: the
    /// folder, the model and the effort.
    fn local_form(&mut self, ui: &mut Ui);
    /// Draws a transcript the plugin has, as a conversation's is drawn,
    /// read-only. `key` is its tab; `version` changes when the transcript
    /// does (`make` is called only then); `tail` draws what follows the
    /// last message, and `tail_sig` changes when that would draw otherwise.
    fn transcript(&mut self, ui: &mut Ui, key: &str, version: u64, make: &mut dyn FnMut() -> Chat, tail_sig: u64, tail: &mut dyn FnMut(&mut Ui));
    /// The list of backend `id`'s cloud sessions, as Home's Cloud tab
    /// shows Oak's own: a click opens the session.
    fn cloud_list(&mut self, ui: &mut Ui, backend: &str);
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

    fn start_conversation(&mut self, new: NewConversation) -> Result<String, String> {
        self.settle();
        let json = serde_json::to_string(&new).unwrap_or_default();
        checked(|out, user| unsafe { (api().app_start_conversation)(s(&json), out, user) })
    }

    fn local_form(&mut self, ui: &mut Ui) {
        self.settle();
        unsafe { (api().app_local_form)(ui.as_raw()) };
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
}

/// What `bytes_from` is for outside this crate: the bytes a call hands its sink.
pub fn collect<R>(call: impl FnOnce(OakSink, *mut c_void) -> R) -> (R, Vec<u8>) {
    bytes_from(call)
}
