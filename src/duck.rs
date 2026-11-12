//! Duck.ai, built in: a private chat through the real duck.ai page.
//!
//! A headless Brave, Edge or Chrome (its own profile in Oak's folder) keeps
//! one duck.ai tab open, driven over the DevTools protocol: files go in
//! through the page's own input, the text is typed into its chat box and
//! Send is clicked, as a person would. The answer is read from the page's own
//! network stream (`/duckchat/v1/chat`), so the tokens are duck.ai's, as they
//! arrive, with the reasoning kept apart. The model and the reasoning effort
//! are set by rewriting those two fields of the outgoing request. Nothing is
//! injected into the page's own scripts. One chat at a time, in memory only.

use crate::tools::Head;
use crate::cdp::{Cdp, Listener};
use base64::Engine as _;
use eframe::egui;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const DUCK_URL: &str = "https://duck.ai/";
const DUCK_ORIGIN: &str = "https://duck.ai";
const CHAT_PATH: &str = "/duckchat/v1/chat";
const CATALOG_URL: &str = "https://duck.ai/duckchat/v1/models";
/// The browser says it is an iPhone's: the page duck-api found to work.
const USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mozilla/5 Mobile/15E148 Version/15.0";
/// The longest an answer may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);
/// From Send to duck.ai's request leaving the page.
const SEND_TIMEOUT: Duration = Duration::from_secs(25);
/// A DevTools call's answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// The encoder the page's bodies travel in.
const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

// ------------------------------------------------------------------ errors

/// Why a turn failed.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// duck.ai wants a person to pick the ducks: solved once in a window.
    Captcha,
    /// duck.ai's rate or conversation limit.
    Limit(String),
    /// The browser is gone or hung (relaunched on the next try).
    Down(String),
    /// Anything else on the page or upstream.
    Page(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Captcha => write!(f, "duck.ai wants to check you're a person. Verify once in a browser window, then try again."),
            Error::Limit(s) | Error::Down(s) | Error::Page(s) => f.write_str(s),
        }
    }
}


// --------------------------------------------------------------- browser

/// Where the duck.ai browser keeps its profile (cookies, duck.ai's
/// settings): its own, in Oak's folder.
fn profile_dir() -> PathBuf {
    crate::store::dir().join("duck-profile")
}

/// Brave, then Edge, then Chrome.
fn find_browser() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OAK_DUCK_BROWSER")
        && Path::new(&p).is_file()
    {
        return Some(p.into());
    }
    #[cfg(windows)]
    {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let (local, pf, pf86) = (env("LOCALAPPDATA", ""), env("ProgramFiles", r"C:\Program Files"), env("ProgramFiles(x86)", r"C:\Program Files (x86)"));
        // (The browser's folder under an install root, its program.)
        let installs = [
            ("BraveSoftware\\Brave-Browser\\Application", "brave.exe"),
            ("Microsoft\\Edge\\Application", "msedge.exe"),
            ("Google\\Chrome\\Application", "chrome.exe"),
            ("Chromium\\Application", "chrome.exe"),
        ];
        // Browser by browser (Brave first), wherever it is installed.
        for (dir, exe) in installs {
            for root in [&local, &pf86, &pf] {
                let p = PathBuf::from(format!(r"{root}\{dir}\{exe}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    let names = ["brave-browser", "brave", "microsoft-edge", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser"];
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).flat_map(|dir| names.iter().map(move |n| dir.join(if cfg!(windows) { format!("{n}.exe") } else { n.to_string() }))).find(|p| p.is_file())
}

const NO_BROWSER: &str = "No Brave, Edge or Chrome found: duck.ai runs in one of them, hidden. Install one (or set OAK_DUCK_BROWSER to its path).";

/// The browser on Oak's own duck profile, hidden.
fn browser_cmd(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["--no-first-run", "--no-default-browser-check"])
        .arg(format!("--user-data-dir={}", profile_dir().display()))
        .arg(format!("--user-agent={USER_AGENT}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    cmd
}

/// The job object that ends the browser with Oak, even when Oak is killed.
#[cfg(windows)]
pub(crate) fn job_object(child: &Child) -> usize {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::*;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32);
        AssignProcessToJobObject(job, child.as_raw_handle() as _);
        job as usize
    }
}

/// A browser left running on the profile (Oak killed mid-chat, say) holds
/// its lock, and a new one would quit at once: end those. Only processes
/// on Oak's duck profile, never the user's own browser.
fn reap_stale(exe: &Path) {
    #[cfg(windows)]
    {
        let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='{name}'\" | Where-Object {{ $_.CommandLine -like '*--user-data-dir=*{}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}",
            profile_dir().display()
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = exe;
        let _ = Command::new("pkill").args(["-f", &format!("--user-data-dir={}", profile_dir().display())]).status();
    }
}

/// The headless browser. Dropping it ends it, and all it started (a job
/// object on Windows ends them even if Oak itself is killed).
struct Browser {
    child: Child,
    port: u16,
    #[cfg(windows)]
    job: usize,
}

impl Browser {
    fn launch() -> Result<Browser, Error> {
        let exe = find_browser().ok_or_else(|| Error::Page(NO_BROWSER.into()))?;
        std::fs::create_dir_all(profile_dir()).map_err(|e| Error::Page(format!("Couldn't make the browser profile folder: {e}")))?;
        reap_stale(&exe);
        let mut cmd = browser_cmd(&exe);
        cmd.args(["--disable-gpu", "--disable-dev-shm-usage", "--headless=new"])
            .arg("--remote-debugging-port=0")
            .arg("about:blank")
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| Error::Page(format!("Couldn't start {}: {e}", exe.display())))?;
        #[cfg(windows)]
        let job = job_object(&child);
        // The DevTools port, from the address the browser prints.
        let stderr = child.stderr.take().expect("piped");
        let (_, rx) = mpsc::channel();
        let rx = crate::cdp::devtools_address(stderr, "duck-browser-log", "ws://127.0.0.1:").unwrap_or(rx);
        let mut b = Browser {
            child,
            port: 0,
            #[cfg(windows)]
            job,
        };
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ws) => match ws[15..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u16>() {
                    Ok(port) => {
                        b.port = port;
                        return Ok(b);
                    }
                    Err(_) => return Err(Error::Page("The browser printed no DevTools port.".into())),
                },
                Err(_) if !b.alive() => return Err(Error::Page("The browser quit as it started (is its profile in use?).".into())),
                Err(_) if Instant::now() > deadline => return Err(Error::Page("The browser didn't start in time.".into())),
                Err(_) => {}
            }
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// A new tab, and a DevTools session with it.
    fn new_tab(&self) -> Result<Arc<Cdp>, Error> {
        let url = format!("http://127.0.0.1:{}/json/new?about:blank", self.port);
        let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).no_proxy().build().map_err(|e| Error::Page(e.to_string()))?;
        let target: Value = client
            .put(&url)
            .send()
            .and_then(|r| r.json())
            .map_err(|e| Error::Down(format!("Couldn't open a tab in the browser: {e}")))?;
        let ws = target["webSocketDebuggerUrl"].as_str().ok_or_else(|| Error::Down("The browser gave no tab address.".into()))?;
        Cdp::connect(ws)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job as _);
        }
    }
}

