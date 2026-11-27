//! Code Mode (experimental): a conversation (`Conversation::code_mode`)
//! whose one tool is REPL, JavaScript run in a persistent context, from
//! which the model calls Oak's other tools as functions (the calls go
//! through `agent`'s usual checks: `agent/repl.rs`). The JavaScript runs in
//! the user's own Node.js (18 or later, found on PATH; without it Code Mode
//! can't be chosen): one host process per conversation (`code_mode/host.js`,
//! Node's REPL on a sealed context), started at the first REPL call. After
//! each script the host saves the context's variables to a snapshot in the
//! data folder (`code-mode/<conversation id>.json`), and a new host puts
//! them back, so they last across restarts of Oak and the host itself
//! needn't stay: one no script has used for 15 minutes is stopped
//! (`stop_idle`). Deleting the conversation deletes its snapshot.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Code Mode's part of the system prompt (its {{tools}}: the tools a
/// script can call).
const PROMPT: &str = include_str!("code_mode/prompt.md");

/// The host the REPL runs in.
const HOST_JS: &str = include_str!("code_mode/host.js");

/// The oldest Node.js Code Mode runs on.
pub const NODE_MIN: u32 = 18;

/// How long a script may run, not counting time waiting for the user.
pub const SCRIPT_LIMIT: Duration = Duration::from_secs(600);

/// What Code Mode says when there is no Node.js to run it.
pub fn missing(why: &str) -> String {
    format!("Code Mode needs Node.js {NODE_MIN} or later, installed on this computer: {why}. Install it from nodejs.org, then try again.")
}

/// REPL, the one tool of a Code Mode conversation.
pub fn tool_definition() -> Value {
    json!({
        "name": "REPL",
        "description": "Runs JavaScript in this conversation's persistent REPL context (see Code Mode in the system prompt): Oak's tools are async functions there, $ runs shell commands, and variables last from one call to the next. Returns the script's logs and the value of its last expression.",
        "input_schema": {
            "type": "object",
            "properties": {"code": {"type": "string", "description": "The JavaScript to run (top-level await works)"}},
            "required": ["code"],
        },
        "eager_input_streaming": true,
    })
}

/// Code Mode's prompt section, listing `tools` (definitions as the model
/// would get them) by name and first sentence.
pub fn prompt(tools: &[Value]) -> String {
    let list: Vec<String> = tools
        .iter()
        .filter_map(|t| {
            let name = t["name"].as_str()?;
            let description = t["description"].as_str().unwrap_or("");
            let first = description.split_inclusive(". ").next().unwrap_or(description).trim();
            Some(format!("- {name}: {first}"))
        })
        .collect();
    PROMPT.replace("{{tools}}", &list.join("\n"))
}

// ---------------------------------------------------------------- Node.js

/// How long a finding of Node.js (or of its absence) holds before it is
/// looked for again: installing it takes effect without restarting Oak.
const RECHECK: Duration = Duration::from_secs(15);

type Found = Option<(Instant, Result<PathBuf, String>)>;
static NODE: LazyLock<Mutex<Found>> = LazyLock::new(Mutex::default);

/// The Node.js Code Mode runs on, or why there is none (`missing` words it
/// for the user).
pub fn node() -> Result<PathBuf, String> {
    let mut n = NODE.lock().unwrap();
    if let Some((_, found)) = n.as_ref().filter(|(at, _)| at.elapsed() < RECHECK) {
        return found.clone();
    }
    let found = find_node();
    *n = Some((Instant::now(), found.clone()));
    found
}

/// Whether Code Mode can be used here, for drawing: as last found, never
/// waiting (a finding gone stale is renewed in the background, then
/// `ctx` repaints).
pub fn available(ctx: &eframe::egui::Context) -> bool {
    static LOOKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let known = NODE.lock().unwrap().as_ref().map(|(at, found)| (at.elapsed() < RECHECK, found.is_ok()));
    if known.is_none_or(|(fresh, _)| !fresh) && !LOOKING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = node();
            LOOKING.store(false, std::sync::atomic::Ordering::SeqCst);
            ctx.request_repaint();
        });
    }
    known.is_some_and(|(_, ok)| ok)
}

