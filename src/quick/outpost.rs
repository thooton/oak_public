//! Oak as a Quick outpost: this machine registers itself with Quick, so the
//! tools Quick's agent asks for are routed here to be run (by `turn`), in the
//! conversation's folder, instead of in Quick's empty sandbox.
//!
//! Quick's desktop app does this with `putOutpost`, an ordinary authenticated
//! call on the same host as the chat socket: it names the outpost, its kind
//! and the tools it can run. Oak registers the same tools it answers
//! (`turn::run_tool`), so what Quick is told it may ask for is what Oak runs.
//! A tool Quick routes here also needs a policy on the outpost (`policies`):
//! without one Quick refuses the call as having "no connector id".
//!
//! **Files to Quick's workspace** (`upload`): Quick's agent sees a file on this
//! machine (an image above all) by asking for `file_upload`; the device puts
//! the file in the thread's workspace, as the app does an attachment, and
//! answers with its path there. The bytes go to `upload-attachment`, Quick's
//! data loss prevention scan is waited on (`attachment-dlp-status`), and the
//! attachment is delivered to the thread (`files/upload/<thread>`), which
//! answers with the file's path in the workspace. Oak uses the same upload for
//! the images its own tools return and for those the user attaches.

use super::*;

/// The outpost's tools, as Quick's desktop app declares them: the names its
/// agent calls, each with the shape of its input and whether it only reads.
/// `turn::run_tool` answers exactly these.
pub const TOOLS: &[(&str, bool)] = &[
    ("file_matches", true),
    ("folder_list", true),
    ("file_edit", false),
    ("folder_create", false),
    ("file_delete", false),
    ("file_copy", false),
    ("file_type", true),
    ("fdfind", true),
    ("ripgrep", true),
    ("file_upload", true),
    ("run_python_outpost_local", true),
    ("run_python_outpost_local_with_write", false),
];

/// The tool a name Quick sends is, as `turn::run_tool` knows it.
pub fn tool_schema(name: &str) -> Value {
    let obj = |props: Value, required: Value| json!({"type": "object", "properties": props, "required": required});
    let s = |d: &str| json!({"type": "string", "description": d});
    match name {
        "file_matches" => obj(json!({"path": s("Path or name to match")}), json!(["path"])),
        "folder_list" => obj(
            json!({
                "path": {"type": "string", "default": "", "description": "Path to the directory to list. Relative paths resolve to the session workspace. If empty, lists allowed folders."},
                "show_hidden": {"type": "boolean", "default": false, "description": "Show hidden entries"}
            }),
            json!([]),
        ),
        "file_edit" => obj(
            json!({
                "path": s("Path to the file to edit."),
                "diff": s("Unified diff string to apply"),
                "old_string": s("Text to replace"),
                "new_string": s("Replacement text")
            }),
            json!(["path"]),
        ),
        "folder_create" => obj(
            json!({"path": s("Path for the new folder to create.")}),
            json!(["path"]),
        ),
        "file_delete" => obj(json!({"path": s("Path to delete.")}), json!(["path"])),
        "file_copy" => obj(
            json!({"source": s("Source path"), "destination": s("Destination path")}),
            json!(["source", "destination"]),
        ),
        "file_type" => obj(json!({"path": s("Path to the file")}), json!(["path"])),
        "file_upload" => obj(
            json!({
                "path": s("Path of the file on this device"),
                "session_id": s("The thread whose workspace the file goes to")
            }),
            json!(["path"]),
        ),
        "fdfind" => obj(
            json!({
                "args": s("fd arguments as a single string (pattern, flags, etc.)."),
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Directories to search in"}
            }),
            json!(["args"]),
        ),
        "ripgrep" => obj(
            json!({
                "patterns": {"type": "array", "items": {"type": "string"}, "description": "One or more search patterns"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Paths to search"},
                "options": s("Extra rg options, e.g. -i -n")
            }),
            json!(["patterns"]),
        ),
        "run_python_outpost_local" | "run_python_outpost_local_with_write" => obj(
            json!({
                "code": {"type": "string", "default": "", "description": "One or more of Oak's tool calls written as Python calls, a line each, with literal arguments: READ(path=\"src/main.rs\"), EXECUTE(command=\"cargo test\", summary=\"Run the tests\"). Not Python: nothing but the calls runs."},
                "timeout": {"type": "integer", "default": 60, "description": "Maximum execution time in seconds"}
            }),
            json!(["code"]),
        ),
        _ => obj(json!({}), json!([])),
    }
}

/// This machine's outpost id: saved in quick.json at first use, so the same
/// one is registered again rather than a new one each start.
pub fn id() -> String {
    let saved = login::saved_outpost();
    if !saved.is_empty() {
        return saved;
    }
    // The machine's own outpost: Quick's desktop app registers one per
    // machine, and it is the one Quick reports ONLINE for this device, so
    // Oak answers as that outpost rather than registering a second name that
    // nothing routes to.
    login::set_outpost(DEFAULT_OUTPOST_ID);
    DEFAULT_OUTPOST_ID.to_string()
}

/// The name Quick shows for this outpost.
fn name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Oak".into())
}