// ------------------------------------------------------------ page script

/// Masks the plain signs of a driven browser before any page script runs.
const STEALTH_JS: &str = r#"(() => {
  try { Object.defineProperty(navigator, 'webdriver', { get: () => false }); } catch (e) {}
  try { window.chrome = window.chrome || {}; window.chrome.runtime = window.chrome.runtime || {}; } catch (e) {}
  try {
    const q = window.navigator.permissions && window.navigator.permissions.query;
    if (q) window.navigator.permissions.query = (p) => p && p.name === 'notifications' ? Promise.resolve({ state: Notification.permission }) : q(p);
  } catch (e) {}
  try { Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] }); } catch (e) {}
})();"#;
/// duck.ai's own "Recent chats" off: the page keeps no chats.
const NO_HISTORY_JS: &str = r#"try { if (location.origin === 'https://duck.ai') localStorage.setItem('isRecentChatsOn', '"0"'); } catch (e) {}"#;
const COMPOSER: &str = r#"(document.querySelector('textarea[aria-label="Ask anything privately"]')||document.querySelector('textarea[aria-label^="Reply"]')||document.querySelector('textarea'))"#;
const STOP_BTN: &str = r#"Array.from(document.querySelectorAll('button[aria-label="Stop generating"]')).find(b => !b.disabled && b.getBoundingClientRect().width > 0 && getComputedStyle(b).visibility !== 'hidden')"#;
const SEND_JS: &str = r#"(() => { const b = document.querySelector('button[aria-label="Send"]') || document.querySelector('button[type="submit"]'); if (!b || b.disabled) return false; b.click(); return true; })()"#;
const CLEAR_ATTACHMENTS_JS: &str = r#"(() => { const bs = Array.from(document.querySelectorAll('button')).filter(b => (b.getAttribute('aria-label') || '').startsWith('Remove ')); bs.forEach(b => b.click()); return bs.length; })()"#;

fn page_state_js() -> String {
    format!(
        r#"(() => {{
  const ta = {COMPOSER};
  const t = document.body ? (document.body.innerText || '') : '';
  const labels = Array.from(document.querySelectorAll('button')).map(b => b.getAttribute('aria-label') || '');
  return JSON.stringify({{
    composer: !!ta,
    busy: !!({STOP_BTN}),
    attached: labels.filter(l => l.startsWith('Remove ')).length,
    captcha: /select all squares|select (all )?the ducks/i.test(t),
    tail: t.slice(-300),
  }});
}})()"#
    )
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct PageState {
    composer: bool,
    busy: bool,
    attached: usize,
    captcha: bool,
    tail: String,
}

// ---------------------------------------------------------- stream parser

/// What the chat stream brings.
#[derive(Debug, PartialEq)]
enum Piece {
    Reasoning(String),
    Content(String),
    /// A tool duck.ai's model called (a web search, say): its call id,
    /// name and arguments.
    ToolCall { id: String, name: String, input: Value },
    /// What it found: its text, the sources it numbered for the answer's
    /// `<citation src="N">` marks, and whether it failed.
    ToolResult { id: String, text: String, citations: Vec<Citation>, error: bool },
    /// A source the answer used, as duck.ai lists them under it.
    Source(Citation),
}

/// A web page an answer cites.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Citation {
    /// Its number in the answer's citation marks (0 for a listed source).
    pub id: u64,
    pub url: String,
    pub title: String,
    /// The site's name ("AccuWeather"), or its domain.
    pub site: String,
}

impl Citation {
    fn from(v: &Value) -> Option<Citation> {
        let url = v["url"].as_str()?.to_string();
        let domain = || url.split("://").nth(1).unwrap_or(&url).split('/').next().unwrap_or("").trim_start_matches("www.").to_string();
        let site = [&v["site_name"], &v["site"], &v["root_domain"], &v["domain"]]
            .into_iter()
            .filter_map(|s| s.as_str())
            .map(|s| s.trim_start_matches("www.").trim())
            .find(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(domain);
        Some(Citation { id: v["id"].as_u64().unwrap_or(0), title: v["title"].as_str().unwrap_or("").trim().to_string(), site, url })
    }
}

/// duck.ai's conversation limit: the chat is full, and a new one is the way
/// past it.
fn conversation_limit() -> Error {
    Error::Limit("duck.ai's conversation limit: start a new chat (the fire button).".into())
}

/// duck.ai's `/duckchat/v1/chat` stream, fed as it arrives:
///   data: {"action":"success","role":"reasoning","state":"text-delta","partIndex":0,"text":"..."}
///   data: {"action":"success","role":"assistant","message":"..."}
///   data: {"action":"error","status":429,"type":"ERR_CONVERSATION_LIMIT"}
///   data: [CHAT_TITLE:...], [DONE]
#[derive(Default)]
struct Parser {
    buf: Vec<u8>,
    part: Option<Value>,
    done: bool,
}

impl Parser {
    fn feed(&mut self, data: &[u8]) -> Result<Vec<Piece>, Error> {
        // (OAK_DUCK_DUMP=file: the raw stream, appended, for working out
        // what duck.ai sends.)
        if let Some(p) = std::env::var_os("OAK_DUCK_DUMP")
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
        {
            let _ = f.write_all(data);
        }
        self.buf.extend_from_slice(data);
        let mut out = vec![];
        while !self.done
            && let Some(i) = self.buf.iter().position(|&b| b == b'\n')
        {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(p) = self.line(line.trim_end_matches(['\r', '\n']))? {
                out.push(p);
            }
        }
        Ok(out)
    }