/// `OAK_NODE` if set (a path; empty means none, as for tests), else
/// `node` on PATH or in the usual install folders (an app started from
/// the desktop may have a shorter PATH than a terminal's).
fn find_node() -> Result<PathBuf, String> {
    let candidates: Vec<PathBuf> = match std::env::var_os("OAK_NODE") {
        Some(p) if p.is_empty() => vec![],
        Some(p) => vec![PathBuf::from(p)],
        None => {
            let exe = if cfg!(windows) { "node.exe" } else { "node" };
            let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
            if cfg!(windows) {
                for var in ["ProgramFiles", "ProgramFiles(x86)"] {
                    if let Some(p) = std::env::var_os(var) {
                        dirs.push(PathBuf::from(p).join("nodejs"));
                    }
                }
            } else {
                dirs.extend(["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"].map(PathBuf::from));
                if let Some(home) = dirs::home_dir() {
                    dirs.push(home.join(".volta/bin"));
                }
            }
            dirs.into_iter().map(|d| d.join(exe)).filter(|p| p.is_file()).collect()
        }
    };
    if candidates.is_empty() {
        return Err("Node.js wasn't found".into());
    }
    let mut last = String::new();
    for p in candidates {
        match version(&p) {
            Ok(major) if major >= NODE_MIN => return Ok(p),
            Ok(major) => last = format!("the Node.js found ({}) is version {major}", p.display()),
            Err(e) => last = format!("{} didn't run ({e})", p.display()),
        }
    }
    Err(last)
}

/// Node.js's major version.
fn version(node: &Path) -> Result<u32, String> {
    let mut cmd = Command::new(node);
    cmd.arg("--version").stdin(Stdio::null()).stderr(Stdio::null());
    no_window(&mut cmd);
    let out = cmd.output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().trim_start_matches('v').split('.').next().and_then(|m| m.parse().ok()).ok_or_else(|| format!("it printed {:?} for its version", text.trim()))
}

/// No console window flashes up for it on Windows.
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

// ---------------------------------------------------------------- snapshots

/// Where conversation `conv`'s REPL context is saved.
pub fn snapshot_path(conv: &str) -> PathBuf {
    let name: String = conv.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    snapshots_dir().join(format!("{name}.json"))
}

fn snapshots_dir() -> PathBuf {
    #[cfg(test)]
    let d = crate::temp_dir().join(format!("oak-test-code-mode-{}", std::process::id()));
    #[cfg(not(test))]
    let d = crate::store::dir().join("code-mode");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Conversation `conv` is deleted: its host stops and its snapshot goes.
pub fn forget(conv: &str) {
    stop(conv);
    let path = snapshot_path(conv);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("json.tmp"));
}

/// A copy of `from`'s snapshot for `to` (a side chat forked from it
/// starts with the same variables).
pub fn copy_snapshot(from: &str, to: &str) {
    let _ = std::fs::copy(snapshot_path(from), snapshot_path(to));
}

// ---------------------------------------------------------------- hosts

/// A conversation's running host.
pub struct Host {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    rx: Mutex<Receiver<Value>>,
    /// What it said as it started: how many names it restored from the
    /// snapshot and which it couldn't (`lost`), and when that was saved.
    pub ready: Value,
    /// Its first script hasn't run yet (the restore is told with it).
    pub fresh: Mutex<bool>,
    /// The last lines Node.js wrote to stderr (why it failed, if it did).
    stderr: Arc<Mutex<Vec<String>>>,
    /// When a script last ran in it (`stop_idle`).
    used: Mutex<Instant>,
}

/// What `Host::recv` got.
pub enum Recv {
    Message(Value),
    Nothing,
    /// The host has exited.
    Gone,
}