/// Registers this machine's outpost with Quick, so the tools Quick's agent
/// asks for are routed here. Called before a turn: registering again is
/// harmless (Quick takes the newest description of the same outpost).
pub fn register() -> Result<(), String> {
    register_as(&id())
}

/// Registers `outpost` with Quick (the same call as [`register`], for a named
/// outpost: a probe uses its own, so what it measures is its own).
pub fn register_as(outpost: &str) -> Result<(), String> {
    let token = login::access_token()?;
    let host = login::host()?;
    let capabilities = ["file_operations", "code_execution"]
        .iter()
        .map(|group| {
            let tools: Vec<Value> = TOOLS
                .iter()
                .filter(|(n, _)| match *group {
                    "code_execution" => n.starts_with("run_python"),
                    _ => !n.starts_with("run_python"),
                })
                .map(|(n, reads)| {
                    json!({
                        "name": n,
                        "description": describe(n),
                        "inputSchema": tool_schema(n),
                        "isReadTool": reads,
                    })
                })
                .collect();
            json!({"name": group, "tools": tools})
        })
        .collect::<Vec<_>>();
    let body = json!({
        "eventType": "putOutpost",
        "eventBody": {"putOutpost": {
            "outpostId": outpost,
            "name": name(),
            "outpostType": "DESKTOP",
            "capabilities": capabilities,
        }}
    });
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "outpost")
        .header("x-amzn-quick-atype", "sbg")
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(api::network_error)?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!(
            "Quick refused this machine's outpost ({status}): {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    // (A tool without a policy is refused; failing here only costs those.)
    let _ = policies(&token, &host, outpost);
    Ok(())
}