    fn line(&mut self, line: &str) -> Result<Option<Piece>, Error> {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else { return Ok(None) };
        if data == "[DONE]" {
            self.done = true;
            return Ok(None);
        }
        if data.is_empty() || data.starts_with('[') {
            return Ok(None);
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return Ok(None) };
        if v["action"] == "error" {
            let kind = v["type"].as_str().or(v["error"].as_str()).unwrap_or("unknown error").to_string();
            return Err(match v["status"].as_u64() {
                Some(429) if kind == "ERR_CONVERSATION_LIMIT" => conversation_limit(),
                Some(429) => Error::Limit(format!("duck.ai's rate limit ({kind}); try again in a little while.")),
                Some(s) => Error::Page(format!("duck.ai error: {kind} (status {s})")),
                None => Error::Page(format!("duck.ai error: {kind}")),
            });
        }
        match v["role"].as_str() {
            Some("assistant") => Ok(v["message"].as_str().filter(|m| !m.is_empty()).map(|m| Piece::Content(m.to_string()))),
            Some("tool-invocation") => {
                let id = v["toolCallId"].as_str().unwrap_or_default().to_string();
                match v["state"].as_str() {
                    Some("call") => {
                        let raw = &v["toolArguments"];
                        let input = match raw.as_str() {
                            Some(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({"arguments": s})),
                            None if raw.is_object() => raw.clone(),
                            None => json!({}),
                        };
                        Ok(Some(Piece::ToolCall { id, name: v["toolName"].as_str().unwrap_or("Tool").to_string(), input }))
                    }
                    Some("result") => {
                        let citations = v["data"]["citations"].as_array().into_iter().flatten().filter_map(Citation::from).collect();
                        Ok(Some(Piece::ToolResult { id, text: v["result"].as_str().unwrap_or("").to_string(), citations, error: false }))
                    }
                    Some("error") => {
                        let text = v["error"].as_str().or(v["result"].as_str()).or(v["message"].as_str()).unwrap_or("The tool failed.").to_string();
                        Ok(Some(Piece::ToolResult { id, text, citations: vec![], error: true }))
                    }
                    _ => Ok(None),
                }
            }
            Some("source") => Ok(Citation::from(&v["source"]).map(Piece::Source)),
            Some("reasoning") if v["state"] == "text-delta" => {
                let Some(text) = v["text"].as_str().filter(|t| !t.is_empty()) else { return Ok(None) };
                let part = v["partIndex"].clone();
                // Parts of the reasoning summary, apart as the web page shows them.
                let text = if self.part.as_ref().is_some_and(|p| *p != part) { format!("\n\n{text}") } else { text.to_string() };
                self.part = Some(part);
                Ok(Some(Piece::Reasoning(text)))
            }
            _ => Ok(None),
        }
    }
}

// ----------------------------------------------------------------- models

/// A model duck.ai's free plan offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    pub name: String,
    /// Its reasoning efforts ("none" is Fast).
    pub efforts: Vec<String>,
    pub images: bool,
    pub pdf: bool,
}

impl Model {
    /// Both Fast and Reasoning.
    pub fn can_toggle(&self) -> bool {
        self.efforts.iter().any(|e| e == "none") && self.efforts.iter().any(|e| e != "none")
    }

    /// Whether it reasons when `reasoning` is asked for.
    pub fn reasons(&self, reasoning: bool) -> bool {
        match self.effort(reasoning) {
            Some(e) => e != "none",
            None => false,
        }
    }

    /// The effort sent: "none" for Fast, "low" (what the page's toggle
    /// sends) for Reasoning; one the model lacks goes to the nearest it has.
    /// None: the model takes no effort at all.
    pub fn effort(&self, reasoning: bool) -> Option<String> {
        let wanted = if reasoning { "low" } else { "none" };
        let pick = |e: &str| self.efforts.iter().find(|x| *x == e).cloned();
        if let Some(e) = pick(wanted) {
            return Some(e);
        }
        if reasoning {
            return self.efforts.iter().find(|e| *e != "none").or(self.efforts.first()).cloned();
        }
        self.efforts.first().cloned()
    }
}

/// The free models as of 2026-11, until the live list comes.
fn builtin_models() -> Vec<Model> {
    let m = |id: &str, name: &str, efforts: &[&str], images: bool, pdf: bool| Model {
        id: id.into(),
        name: name.into(),
        efforts: efforts.iter().map(|e| e.to_string()).collect(),
        images,
        pdf,
    };
    [
        m("gpt-6-luna", "GPT-6 Luna", &["none", "low"], true, true),
        m("gpt-5.4-mini", "GPT-5.4 mini", &["none", "low", "medium"], true, true),
        m("claude-haiku-4-5", "Claude Haiku 4.5", &["none", "low"], true, true),
        m("mistral-small-2603", "Mistral Small 4", &[], false, false),
        m("tinfoil/gpt-oss-120b", "gpt-oss 120B", &["low"], false, false),
        m("tinfoil/gemma4-31b", "Gemma 4 31B", &["none", "low"], true, false),
    ]
    .into()
}

/// The model the page picks when none is chosen.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// duck.ai's live list (public): the models this plan has access to.
fn parse_catalog(v: &Value) -> Vec<Model> {
    v["models"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|m| m["entityHasAccess"] != false && m["id"].is_string())
        .map(|m| Model {
            id: m["id"].as_str().unwrap_or_default().into(),
            name: m["name"].as_str().or(m["id"].as_str()).unwrap_or_default().into(),
            efforts: m["supportedReasoningEffort"].as_array().map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect()).unwrap_or_default(),
            images: m["supportsImageUpload"] == true,
            pdf: m["supportedFileTypes"].as_array().is_some_and(|a| a.iter().any(|t| t == "application/pdf")),
        })
        .collect()
}

static MODELS: Mutex<Option<Vec<Model>>> = Mutex::new(None);
static MODELS_ASKED: AtomicBool = AtomicBool::new(false);

/// The models to pick from: the live list once it has come, else the
/// built-in one. The first call fetches the live list.
pub fn models(ctx: &egui::Context) -> Vec<Model> {
    if !MODELS_ASKED.swap(true, Ordering::SeqCst) {
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let got = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .and_then(|c| c.get(CATALOG_URL).header("User-Agent", USER_AGENT).send())
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<Value>());
            if let Ok(v) = got {
                let list = parse_catalog(&v);
                if !list.is_empty() {
                    *MODELS.lock().unwrap() = Some(list);
                    ctx.request_repaint();
                }
            }
        });
    }
    MODELS.lock().unwrap().clone().unwrap_or_else(builtin_models)
}

// ------------------------------------------------------------ attachments

/// A file for the page's upload input.
#[derive(Clone)]
pub struct Upload {
    pub name: String,
    pub data: Vec<u8>,
}