impl Host {
    pub fn send(&self, m: &Value) -> Result<(), String> {
        *self.used.lock().unwrap() = Instant::now();
        let mut line = m.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().unwrap();
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).map_err(|e| format!("The REPL's Node.js process can't be reached: {e}"))
    }

    pub fn recv(&self, wait: Duration) -> Recv {
        let got = self.rx.lock().unwrap().recv_timeout(wait);
        if got.is_ok() {
            *self.used.lock().unwrap() = Instant::now();
        }
        match got {
            Ok(m) => Recv::Message(m),
            Err(RecvTimeoutError::Timeout) => Recv::Nothing,
            Err(RecvTimeoutError::Disconnected) => Recv::Gone,
        }
    }

    /// Why it ended, from what it wrote to stderr.
    pub fn died(&self) -> String {
        died(&mut self.child.lock().unwrap(), &self.stderr)
    }

    fn kill(&self) {
        let mut c = self.child.lock().unwrap();
        let _ = c.kill();
        let _ = c.wait();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.kill();
    }
}

static HOSTS: LazyLock<Mutex<HashMap<String, Arc<Host>>>> = LazyLock::new(Mutex::default);

/// Conversation `conv`'s host: the running one, or a new one (restoring
/// the snapshot) if there is none or it has exited.
pub fn host(conv: &str) -> Result<Arc<Host>, String> {
    if let Some(h) = HOSTS.lock().unwrap().get(conv).cloned()
        && h.child.lock().unwrap().try_wait().ok().flatten().is_none()
    {
        return Ok(h);
    }
    #[cfg(test)]
    if NO_NODE.lock().unwrap().iter().any(|c| c == conv) {
        return Err(missing("none was found"));
    }
    let node = node().map_err(|e| missing(&e))?;
    static REAPER: std::sync::Once = std::sync::Once::new();
    REAPER.call_once(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(60));
                stop_idle(IDLE);
            }
        });
    });
    let h = Arc::new(start(&node, &snapshot_path(conv))?);
    HOSTS.lock().unwrap().insert(conv.to_string(), h.clone());
    Ok(h)
}

/// Conversations a test runs as if there were no Node.js.
#[cfg(test)]
static NO_NODE: Mutex<Vec<String>> = Mutex::new(vec![]);

/// Runs `f` as if this computer had no Node.js, for conversation `conv`.
#[cfg(test)]
pub fn without_node<T>(conv: &str, f: impl FnOnce() -> T) -> T {
    NO_NODE.lock().unwrap().push(conv.to_string());
    let out = f();
    NO_NODE.lock().unwrap().retain(|c| c != conv);
    out
}

/// Stops conversation `conv`'s host (its context stays in the snapshot).
pub fn stop(conv: &str) {
    let h = HOSTS.lock().unwrap().remove(conv);
    if let Some(h) = h {
        h.kill();
    }
}

/// How long a host with no script running stays up: then it is stopped,
/// and the conversation's next REPL call starts a new one from the
/// snapshot (a Node.js process for each conversation ever used would
/// otherwise stay for as long as Oak runs).
const IDLE: Duration = Duration::from_secs(15 * 60);

/// Stops the hosts no script has used for `idle` and none is using now
/// (only the list holds them).
pub fn stop_idle(idle: Duration) {
    let gone: Vec<Arc<Host>> = {
        let mut hosts = HOSTS.lock().unwrap();
        let idle: Vec<String> = hosts.iter().filter(|(_, h)| Arc::strong_count(h) == 1 && h.used.lock().unwrap().elapsed() >= idle).map(|(k, _)| k.clone()).collect();
        idle.iter().filter_map(|k| hosts.remove(k)).collect()
    };
    for h in gone {
        h.kill();
    }
}