/// One of Quick's events (`qbs-event`) as the outpost sends it: what it
/// answered (`eventResponse`), or Err with what was wrong.
fn event(token: &str, host: &str, kind: &str, body: Value) -> Result<Value, String> {
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "outpost")
        .header("x-amzn-quick-atype", "sbg")
        .json(&json!({"eventType": kind, "eventBody": {kind: body}}))
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(api::network_error)?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) || v["errorMessage"].is_string() {
        return Err(format!(
            "Quick refused {kind} ({status}): {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    Ok(v["eventResponse"].clone())
}

/// Gives each of `TOOLS` that has none a policy on `outpost`: the tool's
/// group (`connectorId`), and whether it may run without asking (the reading
/// ones) or asks each time, as Quick sets them for the app's own tools. The
/// policies the outpost has are kept as they are (the user may have changed
/// them), and nothing is sent when none is missing.
fn policies(token: &str, host: &str, outpost: &str) -> Result<(), String> {
    let got = event(
        token,
        host,
        "batchGetPolicy",
        json!({"policyType": "OUTPOST", "resourceIds": [outpost]}),
    )?;
    let current = &got["batchGetPolicyResponse"]["policies"][0];
    let mut tools = current["toolPolicies"].as_array().cloned().unwrap_or_default();
    let had = tools.len();
    for (name, reads) in TOOLS {
        if tools.iter().any(|t| t["toolName"] == *name) {
            continue;
        }
        tools.push(json!({
            "toolName": name,
            "enabled": true,
            "connectorId": if name.starts_with("run_python") { "code_execution" } else { "file_operations" },
            "permissionPolicy": if *reads { "ALWAYS_ALLOW" } else { "ASK_EACH_TIME" },
            "originPolicy": "ASK_EACH_TIME",
        }));
    }
    if tools.len() == had {
        return Ok(());
    }
    event(
        token,
        host,
        "batchPutPolicy",
        json!({"policyType": "OUTPOST", "policies": [{
            "resourceId": outpost,
            "enabled": current["enabled"].as_bool().unwrap_or(true),
            "toolPolicies": tools,
        }]}),
    )
    .map(|_| ())
}

/// The most Quick takes in one attachment (the app's own limit).
const MAX_UPLOAD: u64 = 95 * 1024 * 1024;

/// The media type a file's name says it is.
pub fn media_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
}

/// `file_upload`: the file at `path`, on this machine, put in thread
/// `session`'s workspace. Answers as Quick's agent reads it, with the file's
/// `path` there.
pub fn upload_file(path: &std::path::Path, session: &str) -> Result<Value, String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| format!("Couldn't read {shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown} isn't a file."));
    }
    if meta.len() > MAX_UPLOAD {
        return Err(format!("{shown} is too big to upload ({} bytes; Quick takes at most 95 MB).", meta.len()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {shown}: {e}"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut v = upload(&bytes, &name, media_type(&name), session)?;
    v["success"] = json!(true);
    Ok(v)
}

/// `bytes`, named `name`, put in thread `session`'s workspace, as Quick's app
/// does an attachment: what the delivery answers (`name`, `path`,
/// `workspace_path`, `size`).
pub fn upload(bytes: &[u8], name: &str, media_type: &str, session: &str) -> Result<Value, String> {
    if session.is_empty() || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("Quick named no thread to upload {name} to."));
    }
    if bytes.is_empty() {
        return Err(format!("{name} is empty."));
    }
    let token = login::access_token()?;
    let host = login::host()?;
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/upload-attachment"))
        .query(&[
            ("name", name.to_string()),
            ("contentType", media_type.to_string()),
            ("contentLength", bytes.len().to_string()),
        ])
        .bearer_auth(&token)
        .header("Content-Type", "application/octet-stream")
        .body(bytes.to_vec())
        .timeout(Duration::from_secs(300))
        .send()
        .map_err(api::network_error)?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("Quick refused the upload of {name} ({status}): {}", text.chars().take(300).collect::<String>()));
    }
    let attachment = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["attachmentId"].as_str().map(String::from));
    let Some(attachment) = attachment else {
        return Err(format!("Quick's upload of {name} answered without an attachment: {}", text.chars().take(300).collect::<String>()));
    };
    // Quick scans what it is given before the thread may have it.
    let mut verdict = String::new();
    for _ in 0..60 {
        let resp = api::http()
            .get(format!("https://{host}/integration/quick-work/attachment-dlp-status"))
            .query(&[("attachmentId", attachment.as_str())])
            .bearer_auth(&token)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(api::network_error)?;
        let v: Value = serde_json::from_str(&resp.text().unwrap_or_default()).unwrap_or(Value::Null);
        let state = v["dlpJobStatus"].as_str().unwrap_or("").to_ascii_uppercase();
        match state.as_str() {
            "COMPLETED" | "SKIPPED" | "NOT_APPLICABLE" => {
                verdict = v["policyAction"].as_str().unwrap_or("ALLOW").to_ascii_uppercase();
                break;
            }
            "FAILED" | "ERROR" => return Err(format!("Quick's data loss prevention scan of {name} failed.")),
            _ => std::thread::sleep(Duration::from_millis(1500)),
        }
    }
    match verdict.as_str() {
        "ALLOW" => {}
        "" => return Err(format!("Quick's data loss prevention scan of {name} didn't finish.")),
        v => return Err(format!("Quick's data loss prevention policy kept {name} out of the thread ({v}).")),
    }
    let envelope = json!({
        "eventType": "invokeAgent",
        "eventBody": {"invokeAgent": {
            "path": format!("files/upload/{session}"),
            "queryParams": {"_method": "POST"},
            "body": json!({"attachmentId": attachment}).to_string(),
            "outpostId": id(),
        }}
    });
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "user-conversation")
        .header("x-amzn-quick-atype", "ufg")
        .json(&envelope)
        .timeout(Duration::from_secs(60))
        .send()
        .map_err(api::network_error)?;
    let text = resp.text().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    // The agent answers in its own envelope: a status and a body, as JSON text.
    let inner: Value = v["eventResponse"]["invokeAgentResponse"]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let body: Value = inner["body"].as_str().and_then(|b| serde_json::from_str(b).ok()).unwrap_or(Value::Null);
    let delivered = inner["status"].as_u64().is_some_and(|s| (200..300).contains(&s));
    if !delivered || !body["path"].is_string() {
        let why = v["errorMessage"].as_str().map(String::from).unwrap_or_else(|| text.chars().take(300).collect());
        return Err(format!("Quick didn't put {name} in the thread: {why}"));
    }
    Ok(body)
}