/// Oak's attachment blocks as duck.ai takes them: images and PDFs as files
/// to upload, text files folded into the message. Errors name what
/// duck.ai can't take.
pub fn uploads(blocks: &[Value], model: &Model) -> Result<(Vec<Upload>, String), String> {
    let b64 = B64;
    let (mut files, mut text) = (vec![], String::new());
    for (i, b) in blocks.iter().enumerate() {
        let name = b["_oak_file"].as_str().or(b["title"].as_str()).map(String::from);
        match (b["type"].as_str(), b["media_type"].as_str()) {
            (Some("image"), Some(mt)) => {
                if !model.images {
                    return Err(format!("{} doesn't take images on duck.ai.", model.name));
                }
                let data = b64.decode(b["data"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
                if data.len() > 5 << 20 {
                    return Err("duck.ai takes images up to 5 MB.".into());
                }
                let ext = mt.rsplit('/').next().unwrap_or("png").replace("jpeg", "jpg");
                files.push(Upload { name: format!("image-{}.{ext}", i + 1), data });
            }
            (Some("document"), Some("application/pdf")) => {
                if !model.pdf {
                    return Err(format!("{} doesn't take PDFs on duck.ai.", model.name));
                }
                let data = b64.decode(b["data"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
                let mut name = name.unwrap_or_else(|| format!("document-{}.pdf", i + 1));
                if !name.to_lowercase().ends_with(".pdf") {
                    name.push_str(".pdf");
                }
                files.push(Upload { name, data });
            }
            (Some("document"), _) => {
                let body = b["data"].as_str().unwrap_or("");
                text.push_str(&format!("\n\n--- {} ---\n{body}", name.as_deref().unwrap_or("file")));
            }
            _ => return Err(format!("duck.ai takes images, PDFs and text files; {} is none of those.", name.as_deref().unwrap_or("an attachment"))),
        }
    }
    Ok((files, text))
}

// ------------------------------------------------------------ citations

/// A citation mark in an answer: where it starts and ends, and the numbers
/// it cites.
type Mark = (usize, usize, Vec<u64>);

/// The citation marks in an answer, and where a mark still coming in
/// starts, if one is.
fn citation_marks(text: &str) -> (Vec<Mark>, Option<usize>) {
    const OPEN: &str = "<citation";
    const CLOSE: &str = "</citation>";
    let mut out = vec![];
    let mut at = 0;
    while let Some(i) = text[at..].find(OPEN).map(|i| i + at) {
        let Some(gt) = text[i..].find('>').map(|g| g + i) else { return (out, Some(i)) };
        let tag = &text[i..=gt];
        let end = if tag.ends_with("/>") {
            gt + 1
        } else {
            match text[gt + 1..].find(CLOSE) {
                Some(c) => gt + 1 + c + CLOSE.len(),
                None => return (out, Some(i)),
            }
        };
        let src = tag.split_once("src=").map(|(_, v)| v.trim_start_matches(['"', '\'']).split(['"', '\'']).next().unwrap_or("")).unwrap_or("");
        let ids = src.split(',').filter_map(|n| n.trim().parse().ok()).collect();
        out.push((i, end, ids));
        at = end;
    }
    // "<", "<cit"...: the start of a mark, cut by the stream.
    let partial = text[at..].rfind('<').map(|i| i + at).filter(|&i| OPEN.starts_with(&text[i..]));
    (out, partial)
}

/// An answer for the screen: each citation mark as chips naming the sites
/// it cites, linking to them (markdown links titled "cite", which
/// `markdown` draws as chips), and a mark still streaming in left out.
pub fn with_citations(text: &str, cites: &[Citation]) -> String {
    let (marks, partial) = citation_marks(text);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, ids) in marks {
        out.push_str(&text[at..start]);
        let mut seen = vec![];
        let chips: Vec<String> = ids
            .iter()
            .filter_map(|id| cites.iter().find(|c| c.id == *id))
            .filter(|c| {
                let new = !seen.contains(&c.url);
                seen.push(c.url.clone());
                new
            })
            .map(|c| {
                let name: String = c.site.chars().filter(|ch| !"[]*_`\\<>".contains(*ch)).collect();
                // (No-break spaces pad the chip's background.)
                format!("[\u{a0}{}\u{a0}](<{}> \"cite\")", name, c.url.replace(['<', '>', ' '], ""))
            })
            .collect();
        // (A no-break space too: chips side by side stay apart.)
        out.push_str(&chips.join("\u{a0} "));
        at = end;
    }
    out.push_str(&text[at..partial.unwrap_or(text.len())]);
    out
}

/// The pages an answer's marks cite, in order, each once.
pub fn cited<'a>(text: &str, cites: &'a [Citation]) -> Vec<&'a Citation> {
    let mut out: Vec<&Citation> = vec![];
    for id in citation_marks(text).0.into_iter().flat_map(|m| m.2) {
        if let Some(c) = cites.iter().find(|c| c.id == id)
            && !out.iter().any(|x| x.url == c.url)
        {
            out.push(c);
        }
    }
    out
}

/// An answer without its citation marks (for the transcript typed into a
/// new chat).
fn strip_citations(text: &str) -> String {
    with_citations(text, &[]).replace(" .", ".").trim_end().to_string()
}

// ------------------------------------------------------------ the driver

/// One turn for the page.
struct Job {
    /// The Oak chat it belongs to.
    round: u64,
    /// The chat so far, (user, text): typed in as a transcript if the page
    /// lost it (the browser was relaunched, say).
    before: Vec<(bool, String)>,
    /// Files sent earlier in the chat, for that too.
    before_files: Vec<Upload>,
    text: String,
    files: Vec<Upload>,
    model: String,
    effort: Option<String>,
    cancel: Arc<AtomicBool>,
}

/// The chat so far as one message, as a new chat takes it back.
fn transcript(job: &Job) -> String {
    let mut lines: Vec<String> = job
        .before
        .iter()
        .filter(|(_, t)| !t.trim().is_empty())
        .map(|(user, t)| format!("{}: {}", if *user { "User" } else { "Assistant" }, t.trim()))
        .collect();
    lines.push(job.text.clone());
    lines.join("\n\n")
}

#[derive(Default)]
struct Driver {
    browser: Option<Browser>,
    cdp: Option<Arc<Cdp>>,
    /// The page shows an empty new chat.
    fresh: bool,
    /// The chat the page holds: Oak's chat and how many messages were sent.
    held: Option<(u64, usize)>,
    /// Files attached in the page's chat (the page reads them again).
    uploads: Option<PathBuf>,
}

static DRIVER: Mutex<Option<Driver>> = Mutex::new(None);

impl Driver {
    fn cdp(&self) -> Result<Arc<Cdp>, Error> {
        self.cdp.clone().ok_or_else(|| Error::Down("The browser isn't running.".into()))
    }

    fn shutdown(&mut self) {
        self.drop_uploads();
        if let Some(c) = self.cdp.take() {
            c.close();
        }
        self.browser = None;
        self.held = None;
        self.fresh = false;
    }

    fn drop_uploads(&mut self) {
        if let Some(d) = self.uploads.take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    fn ensure_started(&mut self) -> Result<(), Error> {
        if let (Some(b), Some(c)) = (self.browser.as_mut(), self.cdp.as_ref())
            && b.alive()
            && !c.is_closed()
            && c.eval_for("1", Duration::from_secs(3)).is_ok_and(|v| v == 1)
        {
            return Ok(());
        }
        self.shutdown();
        let browser = Browser::launch()?;
        let cdp = browser.new_tab()?;
        self.browser = Some(browser);
        self.cdp = Some(cdp.clone());
        let setup = (|| {
            cdp.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": STEALTH_JS}))?;
            cdp.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": NO_HISTORY_JS}))?;
            for d in ["Page", "Network", "DOM"] {
                cdp.call(&format!("{d}.enable"), json!({}))?;
            }
            self.navigate_home()
        })();
        if setup.is_err() {
            self.shutdown();
        }
        setup
    }

    fn state(&self) -> Result<PageState, Error> {
        let cdp = self.cdp()?;
        match cdp.eval_for(&page_state_js(), Duration::from_secs(5)) {
            Ok(v) => Ok(v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()),
            Err(e @ Error::Down(_)) => Err(e),
            Err(_) => Ok(PageState::default()),
        }
    }

    fn wait_for(&self, timeout: Duration, what: &str, ok: impl Fn(&PageState) -> bool) -> Result<PageState, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.state()?;
            if ok(&s) {
                return Ok(s);
            }
            if Instant::now() > deadline {
                let tail: String = s.tail.chars().rev().take(160).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(Error::Page(format!("{what} (the page ends: {tail:?})")));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn navigate_home(&mut self) -> Result<(), Error> {
        self.cdp()?.call("Page.navigate", json!({"url": DUCK_URL}))?;
        let s = self.wait_for(Duration::from_secs(30), "duck.ai's chat box never appeared", |s| s.composer || s.captcha)?;
        if s.captcha {
            return Err(Error::Captcha);
        }
        self.fresh = true;
        self.held = None;
        Ok(())
    }

    /// An empty chat, with nothing of the last one left: what duck.ai
    /// stored in the browser is cleared, and the page loaded again.
    fn reset(&mut self) -> Result<(), Error> {
        self.drop_uploads();
        let cdp = self.cdp()?;
        let _ = cdp.call("Storage.clearDataForOrigin", json!({"origin": DUCK_ORIGIN, "storageTypes": "indexeddb,cache_storage,service_workers"}));
        self.navigate_home()
    }

    fn attach(&mut self, files: &[Upload]) -> Result<(), Error> {
        let dir = self.uploads.get_or_insert_with(|| std::env::temp_dir().join(format!("oak-duck-{}-{}", std::process::id(), rand::random::<u32>()))).clone();
        let mut paths = vec![];
        for f in files {
            // A folder each: one name twice stays two files.
            let sub = dir.join(format!("{:08x}", rand::random::<u32>()));
            std::fs::create_dir_all(&sub).map_err(|e| Error::Page(e.to_string()))?;
            let p = sub.join(&f.name);
            std::fs::write(&p, &f.data).map_err(|e| Error::Page(e.to_string()))?;
            paths.push(p.display().to_string());
        }
        let cdp = self.cdp()?;
        let root = cdp.call("DOM.getDocument", json!({"depth": 0}))?["root"]["nodeId"].clone();
        let node = cdp.call("DOM.querySelector", json!({"nodeId": root, "selector": "input[type=\"file\"]"}))?["nodeId"].as_u64().unwrap_or(0);
        if node == 0 {
            return Err(Error::Page("duck.ai's upload button wasn't found.".into()));
        }
        cdp.call("DOM.setFileInputFiles", json!({"nodeId": node, "files": paths}))?;
        let n = paths.len();
        self.wait_for(Duration::from_secs(20), &format!("duck.ai didn't take the {n} file(s)"), |s| s.attached >= n)?;
        // The page converts images before it can send them.
        std::thread::sleep(Duration::from_millis(300));
        Ok(())
    }

    /// One turn: its reasoning and answer go to `on` as they come.
    fn turn(&mut self, job: &Job, on: &mut dyn FnMut(Piece)) -> Result<(), Error> {
        self.ensure_started()?;
        let sent = job.before.iter().filter(|m| m.0).count();
        let (text, files) = if self.held == Some((job.round, sent)) {
            (job.text.clone(), job.files.clone())
        } else {
            // A new chat; one the page lost goes in as a transcript.
            if !self.fresh {
                self.reset()?;
            }
            (transcript(job), [job.before_files.clone(), job.files.clone()].concat())
        };
        if text.trim().is_empty() && files.is_empty() {
            return Err(Error::Page("Nothing to send.".into()));
        }
        self.wait_for(Duration::from_secs(20), "duck.ai is still busy with the last answer", |s| s.composer && !s.busy)?;
        let cdp = self.cdp()?;
        if self.state()?.attached > 0 {
            cdp.eval_for(CLEAR_ATTACHMENTS_JS, CALL_TIMEOUT)?;
        }
        let mut answered = false;
        let result = (|| {
            if !files.is_empty() {
                self.attach(&files)?;
            }
            self.send_and_stream(&cdp, &text, job, &mut |p| {
                answered |= matches!(p, Piece::Content(_));
                on(p)
            })
        })();
        self.fresh = false;
        match result {
            // Stopped before duck.ai answered: what the page holds is unknown.
            Ok(stopped) if stopped && !answered => self.held = None,
            Ok(_) => self.held = Some((job.round, sent + 1)),
            Err(e) => {
                self.held = None;
                return Err(e);
            }
        }
        Ok(())
    }

    /// Types `text`, clicks Send and streams the answer. True when it was
    /// stopped.
    fn send_and_stream(&self, cdp: &Arc<Cdp>, text: &str, job: &Job, on: &mut dyn FnMut(Piece)) -> Result<bool, Error> {
        let events = cdp.listen();
        cdp.call("Fetch.enable", json!({"patterns": [{"urlPattern": format!("*{CHAT_PATH}*"), "requestStage": "Request"}]}))?;
        let result = (|| {
            if cdp.eval_for(&format!("(() => {{ const e = {COMPOSER}; if (!e) return false; e.focus(); return true; }})()"), CALL_TIMEOUT)? != true {
                return Err(Error::Page("duck.ai's chat box wasn't found.".into()));
            }
            if !text.is_empty() {
                cdp.call("Input.insertText", json!({"text": text}))?;
            }
            std::thread::sleep(Duration::from_millis(200));
            if cdp.eval_for(SEND_JS, CALL_TIMEOUT)? != true {
                // Send enables a beat after the text goes in.
                std::thread::sleep(Duration::from_millis(800));
                if cdp.eval_for(SEND_JS, CALL_TIMEOUT)? != true {
                    return Err(Error::Page("duck.ai's Send button stayed off.".into()));
                }
            }
            self.pump(cdp, &events, job, on)
        })();
        let _ = cdp.call_for("Fetch.disable", json!({}), Duration::from_secs(5));
        result
    }

    fn pump(&self, cdp: &Arc<Cdp>, events: &Listener, job: &Job, on: &mut dyn FnMut(Piece)) -> Result<bool, Error> {
        let mut seen = Stream { parser: Parser::default(), rid: None, status: None, any: false, stopped: false, on };
        let send_deadline = Instant::now() + SEND_TIMEOUT;
        let mut deadline = Instant::now() + ANSWER_TIMEOUT;
        let mut stopped = false;
        loop {
            if job.cancel.load(Ordering::SeqCst) && !stopped {
                stopped = true;
                seen.stopped = true;
                let stop = format!("(() => {{ const b = {STOP_BTN}; if (b) b.click(); return !!b; }})()");
                let _ = cdp.eval_for(&stop, Duration::from_secs(5));
                if seen.rid.is_none() {
                    return Ok(true);
                }
                // The page winds its stream down.
                deadline = deadline.min(Instant::now() + Duration::from_secs(5));
            }
            let now = Instant::now();
            if now > deadline {
                if stopped {
                    return Ok(true);
                }
                return Err(Error::Page(format!("No whole answer within {}s.", ANSWER_TIMEOUT.as_secs())));
            }
            if seen.rid.is_none() && now > send_deadline {
                if self.state()?.captcha {
                    return Err(Error::Captcha);
                }
                return Err(Error::Page("duck.ai didn't send the message.".into()));
            }
            let Ok(ev) = events.rx.recv_timeout(Duration::from_millis(250)) else { continue };
            if matches!(seen.event(self, cdp, &ev, &job.model, job.effort.as_deref())?, End::Break) || seen.parser.done {
                break;
            }
        }
        Ok(stopped && !seen.parser.done)
    }

    /// A paused request: the chat request goes on with Oak's model and
    /// effort; anything else unchanged.
    fn rewrite(&self, cdp: &Cdp, p: &Value, model: &str, effort: Option<&str>) -> Result<(), Error> {
        let id = p["requestId"].clone();
        let go_on = || cdp.call("Fetch.continueRequest", json!({"requestId": id})).map(|_| ());
        if !is_chat(&p["request"]) {
            return go_on();
        }
        let mut raw = p["request"]["postData"].as_str().map(String::from);
        if raw.is_none()
            && let Some(nid) = p["networkId"].as_str()
        {
            raw = cdp.call("Network.getRequestPostData", json!({"requestId": nid})).ok().and_then(|r| r["postData"].as_str().map(String::from));
        }
        let Some(mut body) = raw.and_then(|r| serde_json::from_str::<Value>(&r).ok()).filter(Value::is_object) else {
            return go_on();
        };
        let before = body.clone();
        body["model"] = json!(model);
        match effort {
            Some(e) => body["reasoningEffort"] = json!(e),
            None => {
                body.as_object_mut().map(|o| o.remove("reasoningEffort"));
            }
        }
        if body == before {
            return go_on();
        }
        let data = B64.encode(body.to_string());
        cdp.call("Fetch.continueRequest", json!({"requestId": id, "postData": data})).map(|_| ())
    }
}

/// What a streamed answer has brought so far.
struct Stream<'a> {
    parser: Parser,
    /// The chat request in the page (its id), and its status.
    rid: Option<String>,
    status: Option<u64>,
    /// Some of the answer came, and it was stopped.
    any: bool,
    stopped: bool,
    on: &'a mut dyn FnMut(Piece),
}

/// Whether the answer is over.
enum End {
    Break,
    GoOn,
}

impl Stream<'_> {
    /// Reads one event of the page's. (An Err is the turn's end.)
    fn event(&mut self, d: &Driver, cdp: &Cdp, ev: &Value, model: &str, effort: Option<&str>) -> Result<End, Error> {
        let p = &ev["params"];
        let mine = self.rid.is_some() && p["requestId"].as_str() == self.rid.as_deref();
        match (ev["method"].as_str().unwrap_or(""), mine) {
            ("__closed__", _) => Err(Error::Down("The browser's connection was lost.".into())),
            ("Fetch.requestPaused", _) => {
                d.rewrite(cdp, p, model, effort)?;
                Ok(End::GoOn)
            }
            ("Network.requestWillBeSent", _) if self.rid.is_none() && is_chat(&p["request"]) => {
                let id = p["requestId"].as_str().unwrap_or_default().to_string();
                let res = cdp.call("Network.streamResourceContent", json!({"requestId": id}))?;
                self.rid = Some(id);
                if let Some(data) = res["bufferedData"].as_str().filter(|d| !d.is_empty()) {
                    self.feed(data)?;
                }
                Ok(End::GoOn)
            }
            ("Network.responseReceived", true) => {
                self.status = p["response"]["status"].as_u64();
                Ok(End::GoOn)
            }
            ("Network.dataReceived", true) => {
                if let Some(data) = p["data"].as_str().filter(|d| !d.is_empty()) {
                    self.feed(data)?;
                }
                Ok(End::GoOn)
            }
            ("Network.loadingFinished" | "Network.loadingFailed", true) => {
                if let Some(status) = self.status.filter(|&s| s != 200) {
                    return Err(http_error(cdp, self.rid.as_deref().unwrap_or_default(), status));
                }
                if self.parser.done || self.any || self.stopped {
                    return Ok(End::Break);
                }
                Err(Error::Page(format!("duck.ai's answer ended early: {}", p["errorText"].as_str().unwrap_or("no data"))))
            }
            _ => Ok(End::GoOn),
        }
    }

    /// Adds a base64'd piece of the page's stream, telling `on` what it
    /// brought.
    fn feed(&mut self, data: &str) -> Result<(), Error> {
        for piece in self.parser.feed(&B64.decode(data).unwrap_or_default())? {
            self.any = true;
            (self.on)(piece);
        }
        Ok(())
    }
}

fn is_chat(req: &Value) -> bool {
    req["method"] == "POST" && req["url"].as_str().is_some_and(|u| u.split(['?', '#']).next().is_some_and(|u| u.ends_with(CHAT_PATH)))
}

fn http_error(cdp: &Cdp, rid: &str, status: u64) -> Error {
    let body = cdp
        .call("Network.getResponseBody", json!({"requestId": rid}))
        .ok()
        .map(|r| {
            let b = r["body"].as_str().unwrap_or_default();
            if r["base64Encoded"] == true { String::from_utf8_lossy(&B64.decode(b).unwrap_or_default()).into_owned() } else { b.to_string() }
        })
        .unwrap_or_default();
    let short: String = body.head(200);
    match status {
        429 if body.contains("ERR_CONVERSATION_LIMIT") => conversation_limit(),
        429 => Error::Limit(format!("duck.ai's rate limit; try again in a little while. {short}")),
        418 => Error::Page("duck.ai turned down the browser's check (418). Try again, or verify in a browser window.".into()),
        _ => Error::Page(format!("duck.ai answered HTTP {status}: {short}")),
    }
}

/// Runs `f` with the driver (made the first time).
fn with_driver<R>(f: impl FnOnce(&mut Driver) -> R) -> R {
    let mut d = DRIVER.lock().unwrap_or_else(|e| e.into_inner());
    f(d.get_or_insert_with(Driver::default))
}

// --------------------------------------------------------- the chat (UI)

/// A message in the chat.
#[derive(Clone, Default)]
pub struct Msg {
    pub user: bool,
    pub text: String,
    /// What was sent for a user's message: its text with the text files
    /// attached to it.
    sent: String,
    /// The model's reasoning summary.
    pub reasoning: String,
    /// What the user attached (Oak's blocks, for the thumbnails).
    pub files: Vec<Value>,
    /// How long it reasoned, once it stopped.
    pub thought_ms: Option<u64>,
    reasoning_from: Option<Instant>,
    /// The tools it called, in order: Oak's tool-use blocks (`name`,
    /// `id`, `input`), as a conversation keeps them.
    pub tools: Vec<Value>,
    /// Their outputs by call id: (text, failed).
    pub results: HashMap<String, (String, bool)>,
    /// The pages its citation marks number.
    pub citations: Vec<Citation>,
    /// The sources duck.ai lists under the answer.
    pub sources: Vec<Citation>,
    pub done: bool,
}

impl Msg {
    /// Nothing of the answer has come yet.
    pub fn empty(&self) -> bool {
        self.text.is_empty() && self.reasoning.is_empty() && self.tools.is_empty()
    }

    /// Takes a piece of the answer as it streams in.
    fn take(&mut self, piece: Piece) {
        match piece {
            Piece::Reasoning(s) => {
                self.reasoning_from.get_or_insert_with(Instant::now);
                self.reasoning.push_str(&s);
            }
            Piece::Content(s) => {
                if let Some(t) = self.reasoning_from.take() {
                    self.thought_ms = Some(t.elapsed().as_millis() as u64);
                }
                self.text.push_str(&s);
            }
            Piece::ToolCall { id, name, input } => self.tools.push(json!({"type": "tool_call", "id": id, "name": name, "input": input})),
            Piece::ToolResult { id, text, citations, error } => {
                // The row opens on what was found, numbered as the answer
                // cites it.
                let found: Vec<String> = citations.iter().map(|c| format!("[{}] {}, {}\n    {}", c.id, c.title, c.site, c.url)).collect();
                let text = if found.is_empty() { text } else { found.join("\n") };
                self.results.insert(id, (text, error));
                for c in citations {
                    if !self.citations.iter().any(|x| x.id == c.id) {
                        self.citations.push(c);
                    }
                }
            }
            Piece::Source(s) => {
                if !self.sources.iter().any(|x| x.url == s.url) {
                    self.sources.push(s);
                }
            }
        }
    }
}

/// The one chat. In memory only.
#[derive(Default)]
pub struct Chat {
    pub msgs: Vec<Msg>,
    pub busy: bool,
    pub error: Option<Error>,
    /// Which chat this is: a turn of a burned chat drops what it brings.
    pub round: u64,
    cancel: Option<Arc<AtomicBool>>,
    /// The browser is starting.
    pub starting: bool,
    /// Files sent so far, should the page lose the chat.
    sent_files: Vec<Upload>,
}

static CHAT: Mutex<Option<Chat>> = Mutex::new(None);

/// Reads the chat.
pub fn chat<R>(f: impl FnOnce(&Chat) -> R) -> R {
    let mut c = CHAT.lock().unwrap_or_else(|e| e.into_inner());
    f(c.get_or_insert_default())
}

fn chat_mut<R>(f: impl FnOnce(&mut Chat) -> R) -> R {
    let mut c = CHAT.lock().unwrap_or_else(|e| e.into_inner());
    f(c.get_or_insert_default())
}

/// Starts the browser in the background (the page is opened), so the
/// first message goes quickly.
pub fn warm(ctx: &egui::Context) {
    let ctx = ctx.clone();
    if chat(|c| c.starting || c.busy) {
        return;
    }
    chat_mut(|c| c.starting = true);
    std::thread::spawn(move || {
        let r = with_driver(|d| d.ensure_started());
        chat_mut(|c| {
            c.starting = false;
            if let Err(e) = r
                && c.error.is_none()
            {
                c.error = Some(e);
            }
        });
        ctx.request_repaint();
    });
}

/// Sends a message: `shown` in the chat, `text` to duck.ai (with the text
/// files folded in), `files` for the page and `blocks` for the thumbnails.
#[allow(clippy::too_many_arguments)]
pub fn send(shown: String, text: String, blocks: Vec<Value>, files: Vec<Upload>, model: String, effort: Option<String>, ctx: &egui::Context) {
    let cancel = Arc::new(AtomicBool::new(false));
    let job = chat_mut(|c| {
        if c.busy {
            return None;
        }
        let before = c.msgs.iter().map(|m| (m.user, if m.user { m.sent.clone() } else { strip_citations(&m.text) })).collect();
        let before_files = c.sent_files.clone();
        c.sent_files.extend(files.iter().cloned());
        c.msgs.push(Msg { user: true, text: shown, sent: text.clone(), files: blocks, done: true, ..Default::default() });
        c.msgs.push(Msg::default());
        c.busy = true;
        c.error = None;
        c.cancel = Some(cancel.clone());
        Some(Job { round: c.round, before, before_files, text, files, model, effort, cancel })
    });
    let Some(job) = job else { return };
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("duck-turn".into())
        .spawn(move || {
            let round = job.round;
            let mut on = |p: Piece| {
                chat_mut(|c| {
                    if c.round == round
                        && let Some(m) = c.msgs.last_mut()
                    {
                        m.take(p);
                    }
                });
                ctx.request_repaint();
            };
            let r = with_driver(|d| {
                let r = d.turn(&job, &mut on);
                match r {
                    // The browser hung or went: once more on a new one,
                    // unless some of the answer already came.
                    Err(Error::Down(_)) if chat(|c| c.msgs.last().is_some_and(|m| m.empty())) && !job.cancel.load(Ordering::SeqCst) => {
                        d.shutdown();
                        d.turn(&job, &mut on)
                    }
                    r => r,
                }
            });
            chat_mut(|c| {
                if c.round != round {
                    return;
                }
                c.busy = false;
                c.cancel = None;
                if let Some(m) = c.msgs.last_mut() {
                    m.done = true;
                    if let Some(t) = m.reasoning_from.take() {
                        m.thought_ms = Some(t.elapsed().as_millis() as u64);
                    }
                }
                if let Err(e) = r {
                    // An answer that never started goes; the error says why.
                    if c.msgs.last().is_some_and(|m| !m.user && m.empty()) {
                        c.msgs.pop();
                    }
                    c.error = Some(e);
                }
            });
            ctx.request_repaint();
        })
        .ok();
}

/// Stops the answer coming in (what came stays).
pub fn stop() {
    chat(|c| {
        if let Some(x) = &c.cancel {
            x.store(true, Ordering::SeqCst);
        }
    });
}

/// The fire button: the chat is gone, here and on the page.
pub fn burn(ctx: &egui::Context) {
    let round = chat_mut(|c| {
        if let Some(x) = c.cancel.take() {
            x.store(true, Ordering::SeqCst);
        }
        let round = c.round + 1;
        *c = Chat { round, ..Default::default() };
        round
    });
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        with_driver(|d| {
            // (Unless a newer chat already holds the page.)
            let stale = match d.held {
                Some((g, _)) => g < round,
                None => !d.fresh,
            };
            if stale && d.cdp.is_some() && d.reset().is_err() {
                d.shutdown();
            }
        });
        ctx.request_repaint();
    });
}

