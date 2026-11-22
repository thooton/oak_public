//! One Quick turn: the socket, the user's message, and Quick's frames read
//! until the message ends. Quick asks for tools by name; Oak runs each one
//! here, in the conversation's working folder, and answers on the socket, so
//! Quick works on the real project rather than on a copy in its sandbox.

use super::*;
use crate::quick::socket::Socket;

/// How long one turn may run before Oak gives up on it.
const TURN_LIMIT: Duration = Duration::from_secs(3_600);

/// Streams one Oak request through Quick's agent, sending its events to
/// `tx`. Quick keeps the thread's history itself, so only the newest user
/// turn goes; the tools it asks for are run in `body.folder`.
pub fn stream(body: Request, tx: Sender<Sse>) {
    let id = body.model.clone();
    let Some(m) = model(&id) else {
        return drop(tx.send(Sse::Err(format!("Quick doesn't serve {id}."))));
    };
    let folder = body.folder.clone().filter(|f| !f.is_empty()).map(std::path::PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    // The thread is the conversation's, so Quick keeps its history there.
    let thread = body.conv.clone().filter(|c| !c.is_empty()).unwrap_or_else(hex_uuid);
    let token = match login::access_token() {
        Ok(t) => t,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    let host = match login::host() {
        Ok(h) => h,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    let path = format!(
        "/streaming/quickwork-chat-bidirectional-stream?surfaceType={SURFACE}&token={}&workingSessionId={thread}&channel=prod",
        urlencode(&token)
    );
    let mut sock = match Socket::connect(&host, &path) {
        Ok(s) => s,
        Err(e) => return drop(tx.send(Sse::Err(e))),
    };
    let mut base = json!({
        "surfaceType": SURFACE,
        "appVersion": APP_VERSION,
        "acp_first_text": 1,
        "user_notification": 1,
        "subtask_progress": 1,
        "external_agent_sign_in": 1,
        "recents_promotion": 1,
    });
    let outpost = outpost();
    let mut connect = base.clone();
    connect["actor_id"] = json!("");
    if let Some(o) = outpost.as_deref() {
        connect["outpostId"] = json!(o);
    }
    if sock.send(&json!({"type": "session_connect", "payload": connect})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while connecting.".into())));
    }
    let mut focus = base.clone();
    focus["session_id"] = json!(thread);
    focus["focus_only"] = json!(true);
    // Quick routes a tool call to the machine that owns an outpost; without
    // one it runs the tool in its own sandbox, which has none of the user's
    // files. The outpost is the one this machine registered (quick.json's
    // `outpost`, or OAK_QUICK_OUTPOST for a stand-in).
    if let Some(o) = outpost.as_deref() {
        focus["outpostId"] = json!(o);
    }
    if sock.send(&json!({"type": "session_connect", "payload": focus})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while joining the thread.".into())));
    }
    let user = user_message(&body, &thread, m, outpost.as_deref());
    if let Err(e) = sock.send(&user) {
        return drop(tx.send(Sse::Err(e)));
    }
    match pump(&mut sock, &folder, &thread, &tx) {
        // The stream says the turn is over: the reply is whole.
        Ok(()) => drop(tx.send(Sse::Done)),
        Err(e) => drop(tx.send(Sse::Err(e))),
    }
}

/// Reads Quick's frames until the message ends, running the tools it asks
/// for and answering each on the socket.
fn pump(sock: &mut Socket, folder: &std::path::Path, thread: &str, tx: &Sender<Sse>) -> Result<(), String> {
    let mut turn = Turn::for_message(thread, "");
    let started = std::time::Instant::now();
    // The tool calls of this turn, so a repeated one isn't run twice.
    let mut ran: Vec<(String, String)> = vec![];
    loop {
        if started.elapsed() > TURN_LIMIT {
            return Err("Quick's turn ran too long; it was given up on.".into());
        }
        let Some(frame) = sock.next() else {
            // The socket closed: a reply that ended is done; one that
            // didn't is an error.
            if !turn.started {
                return Err("Quick closed the connection before the reply began.".into());
            }
            return Ok(());
        };
        let kind = frame["type"].as_str().unwrap_or("").to_string();
        // Another thread's frames, and older messages' of this one, are not
        // this turn's (the socket carries several threads).
        let mine = turn.accepts(&frame);
        if std::env::var("OAK_QUICK_DEBUG").is_ok() {
            eprintln!(
                "FRAME kind={kind:22} thread={:28} msg={:12} accepted={mine}",
                frame["thread_id"].as_str().unwrap_or("-"),
                frame["message_id"].as_str().unwrap_or("-")
            );
        }
        if !mine {
            continue;
        }
        // A message from Quick that Oak doesn't show (keepalives, task
        // bookkeeping, the thread's title changes).
        for ev in turn.feed(&frame) {
            if tx.send(Sse::Event(ev)).is_err() {
                return Ok(());
            }
        }
        match kind.as_str() {
            "tool_call" => {
                let p = &frame["payload"];
                let call = p["tool_call_id"].as_str().unwrap_or("").to_string();
                let name = p["tool_name"].as_str().unwrap_or("").to_string();
                let args = if p["arguments"]["params"]["arguments"].is_null() { p["arguments"].clone() } else { p["arguments"]["params"]["arguments"].clone() };
                let key = (call.clone(), format!("{name}{args}"));
                if call.is_empty() || ran.contains(&key) {
                    continue;
                }
                ran.push(key);
                let result = run_tool(&name, &args, folder);
                let (content, is_error) = match result {
                    Ok(s) => (s, false),
                    Err(e) => (e, true),
                };
                // Quick's own tool ids (`tooluse_…`) come with its
                // `tool_progress`; the answer goes by the call's id.
                let reply = json!({
                    "type": "tool_result",
                    "payload": {
                        "tool_call_id": call,
                        "result": content,
                        "error": if is_error { json!(content) } else { Value::Null },
                    }
                });
                if let Err(e) = sock.send(&reply) {
                    return Err(e);
                }
                // Quick carries the turn on in a new message once it has the
                // result; that message is this turn's too.
                turn.awaiting_continuation = true;
                if tx.send(Sse::Event(Event::BlockEnd { index: turn.blocks().len().saturating_sub(1) })).is_err() {
                    return Ok(());
                }
            }
            "message_complete" => {
                // Quick's agent is done (it has run every turn it wanted):
                // Oak's own loop stops here.
                return Ok(());
            }
            "error" => {
                let why = frame["payload"]["message"].as_str().or(frame["payload"]["error"].as_str()).unwrap_or("Quick reported an error").to_string();
                return Err(format!("Quick: {why}"));
            }
            _ => {}
        }
    }
}

/// Runs one of Quick's tool calls in `folder`. Quick names its tools as its
/// own sandbox does; Oak's equivalents take Oak's inputs.
fn run_tool(name: &str, args: &Value, folder: &std::path::Path) -> Result<String, String> {
    match name {
        // Oak's own names (when Quick's agent uses the ones it was given).
        "READ" => crate::tools::read(folder, args),
        "WRITE" => crate::tools::write(folder, args),
        "EDIT" => crate::tools::edit(folder, args),
        "SEARCH" => crate::tools::search(args),
        "FETCH" => crate::web::fetch(args),
        "EXECUTE" => execute(args, folder),
        // Quick's sandbox's names.
        "file_read" | "read_file" => {
            let input = json!({"path": args["path"].as_str().unwrap_or("")});
            crate::tools::read(folder, &input)
        }
        "file_write" | "write_file" => {
            let input = json!({"path": args["path"].as_str().unwrap_or(""), "content": args["content"].as_str().unwrap_or("")});
            crate::tools::write(folder, &input)
        }
        "file_edit" => {
            let input = json!({
                "path": args["path"].as_str().unwrap_or(""),
                "old": args["old_string"].as_str().or(args["old"].as_str()).unwrap_or(""),
                "new": args["new_string"].as_str().or(args["new"].as_str()).unwrap_or(""),
            });
            crate::tools::edit(folder, &input)
        }
        "folder_list" | "list_dir" | "list_directory" => {
            let path = resolve(folder, args["path"].as_str().unwrap_or(""));
            // Asking with no path is how Quick learns which folders it may
            // work in (`allowed_folders`); it names them this way, in its own
            // sandbox's vocabulary, and every later path comes back in it.
            if args["path"].as_str().unwrap_or("").trim().is_empty() {
                return Ok(json!({
                    "allowed_folders": [format!("/outposts/DESKTOP/{}", native(folder))],
                })
                .to_string());
            }
            let mut entries = vec![];
            if let Ok(read) = std::fs::read_dir(&path) {
                for e in read.flatten().take(500) {
                    let p = e.path();
                    let kind = if p.is_dir() { "directory" } else { "file" };
                    entries.push(json!({
                        "name": e.file_name().to_string_lossy(),
                        "type": kind,
                        "path": format!("/outposts/DESKTOP/{}", native(&p)),
                    }));
                }
            }
            entries.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
            Ok(json!({
                "success": true,
                "path": format!("/outposts/DESKTOP/{}", native(&path)),
                "entries": entries,
                "count": entries.len(),
            })
            .to_string())
        }
        "fdfind" | "fd" => {
            let pattern = args["args"].as_str().unwrap_or("").to_string();
            let found = find(folder, &pattern);
            Ok(json!({"success": true, "output": found}).to_string())
        }
        "ripgrep" | "rg" | "grep" => {
            let pat = args["patterns"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("").to_string();
            let found = grep(folder, &pat, args["options"].as_str().unwrap_or(""));
            Ok(json!({"success": true, "output": found}).to_string())
        }
        "bash" | "shell" | "run_command" | "execute_command" => {
            let cmd = args["command"].as_str().or(args["cmd"].as_str()).unwrap_or("").to_string();
            execute(&json!({"command": cmd}), folder)
        }
        // Quick's own coding tool: Python in the conversation's folder, which
        // is how its agent runs shell commands (`subprocess`) too.
        "run_python" | "python" => run_python(args, folder),
        // Quick's sandbox's other tools (its shell, its browser). Its agent
        // reaches for these when it works locally, so they run here too.
        "run_shell" | "shell" => {
            let cmd = args["command"].as_str().or(args["cmd"].as_str()).unwrap_or("").to_string();
            execute(&json!({ "command": cmd }), folder)
        }
        other => Err(format!(
            "Quick asked for {other}, which Oak doesn't run (Oak has no such tool). Oak's own tools are READ, WRITE, EDIT, EXECUTE, SEARCH and FETCH; run              shell commands with EXECUTE or run_python."
        )),
    }
}

/// The folder a path Quick sent names. Quick works in its outpost's
/// vocabulary (`/outposts/<device>/C:/Users/…`, and `artifacts/`,
/// `agent_files/` beside it), which doesn't exist on this machine: a path
/// under an outpost is the real one, and the workspace's own roots are the
/// conversation's folder. Anything else is resolved against it.
fn resolve(folder: &std::path::Path, path: &str) -> std::path::PathBuf {
    let p = path.trim();
    if p.is_empty() {
        return folder.to_path_buf();
    }
    // `/outposts/<device>/C:/Users/sussy/Documents/oak_public` → `C:/Users/…`.
    let after = p.strip_prefix("/outposts/");
    let real = match after {
        Some(rest) => {
            let without_device = rest.split_once('/').map(|(_, tail)| tail).unwrap_or(rest);
            // The drive comes back with a leading slash (`/C:/…`).
            without_device.trim_start_matches('/').to_string()
        }
        None => p.to_string(),
    };
    // Quick's own workspace roots are the conversation's folder.
    let bare = real.trim_start_matches("workspace/").trim_end_matches('/');
    let roots = ["artifacts", "agent_files", "attached_files"];
    if bare.is_empty() || roots.contains(&bare) || roots.iter().any(|r| bare.starts_with(&format!("{r}/"))) {
        let tail = bare.split_once('/').map(|(_, t)| t).unwrap_or("");
        return if tail.is_empty() { folder.to_path_buf() } else { folder.join(tail) };
    }
    let path = std::path::PathBuf::from(&real);
    // An absolute path as sent (C:/… or /C:/…) is used as it is; a relative
    // one is the conversation's folder's.
    if path.is_absolute() || real.chars().nth(1) == Some(':') {
        if !path.exists() {
            // A drive path sent with a leading slash.
            let fixed = std::path::PathBuf::from(real.trim_start_matches('/'));
            if fixed.exists() {
                return fixed;
            }
        }
        path
    } else {
        folder.join(path)
    }
}

/// A path as Quick's outpost names it (`C:/Users/…`), for the paths it is
/// sent back.
fn native(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Files whose name matches `pattern` (fdfind's `args`, e.g. `-i -H pong`),
/// from the conversation's folder down.
fn find(folder: &std::path::Path, pattern: &str) -> String {
    let needle = pattern.split_whitespace().filter(|w| !w.starts_with('-')).collect::<Vec<_>>().join(" ").to_lowercase();
    let mut out = vec![];
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for e in read.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".oak") {
                continue;
            }
            if p.is_dir() {
                stack.push(p.clone());
            }
            if needle.is_empty() || name.contains(&needle) {
                out.push(native(&p));
                if out.len() >= 200 {
                    out.sort();
                    return out.join("\n");
                }
            }
        }
    }
    out.sort();
    out.join("\n")
}

/// Lines matching the first of ripgrep's `patterns`, from the conversation's
/// folder down (`options` may say `-i` for case-insensitive).
fn grep(folder: &std::path::Path, pattern: &str, options: &str) -> String {
    if pattern.is_empty() {
        return String::new();
    }
    let insensitive = options.split_whitespace().any(|o| o.starts_with('-') && !o.starts_with("--") && o[1..].contains('i'));
    let needle = if insensitive { pattern.to_lowercase() } else { pattern.to_string() };
    let mut out = vec![];
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for e in read.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".oak") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            // A few megabytes at most: a search of the project, not of data.
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() > 2_000_000 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            if bytes.contains(&0) {
                continue; // binary
            }
            let text = String::from_utf8_lossy(&bytes);
            for (n, line) in text.lines().enumerate() {
                let hit = if insensitive { line.to_lowercase().contains(&needle) } else { line.contains(&needle) };
                if hit {
                    out.push(format!("{}:{}: {}", native(&p), n + 1, line.trim_end()));
                    if out.len() >= 200 {
                        return out.join("\n");
                    }
                }
            }
        }
    }
    out.join("\n")
}