/// Registers `folder` as one this outpost may work in (`graph/folder`), so
/// Quick knows the conversation's folder lives on this machine. Doing it
/// again is harmless.
pub fn register_folder(token: &str, host: &str, folder: &str) -> Result<(), String> {
    let outpost = id();
    let body = json!({
        "path": folder,
        "device_id": outpost,
        "storage_type": "local",
        "kg_enabled": false,
        "rag_enabled": false,
    });
    let envelope = json!({
        "eventType": "invokeAgent",
        "eventBody": {"invokeAgent": {
            "path": "graph/folder",
            "queryParams": {"_method": "POST"},
            "body": body.to_string(),
            "outpostId": id(),
        }}
    });
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "user-conversation")
        .header("x-amzn-quick-atype", "ufg")
        .json(&envelope)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(api::network_error)?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!(
            "Quick refused the folder ({status}): {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    // The agent answers in its own envelope; a refusal is inside it.
    if let Some(inner) = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
        v["eventResponse"]["invokeAgentResponse"]
            .as_str()
            .map(String::from)
    }) {
        let parsed: Value = serde_json::from_str(&inner).unwrap_or(Value::Null);
        let body = parsed["body"].as_str().unwrap_or("");
        if let Ok(b) = serde_json::from_str::<Value>(body)
            && b["success"].as_bool() == Some(false)
        {
            return Err(format!("Quick refused the folder: {body}"));
        }
    }
    Ok(())
}

/// Tells Quick this outpost's device state (`putDeviceState`), trying the
/// field names the API might use for "this machine is here" and reporting
/// which one the server kept. The validator names only fields it requires
/// (`hasUsedDesktop`), never the one that carries the state, so the shape is
/// found by trying them.
pub fn put_device_state_report(token: &str, host: &str) -> Vec<(String, bool, String)> {
    let outpost = id();
    let mut out = vec![];
    for field in [
        "online",
        "isOnline",
        "status",
        "state",
        "availability",
        "presence",
        "connected",
        "isConnected",
        "deviceStatus",
        "connectionState",
        "outpostStatus",
        "lastSeenAt",
        "lastSeen",
        "heartbeatAt",
        "isAvailable",
        "available",
        "isActive",
        "active",
        "live",
        "isLive",
        "registered",
        "isRegistered",
        "reconnectGraceMs",
    ] {
        let value = match field {
            "online" | "isOnline" | "connected" | "isConnected" | "isAvailable" | "available"
            | "isActive" | "active" | "live" | "isLive" | "registered" | "isRegistered" => {
                json!(true)
            }
            "lastSeenAt" | "lastSeen" | "heartbeatAt" => json!(crate::api::now_ms()),
            "reconnectGraceMs" => json!(0),
            _ => json!("ONLINE"),
        };
        let body = json!({"hasUsedDesktop": true, "deviceId": outpost, "outpostId": outpost, field: value});
        let envelope =
            json!({"eventType": "putDeviceState", "eventBody": {"putDeviceState": body}});
        let resp = api::http()
            .post(format!("https://{host}/integration/quick-work/qbs-event"))
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .header("X-QuickDsk-mdid", "outpost")
            .header("x-amzn-quick-atype", "sbg")
            .json(&envelope)
            .timeout(Duration::from_secs(20))
            .send();
        let (ok, note) = match resp {
            Ok(r) => {
                let t = r.text().unwrap_or_default();
                (
                    t.contains("putDeviceStateResponse\":{"),
                    t.chars().take(160).collect::<String>(),
                )
            }
            Err(e) => (false, format!("{e}")),
        };
        out.push((field.to_string(), ok, note));
    }
    out
}

