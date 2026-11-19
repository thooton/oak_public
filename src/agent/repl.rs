//! REPL, Code Mode's one tool (`crate::code_mode`): runs the model's script
//! in the conversation's Node.js host and answers the calls the script
//! makes ($ commands, file reads and edits, Oak's tools) as the model's
//! own calls would be run (`run_call`, with its checks: a subagent's
//! limits, the command guard). They are listed with the REPL
//! call's result in the transcript (`CALLS_KEY`).

use super::tool_calls::Ran;
use super::*;
use crate::code_mode::{self, Recv};
use std::sync::atomic::AtomicUsize;

/// The key marking a call a script made (not the model directly), which
/// Code Mode runs (`Worker::code_mode_refuses`). Never sent.
pub(crate) const FROM_SCRIPT: &str = "_oak_repl";

/// On a REPL result: the calls its script made, each {name, input,
/// error}, for the transcript. Never sent.
pub(crate) const CALLS_KEY: &str = "_oak_calls";

/// A script's calls whose time doesn't count toward its limit: they wait
/// for the user.
const WAITS_FOR_USER: &[&str] = &["ASK", "BROWSER_ATTACH", "WINDOW_ATTACH"];

/// The most of a command's output a script gets (more is cut, and the
/// whole is in its file).
const SHELL_MAX: usize = 16 << 20;

/// The biggest file `file(path)` reads.
const FILE_MAX: u64 = 64 << 20;

/// A $ command's timeout unless the script gives one, and the longest
/// (past it the command moves to the background).
const SHELL_TIMEOUT_MS: u64 = 120_000;
const SHELL_TIMEOUT_MAX_MS: u64 = 600_000;

/// How long a script that was told to stop may take to: one that won't (a
/// loop that never awaits) has its host restarted.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// What a script's call came back with: its value for the script (or the
/// error it throws), and whether the user interrupted it.
struct Answer {
    ok: bool,
    value: Value,
    interrupted: bool,
}

impl Answer {
    fn ok(value: Value) -> Self {
        Answer { ok: true, value, interrupted: false }
    }
    fn err(e: impl Into<String>) -> Self {
        Answer { ok: false, value: json!(e.into()), interrupted: false }
    }
}

/// What a running script's calls leave for its result.
#[derive(Default)]
struct Script {
    /// Its calls, in the order they were made (`CALLS_KEY`).
    calls: Mutex<Vec<Value>>,
    /// Images its tools returned: they go with the result.
    images: Mutex<Vec<Value>>,
    /// Its running time so far (`code_mode::SCRIPT_LIMIT`): a command it
    /// starts gets no longer than what is left before it moves to the
    /// background.
    counted: Mutex<Duration>,
}

impl Script {
    fn record(&self, name: &str, input: Value, error: bool) {
        self.calls.lock().unwrap().push(json!({"name": name, "input": input, "error": error}));
    }
}