/// Stops conversation `conv`'s host after `h` failed it (unless a new one
/// replaced it meanwhile).
pub fn drop_host(conv: &str, h: &Arc<Host>) {
    let mut hosts = HOSTS.lock().unwrap();
    if hosts.get(conv).is_some_and(|x| Arc::ptr_eq(x, h)) {
        hosts.remove(conv);
    }
    drop(hosts);
    h.kill();
}

/// How long a host may take to start and restore its snapshot.
const START_WAIT: Duration = Duration::from_secs(60);

fn start(node: &Path, snapshot: &Path) -> Result<Host, String> {
    // Named by its content, not the process: one file for every Oak that
    // runs this host, instead of one left behind by each run.
    let script = crate::temp_dir().join(format!("oak-code-mode-host-{:016x}.js", crate::hash_of(HOST_JS)));
    if std::fs::read_to_string(&script).ok().as_deref() != Some(HOST_JS) {
        // Written beside it and moved in, so another Oak starting the host
        // at the same time never reads it half-written.
        let part = script.with_extension(format!("{}.part", std::process::id()));
        let wrote = std::fs::write(&part, HOST_JS).and_then(|()| std::fs::rename(&part, &script));
        if let Err(e) = wrote {
            let _ = std::fs::remove_file(&part);
            if std::fs::read_to_string(&script).ok().as_deref() != Some(HOST_JS) {
                return Err(format!("Couldn't write the REPL's host to {}: {e}", script.display()));
            }
        }
    }
    let mut cmd = Command::new(node);
    crate::oom::isolate(&mut cmd);
    cmd.arg(&script).arg(snapshot).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start Node.js ({}): {e}", node.display()))?;
    let (stdin, stdout, stderr) = (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let e2 = errors.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let mut e = e2.lock().unwrap();
            e.push(line);
            if e.len() > 40 {
                e.remove(0);
            }
        }
    });
    let t = Instant::now();
    let ready = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) if m["t"] == "ready" => break m,
            Ok(_) | Err(RecvTimeoutError::Timeout) if t.elapsed() < START_WAIT => {}
            Ok(_) | Err(RecvTimeoutError::Timeout) => {
                let why = died(&mut child, &errors);
                let _ = child.kill();
                let _ = child.wait();
                return Err(why);
            }
            Err(RecvTimeoutError::Disconnected) => {
                // (Its stderr may still be coming in.)
                std::thread::sleep(Duration::from_millis(100));
                return Err(died(&mut child, &errors));
            }
        }
    };
    Ok(Host { child: Mutex::new(child), stdin: Mutex::new(stdin), rx: Mutex::new(rx), ready, fresh: Mutex::new(true), stderr: errors, used: Mutex::new(Instant::now()) })
}

/// Why a host ended, from its exit and what it wrote to stderr.
fn died(child: &mut Child, stderr: &Mutex<Vec<String>>) -> String {
    let code = child.try_wait().ok().flatten().map(|s| s.to_string()).unwrap_or_else(|| "it stopped answering".into());
    let err = stderr.lock().unwrap().join("\n");
    let err = if err.trim().is_empty() { String::new() } else { format!(":\n{}", crate::tools::ellipsize(err.trim(), 2000)) };
    format!("The REPL's Node.js process ended ({code}){err}")
}

/// What a new host's first result says of the restore, if anything: the
/// names it couldn't put back.
pub fn restore_note(ready: &Value) -> Option<String> {
    let restored = ready["restored"].as_u64().unwrap_or(0);
    let lost: Vec<&str> = ready["lost"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    if lost.is_empty() {
        return None;
    }
    let saved = ready["saved"].as_i64().unwrap_or(0);
    let when = if saved > 0 { format!(" (saved {} ago)", crate::tasks::fmt_took(Duration::from_millis((crate::api::now_ms() - saved).max(0) as u64))) } else { String::new() };
    Some(format!("The REPL restarted and restored {restored} names from its last save{when}; these couldn't be kept and are gone: {}.", lost.join(", ")))
}