/// Whether Quick lists this machine's outpost as ONLINE.
pub fn is_online(token: &str, host: &str) -> Result<bool, String> {
    is_online_of(token, host, &id())
}

/// Whether Quick lists `outpost` as ONLINE (a probe asks about its own).
pub fn is_online_of(token: &str, host: &str, outpost: &str) -> Result<bool, String> {
    let body = json!({"eventType": "listOutposts", "eventBody": {"listOutposts": {"scope": "USER", "surface": "QUICK_WORK"}}});
    let resp = api::http()
        .post(format!("https://{host}/integration/quick-work/qbs-event"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("X-QuickDsk-mdid", "outpost")
        .header("x-amzn-quick-atype", "sbg")
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(api::network_error)?;
    let text = resp.text().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let items = v["eventResponse"]["listOutpostsResponse"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .any(|o| o["id"].as_str() == Some(outpost) && o["status"].as_str() == Some("ONLINE")))
}

/// Waits (briefly) until Quick lists `outpost` as ONLINE. A socket that names
/// its outpost in its address turns that outpost ONLINE when its session
/// connects; a turn that asks for a tool before Quick has listed it would have
/// that tool run in Quick's own sandbox instead of here. The wait is short and
/// best-effort: a turn still goes ahead without it.
pub fn await_online(token: &str, host: &str, outpost: &str) {
    for i in 0..8 {
        if is_online_of(token, host, outpost).unwrap_or(false) {
            if std::env::var("OAK_QUICK_DEBUG").is_ok() {
                eprintln!("outpost {outpost} is ONLINE (after {i} polls)");
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if std::env::var("OAK_QUICK_DEBUG").is_ok() {
        eprintln!("outpost {outpost} never showed ONLINE before the turn");
    }
}

/// What a tool does, as Quick's agent reads it.
fn describe(name: &str) -> &'static str {
    match name {
        "file_matches" => "Find paths matching a name or pattern, in the conversation's folder.",
        "folder_list" => "List a directory. With no path, lists the folders this machine allows.",
        "file_edit" => "Edit a file: apply a unified diff, or replace old_string with new_string.",
        "folder_create" => "Create a folder.",
        "file_delete" => "Delete a file or empty folder.",
        "file_copy" => "Copy a file or folder.",
        "file_type" => "Say what kind of file this is, and its size.",
        "file_upload" => "Put a file from this machine in the thread's workspace, and answer with its path there.",
        "fdfind" => {
            "Find files and folders by name (fd's arguments: a regex, -e, -t, -g, -H, -I, -d), in the paths given or the conversation's folder."
        }
        "ripgrep" => {
            "Search file contents with ripgrep: every pattern a regex, rg's own options, in the paths given or the conversation's folder."
        }
        "run_python_outpost_local" | "run_python_outpost_local_with_write" => {
            "Run Oak's tools in the conversation's folder: the code is one or more Oak tool calls written as Python calls, a line each, with literal arguments (READ(path=\"src/main.rs\"), EXECUTE(command=\"cargo test\", summary=\"Run the tests\")), all run at the same time, and the result has each call's. It isn't Python: nothing but the calls runs."
        }
        _ => "A tool this machine runs.",
    }
}
