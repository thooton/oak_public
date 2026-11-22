//! Oak as a Quick outpost: this machine registers itself with Quick, so the
//! tools Quick's agent asks for are routed here to be run (by `turn`), in the
//! conversation's folder, instead of in Quick's empty sandbox.
//!
//! Quick's desktop app does this with `putOutpost`, an ordinary authenticated
//! call on the same host as the chat socket: it names the outpost, its kind
//! and the tools it can run. Oak registers the same tools it answers
//! (`turn::run_tool`), so what Quick is told it may ask for is what Oak runs.

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
        "folder_create" => obj(json!({"path": s("Path for the new folder to create.")}), json!(["path"])),
        "file_delete" => obj(json!({"path": s("Path to delete.")}), json!(["path"])),
        "file_copy" => obj(json!({"source": s("Source path"), "destination": s("Destination path")}), json!(["source", "destination"])),
        "file_type" => obj(json!({"path": s("Path to the file")}), json!(["path"])),
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
                "code": {"type": "string", "default": "", "description": "The Python code to execute."},
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
    if !saved.is_empty() && saved != DEFAULT_OUTPOST_ID {
        return saved;
    }
    let new = format!("oak-{}", &hex_uuid().replace('-', "")[..20]);
    login::set_outpost(&new);
    new
}

/// The name Quick shows for this outpost.
fn name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "Oak".into())
}

/// Registers this machine's outpost with Quick, so the tools Quick's agent
/// asks for are routed here. Called before a turn: registering again is
/// harmless (Quick takes the newest description of the same outpost).
pub fn register() -> Result<(), String> {
    let token = login::access_token()?;
    let host = login::host()?;
    let outpost = id();
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
        return Err(format!("Quick refused this machine's outpost ({status}): {}", text.chars().take(200).collect::<String>()));
    }
    Ok(())
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
        "fdfind" => "Find files by name (fd), searching the conversation's folder.",
        "ripgrep" => "Search file contents (ripgrep), searching the conversation's folder.",
        "run_python_outpost_local" => "Run Python in the conversation's folder.",
        "run_python_outpost_local_with_write" => "Run Python in the conversation's folder, which may write files there.",
        _ => "A tool this machine runs.",
    }
}