impl Worker {
    /// A REPL call (from the model; the stop reason as for `run_call`).
    pub(super) fn repl_call(&self, call: &Value, stop: &str) -> (Value, bool) {
        let fail = |e: String| (result_block(call, json!(e), true), false);
        let (code_mode, conv) = {
            let c = self.conv.lock().unwrap();
            (c.code_mode && !c.chat, c.id.clone())
        };
        if !code_mode {
            return fail("REPL is a Code Mode tool, and this conversation isn't in Code Mode.".into());
        }
        if call.get(FROM_SCRIPT).is_some() {
            return fail("A script can't call REPL: it is the REPL already.".into());
        }
        let _ = stop;
        let Some(code) = call["input"]["code"].as_str() else {
            return fail("Invalid input: `code` is required, the JavaScript to run.".into());
        };
        self.set(|l| l.status = "Starting the REPL".into());
        let host = match code_mode::host(&conv) {
            Ok(h) => h,
            Err(e) => return fail(e),
        };
        self.set(|l| l.status = "Running REPL".into());
        let note = if std::mem::take(&mut *host.fresh.lock().unwrap()) { code_mode::restore_note(&host.ready) } else { None };
        let script = Script::default();
        let call_id = call["id"].as_str().unwrap_or("");
        let (outcome, interrupted) = self.run_script(&conv, &host, call_id, code, note, &script);
        let mut text = match outcome {
            Ok(done) => {
                let mut text = done["text"].as_str().unwrap_or("").to_string();
                if let Some(e) = done["saveError"].as_str().filter(|e| !e.is_empty()) {
                    text += &format!("\n(The REPL's variables couldn't be saved: {e}. They are kept while Oak runs.)");
                }
                Ok((text, done["error"].as_bool().unwrap_or(false)))
            }
            Err(e) => Err(e),
        };
        // An output too long to send goes to the scratchpad, as EXECUTE's.
        if let Ok((t, _)) = &mut text
            && tools::too_large(t)
        {
            *t = tools::persist(t, &scratchpad(&mut self.conv.lock().unwrap()));
        }
        let (text, is_error) = match text {
            Ok(t) => t,
            Err(e) => (e, true),
        };
        let images = std::mem::take(&mut *script.images.lock().unwrap());
        let content = if images.is_empty() { json!(text) } else { Value::Array([vec![oak::text(&text)], images].concat()) };
        let mut r = result_block(call, content, is_error);
        let calls = std::mem::take(&mut *script.calls.lock().unwrap());
        if !calls.is_empty() {
            r[CALLS_KEY] = json!(calls);
        }
        (r, interrupted)
    }