/// Oak is closing: the browser goes.
pub fn shutdown() {
    stop();
    if let Ok(mut d) = DRIVER.try_lock()
        && let Some(d) = d.as_mut()
    {
        d.shutdown();
    }
}

/// duck.ai asked for a person: the browser, headless no more, opens
/// duck.ai in a window on the same profile for the user to pass the check.
/// The next message closes it and goes on headless.
pub fn verify_in_window() -> Result<(), String> {
    with_driver(|d| d.shutdown());
    let exe = find_browser().ok_or("No Brave, Edge or Chrome found.")?;
    reap_stale(&exe);
    browser_cmd(&exe)
        .arg(DUCK_URL)
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Couldn't open the browser: {e}"))?;
    chat_mut(|c| c.error = None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_become_chips() {
        let c = |id, site: &str| Citation { id, url: format!("https://{site}.com/x"), title: String::new(), site: site.into() };
        let cites = [c(2, "AccuWeather"), c(4, "Weather")];
        let text = "Sunny, high near 77°F. <citation src=\"2,4\"></citation> Low 56. <citation src=\"9\"></citation>";
        let out = with_citations(text, &cites);
        assert_eq!(out, "Sunny, high near 77°F. [\u{a0}AccuWeather\u{a0}](<https://AccuWeather.com/x> \"cite\")\u{a0} [\u{a0}Weather\u{a0}](<https://Weather.com/x> \"cite\") Low 56. ");
        // Streaming: a mark cut anywhere is held back until it is whole.
        for cut in ["Hi <", "Hi <cit", "Hi <citation src=\"1", "Hi <citation src=\"1\">", "Hi <citation src=\"1\"></cita"] {
            assert_eq!(with_citations(cut, &cites), "Hi ", "{cut}");
        }
        assert_eq!(with_citations("a < b", &cites), "a < b");
        assert_eq!(strip_citations(text), "Sunny, high near 77°F.  Low 56.");
        // The chip parses as a "cite" link.
        let crate::markdown::Block::Para(spans) = &crate::markdown::parse(&out)[0] else { panic!() };
        let chip = spans.iter().find(|s| s.link.is_some()).unwrap();
        assert!(chip.style.cite && chip.text.contains("AccuWeather"), "{spans:?}");
        assert_eq!(chip.link.as_deref(), Some("https://AccuWeather.com/x"));
    }

    #[test]
    fn parses_tool_calls() {
        let mut p = Parser::default();
        let got = p
            .feed(concat!(
                "data: {\"role\":\"tool-invocation\",\"state\":\"call\",\"toolName\":\"WebSearch\",\"toolCallId\":\"c1\",\"toolArguments\":\"{\\\"query\\\":\\\"phoenix weather\\\"}\"}\n",
                "data: {\"role\":\"tool-invocation\",\"state\":\"result\",\"toolCallId\":\"c1\",\"result\":\"done\",\"data\":{\"citations\":[{\"id\":1,\"url\":\"https://www.accuweather.com/a\",\"title\":\"T\",\"site_name\":\"AccuWeather\"}]}}\n",
                "data: {\"role\":\"source\",\"source\":{\"url\":\"https://www.easeweather.com/b\",\"title\":\"B\",\"site\":\"www.easeweather.com\"}}\n",
            ).as_bytes())
            .unwrap();
        assert_eq!(got[0], Piece::ToolCall { id: "c1".into(), name: "WebSearch".into(), input: json!({"query": "phoenix weather"}) });
        let Piece::ToolResult { citations, .. } = &got[1] else { panic!() };
        assert_eq!((citations[0].id, citations[0].site.as_str()), (1, "AccuWeather"));
        let Piece::Source(s) = &got[2] else { panic!() };
        assert_eq!(s.site, "easeweather.com");
    }

    #[test]
    fn parses_reasoning_and_content() {
        let mut p = Parser::default();
        let stream = concat!(
            "data: [CHAT_TITLE:Hi]\n\n",
            "data: {\"action\":\"success\",\"role\":\"reasoning\",\"state\":\"text-delta\",\"partIndex\":0,\"text\":\"Think\"}\n\n",
            "data: {\"action\":\"success\",\"role\":\"reasoning\",\"state\":\"text-delta\",\"partIndex\":1,\"text\":\"More\"}\n\n",
            "data: {\"action\":\"success\",\"role\":\"assistant\",\"message\":\"Hé\"}\n\n",
            "data: [DONE]\n\n",
        );
        // Split mid-character: the parser waits for whole lines.
        let bytes = stream.as_bytes();
        let cut = stream.find('é').unwrap() + 1;
        let mut got = p.feed(&bytes[..cut]).unwrap();
        got.extend(p.feed(&bytes[cut..]).unwrap());
        assert_eq!(got, vec![Piece::Reasoning("Think".into()), Piece::Reasoning("\n\nMore".into()), Piece::Content("Hé".into())]);
        assert!(p.done);
    }
}
