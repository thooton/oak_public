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
    let base = json!({
        "surfaceType": SURFACE,
        "appVersion": APP_VERSION,
        "acp_first_text": 1,
        "user_notification": 1,
        "subtask_progress": 1,
        "external_agent_sign_in": 1,
        "recents_promotion": 1,
    });
    let mut connect = base.clone();
    connect["actor_id"] = json!("");
    if sock.send(&json!({"type": "session_connect", "payload": connect})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while connecting.".into())));
    }
    let mut focus = base.clone();
    focus["session_id"] = json!(thread);
    focus["focus_only"] = json!(true);
    if sock.send(&json!({"type": "session_connect", "payload": focus})).is_err() {
        return drop(tx.send(Sse::Err("Quick closed the socket while joining the thread.".into())));
    }
    let user = user_message(&body, &thread, m);
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
            let input = json!({"command": format!("ls -la {}", args["path"].as_str().unwrap_or("."))});
            execute(&input, folder)
        }
        "fdfind" | "fd" => {
            let input = json!({"command": format!("find . -iname '*{}*' -not -path '*/node_modules/*' | head -200", args["args"].as_str().unwrap_or("").replace(['*', '\'', '"'], ""))});
            execute(&input, folder)
        }
        "ripgrep" | "rg" | "grep" => {
            let pat = args["patterns"].as_array().and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
            let input = json!({"command": format!("grep -rIn --max-count=50 -- {pat:?} . 2>/dev/null | head -200")});
            execute(&input, folder)
        }
        "bash" | "shell" | "run_command" | "execute_command" => {
            let cmd = args["command"].as_str().or(args["cmd"].as_str()).unwrap_or("").to_string();
            execute(&json!({"command": cmd}), folder)
        }
        // Quick's own coding tool: Python in the conversation's folder, which
        // is how its agent runs shell commands (`subprocess`) too.
        "run_python" | "python" => run_python(args, folder),
        other => Err(format!("Quick asked for {other}, which Oak doesn't run. Oak's tools: READ, WRITE, EDIT, EXECUTE, SEARCH, FETCH.")),
    }
}

/// Runs a shell command in `folder`, with the output clipped for a tool
/// result.
fn execute(input: &Value, folder: &std::path::Path) -> Result<String, String> {
    let command = input["command"].as_str().unwrap_or("").trim();
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