/// Runs a shell command in `folder`, with the output clipped for a tool
/// result.
fn execute(input: &Value, folder: &std::path::Path) -> Result<String, String> {    let command = input["command"].as_str().unwrap_or("").trim();
    if command.is_empty() {
        return Err("EXECUTE needs a `command`.".into());
    }
    let timeout = input["timeout"].as_u64().unwrap_or(120).min(crate::tools::EXECUTE_TIMEOUT_MAX.max(1));
    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(command)
            .current_dir(folder)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
    };
    #[cfg(not(windows))]
    let spawned: std::io::Result<std::process::Child> = std::process::Command::new("bash").arg("-lc").arg(command).current_dir(folder).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn();
    let mut child = spawned.map_err(|e| format!("Couldn't run `{command}`: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                return Err(format!("`{command}` ran past its {timeout}s."));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("`{command}` failed: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut text = stdout.into_owned();
    if text.trim().is_empty() {
        text = stderr.into_owned();
    } else if !stderr.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&stderr);
    }
    if text.trim().is_empty() {
        text = format!("`{command}` finished with {}.", out.status);
    }
    Ok(crate::tools::clip(&text))
}

/// Runs a Python snippet in `folder` (Quick's own coding tool: its agent
/// uses it for shell work too), with the output clipped for a tool result.
fn run_python(args: &Value, folder: &std::path::Path) -> Result<String, String> {
    let code = args["code"].as_str().or(args["script"].as_str()).or(args["source"].as_str()).unwrap_or("");
    if code.trim().is_empty() {
        return Err("run_python needs `code`.".into());
    }
    // The snippet goes beside Oak's scratchpad, so the working folder's own
    // files aren't touched.
    let dir = folder.join(".oak").join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("quick-{}.py", crate::api::now_ms()));
    std::fs::write(&file, code).map_err(|e| format!("Couldn't write the snippet: {e}"))?;
    let mut child = python_command()
        .arg(&file)
        .current_dir(folder)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't start Python: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                return Err("The snippet ran past its 120s.".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("Python failed: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push_str("\n[stderr]\n");
        }
        text.push_str(&stderr);
    }
    if text.trim().is_empty() {
        text = format!("The snippet finished and printed nothing ({}).", out.status);
    }
    Ok(crate::tools::clip(&text))
}

/// Python: the environment's, or the usual launcher on Windows.
fn python_command() -> std::process::Command {
    if let Ok(p) = std::env::var("OAK_TEST_PYTHON")
        && !p.trim().is_empty()
    {
        return std::process::Command::new(p);
    }
    #[cfg(windows)]
    let candidates = ["python", "py", "python3"];
    #[cfg(not(windows))]
    let candidates = ["python3", "python"];
    for c in candidates {
        if std::process::Command::new(c)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return std::process::Command::new(c);
        }
    }
    std::process::Command::new(candidates[0])
}

/// The outpost Quick routes this machine's tool calls to: the one saved at
/// sign-in, or `OAK_QUICK_OUTPOST` (the desktop app registers one per
/// machine; Quick's sandbox is used when there is none).
fn outpost() -> Option<String> {
    if let Ok(o) = std::env::var("OAK_QUICK_OUTPOST") {
        let o = o.trim().to_string();
        if !o.is_empty() {
            return Some(o);
        }
    }
    let saved = crate::quick::login::saved_outpost();
    (!saved.is_empty()).then_some(saved)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