    /// Runs `code` in `host` and answers its calls until it is done: its
    /// `done` message, or why there is none. The bool is true when the user
    /// interrupted it.
    fn run_script(&self, conv: &str, host: &Arc<code_mode::Host>, call_id: &str, code: &str, note: Option<String>, script: &Script) -> (Result<Value, String>, bool) {
        let (agent, spawner, local, mut kits) = {
            let c = self.conv.lock().unwrap();
            (c.parent.is_some(), c.spawner, crate::local::serves(&c.model), c.prefix.kits.clone())
        };
        if local && !self.conv.lock().unwrap().goal.is_empty() {
            kits.push("GOAL".into());
        }
        let tools = tools::definitions_with_kits(agent, spawner, local, &kits);
        if let Err(e) = host.send(&json!({"t": "eval", "code": code, "tools": tools, "note": note})) {
            code_mode::drop_host(conv, host);
            return (Err(e), false);
        }
        let stopped = AtomicBool::new(false);
        let waiting = AtomicUsize::new(0);
        let outcome = std::thread::scope(|s| {
            // Its running time, not counting waits for the user.
            let mut counted = Duration::ZERO;
            let mut last = Instant::now();
            let mut told: Option<Instant> = None;
            loop {
                match host.recv(Duration::from_millis(30)) {
                    Recv::Message(m) if m["t"] == "done" => return Ok(m),
                    Recv::Message(m) if m["t"] == "call" => {
                        let (stopped, waiting) = (&stopped, &waiting);
                        s.spawn(move || {
                            let name = m["name"].as_str().unwrap_or("");
                            let tool = if name == "tool" { tools::canonical_name(m["input"]["name"].as_str().unwrap_or(""), &m["input"]["input"]).to_string() } else { String::new() };
                            let user = WAITS_FOR_USER.contains(&tool.as_str());
                            if user {
                                waiting.fetch_add(1, Ordering::SeqCst);
                            }
                            let a = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.script_call(call_id, name, &m["input"], script)))
                                .unwrap_or_else(|_| Answer::err("The call failed unexpectedly (Oak caught a crash)."));
                            if user {
                                waiting.fetch_sub(1, Ordering::SeqCst);
                            }
                            if a.interrupted {
                                stopped.store(true, Ordering::SeqCst);
                            }
                            let _ = host.send(&json!({"t": "reply", "id": m["id"], "ok": a.ok, "value": a.value}));
                        });
                    }
                    Recv::Message(_) | Recv::Nothing => {}
                    Recv::Gone => {
                        code_mode::drop_host(conv, host);
                        return Err(format!("{}. Its variables are as they were last saved, before this script; the next REPL call starts it again with them.", host.died()));
                    }
                }
                let now = Instant::now();
                if waiting.load(Ordering::SeqCst) == 0 {
                    counted += now - last;
                    *script.counted.lock().unwrap() = counted;
                }
                last = now;
                match told {
                    None => {
                        let why = if self.ultra() {
                            Some("Interrupted: the user stopped everything (ultrapause).".to_string())
                        } else if stopped.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) || self.deleted.load(Ordering::SeqCst) {
                            Some("Interrupted: the user sent a message.".to_string())
                        } else if counted >= code_mode::SCRIPT_LIMIT {
                            Some(format!(
                                "Timed out after {}, not counting time waiting for the user. What the script set before that is kept; split long work into several calls, or run a long command in the background (TASK).",
                                fmt_duration(code_mode::SCRIPT_LIMIT)
                            ))
                        } else {
                            None
                        };
                        if let Some(why) = why {
                            told = Some(now);
                            let _ = host.send(&json!({"t": "interrupt", "why": why}));
                        }
                    }
                    // It doesn't stop: a loop that never awaits.
                    Some(at) if at.elapsed() > STOP_GRACE && waiting.load(Ordering::SeqCst) == 0 => {
                        code_mode::drop_host(conv, host);
                        return Err("Interrupted, and the script didn't stop (a loop that never awaits?), so the REPL was restarted: its variables are as they were last saved, before this script.".to_string());
                    }
                    Some(_) => {}
                }
            }
        });
        // (An interrupt no call took is taken here.)
        let interrupted = self.interrupted() | stopped.load(Ordering::SeqCst) | self.ultra();
        (outcome, interrupted)
    }

    /// One call a script made: `$` (a command), `_file`, `_edit` or `tool`
    /// (one of Oak's tools by name).
    fn script_call(&self, call_id: &str, name: &str, input: &Value, script: &Script) -> Answer {
        match name {
            "$" => self.script_shell(call_id, input, script),
            "_file" => self.script_file(input, script),
            "_edit" => self.script_edit(call_id, input, script),
            "tool" => self.script_tool(call_id, input["name"].as_str().unwrap_or(""), &input["input"], script),
            _ => Answer::err(format!("Oak doesn't know the call {name}.")),
        }
    }

    /// One of Oak's tools, run as the model's call would be.
    fn script_tool(&self, call_id: &str, name: &str, input: &Value, script: &Script) -> Answer {
        let shown = tools::canonical_name(name, input);
        self.set(|l| l.status = format!("Running {} in REPL", tools::shown_name(shown)));
        let (r, interrupted) = self.run_call(&json!({"id": call_id, "name": name, "input": input, FROM_SCRIPT: true}), "");
        let is_error = r["is_error"].as_bool().unwrap_or(false);
        script.record(name, input.clone(), is_error);
        let text = match &r["content"] {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => {
                let mut texts = vec![];
                for b in blocks {
                    if b["type"] == "text" {
                        texts.push(b["text"].as_str().unwrap_or("").to_string());
                    } else if b["type"] == "image" {
                        script.images.lock().unwrap().push(b.clone());
                        texts.push("[an image: you see it with this script's result]".to_string());
                    }
                }
                texts.join("\n")
            }
            v => v.to_string(),
        };
        Answer { ok: true, value: json!({"text": text, "error": is_error}), interrupted }
    }

    /// `$`: a command in bash, as EXECUTE runs one (a subagent's limits,
    /// the command guard, the long-sleep check), its output for the script.
    fn script_shell(&self, call_id: &str, input: &Value, script: &Script) -> Answer {
        let command = input["command"].as_str().unwrap_or("").to_string();
        let mut shown = json!({"command": command});
        let a = self.shell(call_id, input, &mut shown, script);
        script.record("$", shown, !a.ok || a.value["error"].is_string() || a.value["exitCode"].as_i64().is_some_and(|c| c != 0));
        a
    }

    fn shell(&self, call_id: &str, input: &Value, shown: &mut Value, script: &Script) -> Answer {
        let (folder, agent, spawner) = {
            let c = self.conv.lock().unwrap();
            (c.folder.clone(), c.parent.is_some(), c.spawner)
        };
        if !tools::allowed("EXECUTE", agent, spawner) {
            return Answer::err("Commands aren't available to this agent.");
        }
        let command = input["command"].as_str().unwrap_or("");
        if command.trim().is_empty() {
            return Answer::err("The command is empty.");
        }
        let cwd = match input["cwd"].as_str() {
            Some(d) => {
                let d = tools::resolve(&folder, d);
                if !d.is_dir() {
                    return Answer::err(format!("No folder {}.", d.display()));
                }
                shown["cwd"] = json!(d.display().to_string());
                d
            }
            None => folder.clone(),
        };
        let mut run = String::new();
        if let Some(env) = input["env"].as_object() {
            for (k, v) in env {
                if !k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Answer::err(format!("{k:?} isn't an environment variable's name."));
                }
                let v = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
                run += &format!("export {k}='{}'\n", v.replace('\'', "'\\''"));
            }
        }
        run += command;
        let left = code_mode::SCRIPT_LIMIT.saturating_sub(*script.counted.lock().unwrap()).max(Duration::from_secs(1));
        let timeout = Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(SHELL_TIMEOUT_MS).clamp(1, SHELL_TIMEOUT_MAX_MS)).min(left);
        let started = crate::sleeps::check(command).and_then(|_| crate::guard::check(command, &folder)).and_then(|_| crate::guard::check(command, &cwd)).and_then(|_| tasks::spawn(&run, &cwd));
        let p = match started {
            Ok(p) => p,
            Err(e) => return Answer::err(e),
        };
        self.set(|l| l.status = format!("Running {} in REPL", command.head(40).replace('\n', " ")));
        let row = if input["quiet"].as_bool().unwrap_or(false) { "" } else { call_id };
        match self.run_command(row, &p, timeout, &command.head(60)) {
            Ran::Exited { out, code } => {
                let mut v = json!({"stdout": out, "exitCode": code});
                if out.len() > MAX_OUTPUT {
                    // The whole of a long output is kept in a file too.
                    let dir = scratchpad(&mut self.conv.lock().unwrap()).join("tool_results");
                    let path = dir.join(format!("repl-{}.txt", api::now_ms()));
                    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &out)).is_ok() {
                        v["file"] = json!(path.display().to_string());
                        v["bytes"] = json!(out.len());
                    }
                }
                if out.len() > SHELL_MAX {
                    let mut cut = SHELL_MAX;
                    while !out.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    v["stdout"] = json!(&out[..cut]);
                    v["truncated"] = json!(true);
                }
                Answer::ok(v)
            }
            Ran::Killed { out, took } => Answer {
                ok: false,
                value: json!(format!("The user stopped everything (ultrapause), so the command was killed after {}. Output so far:\n{}", tasks::fmt_took(took), out.trim_end())),
                interrupted: true,
            },
            Ran::Moved { out, id, took, interrupted, .. } => {
                let why = if interrupted { "the user sent a message".to_string() } else { format!("it was still running after {} (its timeout)", tasks::fmt_took(took)) };
                Answer {
                    ok: true,
                    value: json!({
                        "stdout": out,
                        "exitCode": null,
                        "running": {"taskId": id.to_string(), "outputPath": p.log_path().display().to_string()},
                        "error": format!("{why}, so it moved to the background as task {id}: TASK({{action: \"output\", id: {id}}}) reads what it prints next, and you are told when it ends"),
                    }),
                    interrupted,
                }
            }
        }
    }

    /// `file(path)`: a file's text, or whether it exists.
    fn script_file(&self, input: &Value, script: &Script) -> Answer {
        let folder = self.conv.lock().unwrap().folder.clone();
        let typed = input["path"].as_str().unwrap_or("");
        let path = tools::resolve(&folder, typed);
        if input["exists"].as_bool().unwrap_or(false) {
            return Answer::ok(json!({"exists": path.exists()}));
        }
        let read = match std::fs::metadata(&path) {
            Err(e) => Err(format!("Can't read {}: {e}", path.display())),
            Ok(m) if m.is_dir() => Err(format!("{} is a folder: list it with $`ls`.", path.display())),
            Ok(m) if m.len() > FILE_MAX => Err(format!("{} is {} MB, too big to read whole: use slice() on a part, or $ with head or grep.", path.display(), m.len() >> 20)),
            Ok(_) => std::fs::read(&path).map_err(|e| format!("Can't read {}: {e}", path.display())),
        };
        script.record("READ", json!({"path": typed}), read.is_err());
        match read {
            Ok(bytes) => {
                self.track("READ", &json!({"path": typed}));
                Answer::ok(json!({"text": String::from_utf8_lossy(&bytes)}))
            }
            Err(e) => Answer::ok(json!({"error": e})),
        }
    }

    /// `edit(...)`: exact-string edits of one file, all of them checked
    /// first (a batch applies whole or not at all), then each made as an
    /// EDIT call would make it.
    fn script_edit(&self, call_id: &str, input: &Value, script: &Script) -> Answer {
        let folder = self.conv.lock().unwrap().folder.clone();
        let typed = input["path"].as_str().unwrap_or("");
        let all = input["replace_all"].as_bool().unwrap_or(false);
        let edits: Vec<(String, String)> = input["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| (e["old_string"].as_str().unwrap_or("").to_string(), e["new_string"].as_str().unwrap_or("").to_string()))
            .collect();
        let fail = |e: String| Answer::ok(json!({"ok": false, "error": e}));
        if edits.is_empty() {
            return fail("No edits given.".into());
        }
        let path = tools::resolve(&folder, typed);
        let mut text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return fail(format!("Can't read {}: {e}", path.display())),
        };
        let mut replacements = 0;
        for (i, (old, new)) in edits.iter().enumerate() {
            let which = if edits.len() > 1 { format!("Edit {} of {}: ", i + 1, edits.len()) } else { String::new() };
            if old.is_empty() {
                return fail(format!("{which}the old string is empty."));
            }
            let n = text.matches(old.as_str()).count();
            if n == 0 {
                return fail(format!("{which}the old string isn't in {} (nothing was changed): {:?}", typed, old.head(120)));
            }
            if n > 1 && !all {
                return fail(format!("{which}the old string is in {typed} {n} times (nothing was changed): give more of the text around it, or {{all: true}} to replace every one."));
            }
            text = text.replace(old.as_str(), new);
            replacements += n;
        }
        // Checked: now made, one EDIT each.
        for (old, new) in &edits {
            let edit = json!({"path": typed, "old_string": old, "new_string": new, "replace_all": all});
            let (r, _) = self.run_call(&json!({"id": call_id, "name": "EDIT", "input": edit, FROM_SCRIPT: true}), "");
            let error = r["is_error"].as_bool().unwrap_or(false);
            script.record("EDIT", json!({"path": typed}), error);
            if error {
                return fail(r["content"].as_str().unwrap_or("The edit failed.").to_string());
            }
        }
        Answer::ok(json!({"ok": true, "replacements": replacements}))
    }
}
