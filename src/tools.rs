// The agent's tools. Each returns the text sent back as the tool
// result; Err means the result is marked is_error.

use serde_json::{Value, json};
pub use oak_sdk::ui::{Head, one_line};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

mod defs;
pub use defs::{browser_help, definitions_with_kits, kit_help, kit_of, kits_for};
mod search;
pub use search::{parse_results as search_results, search};

pub const MAX_OUTPUT: usize = 30_000;

/// The longest TASK wait may wait, in seconds.
pub const TASK_WAIT_MAX: u64 = 900;

/// The `summary` of EXECUTE and TASK new, which the user sees in place of
/// the command.
const SUMMARY: &str = "A short present-tense label shown to the user in place of the command, e.g. \"Run the unit tests\"";

/// The shell EXECUTE and TASK new run in, as their descriptions name it.
#[cfg(windows)]
const SHELL: (&str, &str) = ("Git Bash (Windows)", "Git Bash");
#[cfg(not(windows))]
const SHELL: (&str, &str) = ("bash", "bash");

/// The range of EXECUTE's `timeout`, in seconds (0 moves the command to
/// the background at once, unless it is already done).
pub const EXECUTE_TIMEOUT_MIN: u64 = 0;
pub const EXECUTE_TIMEOUT_MAX: u64 = 550;

/// The longest AGENT wait may wait, in seconds.
pub const AGENT_WAIT_MAX: u64 = 900;

/// Tools only a session the user started has: a subagent can't ask the
/// user, name itself, start (stop, delete) agents of its own or set up
/// progress meters.
pub const MAIN_ONLY: [&str; 10] =
    ["ASK", "REQUEST_ASK", "AGENT_NEW", "AGENT_SET", "AGENT_PAUSE", "AGENT_RESUME", "AGENT_STOP", "AGENT_DELETE", "PROGRESS_SET", "PROGRESS_REMOVE"];

/// Of MAIN_ONLY, what a sub-orchestrator (a subagent started with
/// `can_spawn`) has too: managing agents of its own.
pub const SPAWNER_TOO: [&str; 6] = ["AGENT_NEW", "AGENT_SET", "AGENT_PAUSE", "AGENT_RESUME", "AGENT_STOP", "AGENT_DELETE"];

/// Whether a session may use tool `name`: `agent` for a subagent,
/// `spawner` for a sub-orchestrator. The browser's tools (`main_only`)
/// stay with the main session even for a sub-orchestrator.
pub fn allowed(name: &str, agent: bool, spawner: bool) -> bool {
    !agent || !main_only(name) || (spawner && SPAWNER_TOO.contains(&name))
}

/// Tools the model gets as one, picked by its `action`: (the tool, its
/// actions and the tool OAK runs for each). OAK runs, checks and shows
/// the calls by those names (`canonical`), so a conversation from before
/// the families (which called TASK_WAIT and so on) carries on as it was.
const FAMILIES: &[(&str, &[(&str, &str)])] = &[
    ("TASK", &[("new", "TASK_NEW"), ("output", "TASK_GET_NEW_OUTPUT"), ("wait", "TASK_WAIT"), ("list", "TASK_LIST"), ("delete", "TASK_DELETE")]),
    ("NOTE", &[("new", "NOTE_NEW"), ("update", "NOTE_UPDATE"), ("delete", "NOTE_DELETE")]),
    ("HEARTBEAT", &[("start", "HEARTBEAT_START"), ("stop", "HEARTBEAT_STOP")]),
    ("PROGRESS", &[("set", "PROGRESS_SET"), ("remove", "PROGRESS_REMOVE")]),
    ("GOAL", &[("justify", "GOAL_JUSTIFICATION"), ("finish", "GOAL_FINISH")]),
    ("AGENT", &[("new", "AGENT_NEW"), ("send", "AGENT_SEND"), ("wait", "AGENT_WAIT"), ("list", "AGENT_LIST"), ("set", "AGENT_SET"), ("pause", "AGENT_PAUSE"), ("resume", "AGENT_RESUME"), ("stop", "AGENT_STOP"), ("delete", "AGENT_DELETE")]),
    ("BROWSER", &[
        ("goto", "BROWSER_GOTO"), ("back", "BROWSER_BACK"), ("forward", "BROWSER_FORWARD"), ("reload", "BROWSER_RELOAD"),
        ("read", "BROWSER_READ"), ("click", "BROWSER_CLICK"), ("type", "BROWSER_TYPE"), ("key", "BROWSER_KEY"),
        ("scroll", "BROWSER_SCROLL"), ("screenshot", "BROWSER_SCREENSHOT"), ("eval", "BROWSER_EVAL"),
        ("dialog", "BROWSER_DIALOG"), ("upload", "BROWSER_UPLOAD"), ("help", "BROWSER_HELP"),
    ]),
];

/// Whether a tool OAK runs is only for a session the user started: those
/// in MAIN_ONLY, and the browser's (the pane belongs to the conversation
/// the user opened it in).
pub fn main_only(tool: &str) -> bool {
    MAIN_ONLY.contains(&tool) || tool.starts_with("BROWSER_")
}

/// The family a tool OAK runs belongs to, and its action there:
/// `TASK_WAIT` is TASK's `wait`.
fn family_of(tool: &str) -> Option<(&'static str, &'static str)> {
    FAMILIES.iter().find_map(|(family, actions)| actions.iter().find(|(_, t)| *t == tool).map(|(a, _)| (*family, *a)))
}

/// The tool OAK runs for a call, and its input: a family's call (`TASK`
/// with `"action": "wait"`) is the action's tool (TASK_WAIT), with the
/// action taken out of the input; any other call is as it came. Fails for
/// a family call without a known action.
pub fn canonical(name: &str, input: &Value) -> Result<(String, Value), String> {
    let Some((_, actions)) = FAMILIES.iter().find(|(f, _)| *f == name) else {
        return Ok((name.to_string(), input.clone()));
    };
    let list = actions.iter().map(|(a, _)| *a).collect::<Vec<_>>().join(", ");
    let action = input["action"].as_str().map(|a| a.trim().to_lowercase());
    let Some(action) = action else {
        return Err(format!("Missing required field `action` (one of {list})"));
    };
    let Some((_, tool)) = actions.iter().find(|(a, _)| *a == action) else {
        return Err(format!("Unknown action `{action}` for {name}: use one of {list}"));
    };
    let mut input = input.clone();
    if let Some(o) = input.as_object_mut() {
        o.remove("action");
    }
    Ok((tool.to_string(), input))
}

/// The tool a call runs as (`canonical`'s name), without copying its
/// input: a family's call as its action's tool, any other (or a family
/// call without a known action) as it is named.
pub fn canonical_name<'a>(name: &'a str, input: &Value) -> &'a str {
    let Some((_, actions)) = FAMILIES.iter().find(|(f, _)| *f == name) else { return name };
    let action = input["action"].as_str().map(|a| a.trim().to_lowercase());
    action.and_then(|action| actions.iter().find(|(a, _)| *a == action)).map_or(name, |(_, tool)| tool)
}

/// A call as OAK runs and shows it, without copying a call that isn't a
/// family's (`canonical`; a family call whose action hasn't streamed in
/// yet, or is wrong, stays as it is).
pub fn canonical_cow<'a>(name: &'a str, input: &'a Value) -> (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, Value>) {
    use std::borrow::Cow;
    if FAMILIES.iter().any(|(f, _)| *f == name)
        && let Ok((n, i)) = canonical(name, input)
    {
        return (Cow::Owned(n), Cow::Owned(i));
    }
    (Cow::Borrowed(name), Cow::Borrowed(input))
}

/// A tool OAK runs as the transcript names it: a family's as the family and
/// action ("TASK wait" for TASK_WAIT, also in conversations from before the
/// families), any other as it is.
pub fn shown_name(tool: &str) -> String {
    match family_of(tool) {
        Some((family, action)) => format!("{family} {action}"),
        None => tool.to_string(),
    }
}

/// How a call of a tool OAK runs is named to the model, e.g. `TASK
/// action=wait` for TASK_WAIT, `READ` for READ.
pub fn call_name(tool: &str) -> String {
    match family_of(tool) {
        Some((family, action)) => format!("{family} action={action}"),
        None => tool.to_string(),
    }
}

/// The tools of a session the user started, or (`agent`) of a subagent,
/// as the model gets them.
#[cfg(test)]
pub fn definitions(agent: bool) -> Value {
    definitions_as(agent, false, false, false)
}

/// What a model on the user's own llama.cpp server gets (`local`): the
/// basics for simple jobs, as it is small, slow and has a small context.
/// No notes, heartbeats, progress meters, subagents (it serves one request
/// at a time) or SCREENSHOT (it sees no images); GOAL and REQUEST_ASK only
/// while a goal is set, so that one can be finished.
pub const LOCAL_TOOLS: [&str; 13] = [
    "READ", "WRITE", "EDIT", "EXECUTE", "TASK_NEW", "TASK_GET_NEW_OUTPUT", "TASK_WAIT", "TASK_LIST", "TASK_DELETE", "SEARCH", "FETCH", "ASK", "DISPLAY",
];
const GOAL_TOOLS: [&str; 3] = ["GOAL_JUSTIFICATION", "GOAL_FINISH", "REQUEST_ASK"];

/// The tools the model gets: those OAK runs, with each family's as one
/// tool. `local`: a model on the user's own llama.cpp server
/// (`LOCAL_TOOLS`); `goal`: the session has a goal.
/// `agent`: a subagent; `spawner`: a sub-orchestrator (AGENT_NEW `can_spawn`).
/// Every kit's tools included.
#[cfg(test)]
pub fn definitions_as(agent: bool, spawner: bool, local: bool, goal: bool) -> Value {
    Value::Array(defs::definitions_as(agent, spawner, local, goal))
}

/// The most files one DISPLAY call shows.
const DISPLAY_MAX: usize = 10;
/// The start of a text file that DISPLAY shows: this many lines at most...
const DISPLAY_LINES: usize = 40;
/// ...and this many characters.
const DISPLAY_CHARS: usize = 8_000;

/// DISPLAY: the files as the chat shows them (kept with the result, under
/// `_oak_display`, never sent), and the confirmation for the model. Each
/// is `{path, name, bytes}` plus `image` (an image block) for an image or
/// `text` and `lines` for a text file. Files that can't be read are named
/// in the confirmation; with none readable, it fails.
pub fn display(folder: &Path, input: &Value) -> Result<(String, Vec<Value>), String> {
    let paths: Vec<&str> = input["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).filter(|p| !p.trim().is_empty()).collect();
    if paths.is_empty() || paths.len() > DISPLAY_MAX {
        return Err(format!("Give 1 to {DISPLAY_MAX} paths."));
    }
    let (mut shown, mut said, mut failed) = (vec![], vec![], vec![]);
    for p in paths {
        let path = resolve(folder, p.trim());
        let bytes = match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => Err("it is a folder".to_string()),
            _ => std::fs::read(&path).map_err(|e| e.to_string()),
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        let mut file = json!({"path": path.display().to_string(), "name": name, "bytes": bytes.len()});
        let image = (crate::images::is_image_path(&path) || image::guess_format(&bytes).is_ok()).then(|| crate::images::from_bytes(&bytes).ok()).flatten();
        if let Some(img) = image {
            said.push(format!("{} (image, {}x{})", path.display(), img.width, img.height));
            file["image"] = img.block();
        } else if let Some(text) = decode_text(&bytes) {
            let lines = text.lines().count();
            let mut preview: String = text.lines().take(DISPLAY_LINES).collect::<Vec<_>>().join("\n");
            if preview.len() > DISPLAY_CHARS {
                preview.truncate(preview.floor_char_boundary(DISPLAY_CHARS));
            }
            said.push(format!("{} (text, {lines} lines; the user sees the start)", path.display()));
            file["text"] = json!(preview);
            file["lines"] = json!(lines);
        } else {
            said.push(format!("{} (a link that opens it)", path.display()));
        }
        shown.push(file);
    }
    if shown.is_empty() {
        return Err(format!("Nothing shown: {}", failed.join("; ")));
    }
    let mut out = format!("Shown to the user in the chat: {}.", said.join(", "));
    if !failed.is_empty() {
        out += &format!(" Not shown (couldn't read them): {}.", failed.join("; "));
    }
    Ok((out, shown))
}

/// Checks a tool input (which streamed in unvalidated) against its schema.
/// A family's call (`TASK` with an action) is checked as the action's
/// tool (`canonical`).
pub fn validate(name: &str, input: &Value) -> Result<(), String> {
    let (name, input) = canonical(name, input)?;
    let (name, input) = (name.as_str(), &input);
    let defs = defs::run_definitions(false, false, false, false);
    let def = defs.iter().find(|d| d["name"] == name).ok_or(format!("Unknown tool {name}"))?;
    let schema = &def["input_schema"];
    let obj = input.as_object().ok_or("Input must be an object")?;
    for r in schema["required"].as_array().unwrap() {
        if !obj.contains_key(r.as_str().unwrap()) {
            return Err(format!("Missing required field `{}`", r.as_str().unwrap()));
        }
    }
    for (k, v) in obj {
        let p = &schema["properties"][k];
        let ty = p["type"].as_str().unwrap_or("");
        let ok = match ty {
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "boolean" => v.is_boolean(),
            "array" => v.as_array().is_some_and(|a| match p["items"]["type"].as_str() {
                Some("object") => a.iter().all(|x| x.is_object()),
                _ => a.iter().all(|x| x.is_string()),
            }),
            _ => true,
        };
        if !ok {
            return Err(format!("Field `{k}` must be a {ty}"));
        }
    }
    Ok(())
}

/// Names other agents' file tools give the fields of OAK's: (tool, OAK's
/// name, others).
const ALIASES: &[(&str, &str, &[&str])] = &[
    ("READ", "path", &["file_path", "filePath"]),
    ("WRITE", "path", &["file_path", "filePath"]),
    ("WRITE", "content", &["file_text"]),
    ("EDIT", "path", &["file_path", "filePath"]),
    ("EDIT", "old_string", &["old_str", "oldText", "old_text"]),
    ("EDIT", "new_string", &["new_str", "newText", "new_text"]),
    ("EDIT", "replace_all", &["replaceAll"]),
];

/// Fixes the slips models make in a call's input, as it arrives (before
/// it is shown, ordered by `waves` and checked by `validate`): field names
/// from other agents' file tools (`file_path`, `old_str`, `oldText`, …),
/// and values of the wrong shape that are unambiguous: an array sent as
/// JSON text or as its lone item, "true"/"false" for a boolean, digits in
/// a string for an integer.
pub fn repair(name: &str, mut input: Value) -> Value {
    let defs = defs::run_definitions(false, false, false, false);
    // A family's call is repaired as its action's tool (its `action` kept).
    let name = canonical(name, &input).map(|(n, _)| n).unwrap_or_else(|_| name.to_string());
    let name = name.as_str();
    let Some(def) = defs.iter().find(|d| d["name"] == name) else {
        return input;
    };
    let props = &def["input_schema"]["properties"];
    if let Some(obj) = input.as_object_mut() {
        for (tool, field, others) in ALIASES {
            if *tool == name
                && !obj.contains_key(*field)
                && let Some(v) = others.iter().find_map(|k| obj.remove(*k))
            {
                obj.insert(field.to_string(), v);
            }
        }
        for (k, v) in obj.iter_mut() {
            let p = &props[k.as_str()];
            let objects = p["items"]["type"] == "object";
            let fixed = match (p["type"].as_str(), &*v) {
                (Some("array"), Value::String(s)) => match serde_json::from_str::<Value>(s) {
                    Ok(a @ Value::Array(_)) => Some(a),
                    Ok(o @ Value::Object(_)) if objects => Some(json!([o])),
                    _ if !objects => Some(json!([s])),
                    _ => None,
                },
                (Some("array"), Value::Object(_)) if objects => Some(json!([v])),
                (Some("boolean"), Value::String(s)) => match s.trim() {
                    "true" => Some(json!(true)),
                    "false" => Some(json!(false)),
                    _ => None,
                },
                (Some("integer"), Value::String(s)) => s.trim().parse::<i64>().ok().map(|n| json!(n)),
                _ => None,
            };
            if let Some(f) = fixed {
                *v = f;
            }
        }
    }
    input
}

/// A path, resolved against the working folder and with native separators
/// throughout, so messages don't show a mix like `C:\\oak\\src/main.rs`.
pub fn resolve(folder: &Path, p: &str) -> PathBuf {
    let p = PathBuf::from(native_seps(p));
    if p.is_absolute() { p } else { folder.join(p) }
}

/// `p` with Windows separators on Windows; as it is elsewhere, where a
/// backslash can be part of a name.
pub fn native_seps(p: &str) -> String {
    if cfg!(windows) { p.replace('/', "\\") } else { p.to_string() }
}

/// A path as a key for telling files apart: Windows paths ignore case,
/// Linux ones don't.
pub fn path_key(p: &Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// How a text file is stored on disk.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    /// UTF-16 files are recognised by their byte order mark.
    fn sniff(bytes: &[u8]) -> Encoding {
        match bytes {
            [0xFF, 0xFE, ..] => Encoding::Utf16Le,
            [0xFE, 0xFF, ..] => Encoding::Utf16Be,
            _ => Encoding::Utf8,
        }
    }

    fn units(self, bytes: &[u8]) -> Vec<u16> {
        let be = self == Encoding::Utf16Be;
        bytes[2..].chunks_exact(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect()
    }

    /// Strict decoding, for files that will be written back.
    fn decode(self, bytes: &[u8]) -> Result<String, &'static str> {
        match self {
            Encoding::Utf8 => String::from_utf8(bytes.to_vec()).map_err(|_| "the file is not valid UTF-8"),
            _ => String::from_utf16(&self.units(bytes)).map_err(|_| "the file is not valid UTF-16"),
        }
    }

    /// The bytes to store, with the byte order mark for UTF-16.
    fn encode(self, text: &str) -> Vec<u8> {
        match self {
            Encoding::Utf8 => text.as_bytes().to_vec(),
            Encoding::Utf16Le => [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
            Encoding::Utf16Be => [0xFE, 0xFF].into_iter().chain(text.encode_utf16().flat_map(u16::to_be_bytes)).collect(),
        }
    }
}

/// Decodes file bytes for display: UTF-16 with a BOM is converted, anything
/// else goes through lossy UTF-8. None means the file looks binary.
fn decode_text(bytes: &[u8]) -> Option<String> {
    match Encoding::sniff(bytes) {
        Encoding::Utf8 if bytes[..bytes.len().min(8192)].contains(&0) => None,
        Encoding::Utf8 => Some(String::from_utf8_lossy(bytes).into_owned()),
        enc => Some(String::from_utf16_lossy(&enc.units(bytes))),
    }
}

/// `s` cut to its first `max` characters, with "…" when it was longer.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.head(max)) }
}

/// A command's output longer than this many characters is saved to a file
/// and replaced by its path and a preview (Claude Code's threshold).
pub const PERSIST_CHARS: usize = 50_000;

/// How much of a saved result the preview shows, in bytes.
const PREVIEW_BYTES: usize = 2_000;

/// True when `out` is too long to send as it is (see `persist`).
pub fn too_large(out: &str) -> bool {
    out.len() > PERSIST_CHARS && out.chars().count() > PERSIST_CHARS
}

/// Saves `out` as `dir/tool_results/N.txt`, N the first number from 1 with no
/// such file (it never overwrites one), and returns what the model gets
/// instead: the file's path and size and the first PREVIEW_BYTES of it (cut
/// at a line end when one is in its second half), as Claude Code words it.
/// If it can't be saved, its head and tail.
pub fn persist(out: &str, dir: &Path) -> String {
    let Some(path) = save_new(dir, out.as_bytes()) else {
        return clip(out);
    };
    let mut preview = &out[..out.floor_char_boundary(PREVIEW_BYTES)];
    if let Some(nl) = preview.rfind('\n').filter(|&nl| nl > PREVIEW_BYTES / 2) {
        preview = &preview[..nl];
    }
    let more = if preview.len() < out.len() { "\n..." } else { "" };
    format!(
        "<persisted-output>\nOutput too large ({}). Full output saved to: {}\n\nPreview (first 2KB):\n{preview}{more}\n</persisted-output>",
        file_size(out.len()),
        path.display()
    )
}

/// Writes `bytes` to the first `dir/tool_results/N.txt` (N from 1) that
/// doesn't exist yet. Each name is claimed with create_new, so two calls at
/// once get different files and none is overwritten.
fn save_new(dir: &Path, bytes: &[u8]) -> Option<PathBuf> {
    use std::io::Write;
    let dir = dir.join("tool_results");
    std::fs::create_dir_all(&dir).ok()?;
    for n in 1u64.. {
        let path = dir.join(format!("{n}.txt"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => return f.write_all(bytes).ok().map(|_| path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// A size as Claude Code shows it: "512 bytes", "108.5KB", "2.3MB".
fn file_size(bytes: usize) -> String {
    let one = |x: f64, unit: &str| format!("{}{unit}", format!("{x:.1}").trim_end_matches(".0"));
    match bytes {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1024 * 1024 => one(b as f64 / 1024.0, "KB"),
        b => one(b as f64 / (1024.0 * 1024.0), "MB"),
    }
}

/// Keeps the head and tail of long output.
pub fn clip(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let head = &s[..s.floor_char_boundary(MAX_OUTPUT / 2)];
    let tail = &s[s.ceil_char_boundary(s.len() - MAX_OUTPUT / 2)..];
    format!("{head}\n\n[… {} characters omitted …]\n\n{tail}", s.len() - head.len() - tail.len())
}

pub fn read(folder: &Path, input: &Value) -> Result<String, String> {
    let path = resolve(folder, input["path"].as_str().unwrap());
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = decode_text(&bytes)
        .ok_or_else(|| format!("{} looks like a binary file ({} bytes), so it isn't shown. Inspect it with EXECUTE (e.g. `xxd`, `file`).", path.display(), bytes.len()))?;
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok("(empty file)".into());
    }
    if offset > lines.len() {
        return Err(format!("Offset {offset} is past the end of the file, which has {} lines.", lines.len()));
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
        let line = if line.len() > 2000 { &line[..line.floor_char_boundary(2000)] } else { line };
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    let shown_end = (offset - 1 + limit).min(lines.len());
    if shown_end < lines.len() {
        out.push_str(&format!("\n[{} more lines; use offset {} to continue]", lines.len() - shown_end, shown_end + 1));
    }
    Ok(clip(&out))
}

/// The file a path names, however it is named (through a symlink, `..`,
/// or a folder that is a link), even when it doesn't exist yet.
fn real_path(path: &Path) -> PathBuf {
    let (mut p, mut rest) = (path, vec![]);
    loop {
        if let Ok(real) = std::fs::canonicalize(p) {
            return rest.iter().rev().fold(real, |acc, name| acc.join(name));
        }
        match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                p = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// A lock per file, held by WRITE and EDIT from reading the file to
/// writing it, so that two agents (or an agent and a subagent) changing
/// one file at once can't interleave and lose one change. The calls of a
/// single reply are already put in order by `waves`.
fn file_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = LazyLock::new(Default::default);
    let key = path_key(&real_path(path));
    let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, l| l.strong_count() > 0);
    if let Some(l) = locks.get(&key).and_then(Weak::upgrade) {
        return l;
    }
    let l = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&l));
    l
}

pub fn write(folder: &Path, input: &Value) -> Result<String, String> {
    let path = resolve(folder, input["path"].as_str().unwrap());
    let lock = file_lock(&path);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = input["content"].as_str().unwrap();
    // Overwriting a UTF-16 file keeps it UTF-16.
    let enc = std::fs::read(&path).map(|b| Encoding::sniff(&b)).unwrap_or(Encoding::Utf8);
    std::fs::write(&path, enc.encode(content)).map_err(|e| format!("{}: {e}", path.display()))?;
    let note = if enc == Encoding::Utf8 { "" } else { ", kept as UTF-16" };
    Ok(format!("Wrote {} ({} lines{note})", path.display(), content.lines().count()))
}

pub fn edit(folder: &Path, input: &Value) -> Result<String, String> {
    let path = resolve(folder, input["path"].as_str().unwrap());
    let lock = file_lock(&path);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let enc = Encoding::sniff(&bytes);
    let text = enc.decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let (old, new) = (input["old_string"].as_str().unwrap(), input["new_string"].as_str().unwrap());
    if old.is_empty() {
        return Err("old_string is empty".into());
    }
    if old == new {
        return Err("old_string and new_string are the same".into());
    }
    let all = input["replace_all"].as_bool().unwrap_or(false);
    let take = if all { usize::MAX } else { 1 };
    // Files with CRLF endings: match the model's LF text against them too.
    let crlf = !text.contains(old) && text.contains("\r\n") && text.contains(&old.replace('\n', "\r\n"));
    let (exact_old, exact_new) = if crlf { (old.replace('\n', "\r\n"), new.replace('\n', "\r\n")) } else { (old.to_string(), new.to_string()) };
    let count = text.matches(&exact_old).count();
    let (spans, new, loose) = if count > 0 {
        if count > 1 && !all {
            return Err(format!("old_string appears {count} times; add context to make it unique or set replace_all"));
        }
        let spans: Vec<(usize, usize)> = text.match_indices(&exact_old).take(take).map(|(i, m)| (i, i + m.len())).collect();
        (spans, exact_new, false)
    } else {
        // Not there as it is: try again ignoring trailing whitespace and
        // reading typographic quotes, dashes and spaces as ASCII ones.
        let spans = loose_find(&text, old);
        match spans.len() {
            0 => return Err("old_string not found in the file".into()),
            n if n > 1 && !all => {
                return Err(format!(
                    "old_string isn't in the file exactly, and appears {n} times when trailing whitespace is ignored and curly quotes, Unicode dashes and special spaces are read as ASCII; add context to make it unique or set replace_all"
                ));
            }
            _ => {}
        }
        let lf = new.replace("\r\n", "\n");
        let new = if text.contains("\r\n") { lf.replace('\n', "\r\n") } else { lf };
        (spans.into_iter().take(take).collect(), new, true)
    };
    let out = splice(&text, &spans, &new);
    if out == text {
        return Err("The edit leaves the file unchanged.".into());
    }
    std::fs::write(&path, enc.encode(&out)).map_err(|e| e.to_string())?;
    let n = spans.len();
    let mut msg = format!("Edited {} ({n} replacement{}; {})", path.display(), crate::plural(n), line_ranges(&edited_lines(&text, &spans, &new)));
    if loose {
        msg.push_str(
            "\nold_string matched only once trailing whitespace was ignored and curly quotes, Unicode dashes and special spaces were read as ASCII ones. \
             Only the matched text was replaced; the rest of the file is as it was.",
        );
    }
    Ok(msg)
}

/// `text` with each span (start, end, in order, apart) replaced by `new`.
fn splice(text: &str, spans: &[(usize, usize)], new: &str) -> String {
    let (mut out, mut last) = (String::with_capacity(text.len()), 0);
    for &(a, b) in spans {
        out.push_str(&text[last..a]);
        out.push_str(new);
        last = b;
    }
    out.push_str(&text[last..]);
    out
}

/// How the loose match reads a character: typographic quotes, dashes and
/// spaces as their ASCII counterparts.
fn loosen(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}'..='\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

/// `text` read loosely (each line without its trailing whitespace, CRLF
/// as LF, characters through `loosen`), and for each character of that
/// where it came from in `text`: (start in the loose text, start and end
/// in `text`). A newline stands for the whitespace that ended its line
/// and the line ending.
fn loose(text: &str) -> (String, Vec<(usize, usize, usize)>) {
    let (mut out, mut map, mut at) = (String::new(), vec![], 0);
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let kept = body.trim_end();
        for (i, c) in kept.char_indices() {
            map.push((out.len(), at + i, at + i + c.len_utf8()));
            out.push(loosen(c));
        }
        if body.len() < line.len() {
            map.push((out.len(), at + kept.len(), at + line.len()));
            out.push('\n');
        }
        at += line.len();
    }
    (out, map)
}

/// Where `old` is in `text` when both are read loosely: spans of `text`,
/// in order. Each covers just what matched, so the rest of the lines it
/// touches stays as it is.
fn loose_find(text: &str, old: &str) -> Vec<(usize, usize)> {
    let (hay, map) = loose(text);
    let needle = loose(old).0;
    if needle.is_empty() {
        return vec![];
    }
    hay.match_indices(&needle)
        .map(|(s, m)| {
            let first = map.partition_point(|c| c.0 < s);
            let last = map.partition_point(|c| c.0 < s + m.len()) - 1;
            (map[first].1, map[last].2)
        })
        .collect()
}

/// Where replacing the spans of `text` with `new` puts each in the edited
/// file: (first line, last line), from 1.
fn edited_lines(text: &str, spans: &[(usize, usize)], new: &str) -> Vec<(usize, usize)> {
    // Lines the new text spans (a deletion still marks the line it was on).
    let span = new.trim_end_matches('\n').matches('\n').count();
    let (mut out, mut line, mut last) = (vec![], 1, 0);
    for &(a, b) in spans {
        // Lines before this match in the edited file: those before it in
        // the original, plus what earlier replacements added or removed.
        line += text[last..a].matches('\n').count();
        out.push((line, line + span));
        line += new.matches('\n').count();
        last = b;
    }
    out
}

/// "line 5", "lines 5–9", or several, at most five ("…" after).
fn line_ranges(r: &[(usize, usize)]) -> String {
    let one = |&(a, b): &(usize, usize)| if a == b { a.to_string() } else { format!("{a}–{b}") };
    let mut parts: Vec<String> = r.iter().take(5).map(one).collect();
    if r.len() > 5 {
        parts.push("…".into());
    }
    let single = r.len() == 1 && r[0].0 == r[0].1;
    format!("{} {}", if single { "line" } else { "lines" }, parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call's tool name without its input copied is `canonical`'s.
    #[test]
    fn canonical_name_matches() {
        let calls = [
            ("TASK", json!({"action": "wait", "id": 1})),
            ("TASK", json!({"action": " Wait "})),
            ("TASK", json!({"action": "nope"})),
            ("TASK", json!({})),
            ("AGENT", json!({"action": "new"})),
            ("READ", json!({"action": "wait"})),
            ("EXECUTE", json!({"command": "ls"})),
        ];
        for (name, input) in &calls {
            assert_eq!(canonical_name(name, input), canonical_cow(name, input).0, "{name} {input}");
        }
    }

    /// A kit's tools are there once it is loaded; LOAD offers the others.
    #[test]
    fn kits_load_their_tools() {
        let names = |v: Vec<Value>| v.iter().map(|d| d["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let kits = |k: &[&str]| k.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let main = names(definitions_with_kits(false, false, false, &[]));
        assert!(main.contains(&"LOAD".into()) && !main.contains(&"AGENT".into()) && !main.contains(&"BROWSER".into()) && main.contains(&"GOAL".into()));
        let loaded = definitions_with_kits(false, false, false, &kits(&["AGENT", "BROWSER"]));
        assert!(names(loaded.clone()).contains(&"AGENT".into()) && names(loaded.clone()).contains(&"BROWSER".into()));
        let browser = loaded.iter().find(|d| d["name"] == "BROWSER").unwrap();
        assert!(browser["input_schema"]["properties"]["url"].is_object(), "the whole family once loaded");
        // A subagent has AGENT, and only WINDOW to load.
        let sub = definitions_with_kits(true, false, false, &[]);
        let load = sub.iter().find(|d| d["name"] == "LOAD").unwrap();
        assert!(names(sub.clone()).contains(&"AGENT".into()));
        assert_eq!(load["input_schema"]["properties"]["name"]["enum"], json!(["WINDOW"]));
        // A local model: no LOAD; GOAL with a goal.
        assert!(!names(definitions_with_kits(false, false, true, &[])).iter().any(|n| n == "LOAD" || n == "GOAL"));
        assert!(names(definitions_with_kits(false, false, true, &kits(&["GOAL"]))).contains(&"GOAL".into()));
        assert_eq!((kit_of("BROWSER_GOTO", false), kit_of("AGENT_NEW", false), kit_of("AGENT_SEND", true), kit_of("READ", false)), (Some("BROWSER"), Some("AGENT"), None, None));
        assert!(kit_help("AGENT", false).contains("- action: new|send"));
    }

    /// A sub-orchestrator manages agents of its own, but still can't ask
    /// the user or set progress meters; a plain subagent can do neither.
    #[test]
    fn sub_orchestrators_manage_their_own_agents_only() {
        assert!(allowed("AGENT_NEW", false, false));
        assert!(!allowed("AGENT_NEW", true, false));
        assert!(allowed("AGENT_NEW", true, true) && allowed("AGENT_DELETE", true, true));
        assert!(!allowed("ASK", true, true) && !allowed("PROGRESS_SET", true, true));
        let names = |v: Value| v.as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let sub = names(definitions_as(true, true, false, false));
        let agent = definitions_as(true, true, false, false).as_array().unwrap().iter().find(|d| d["name"] == "AGENT").cloned().unwrap();
        assert!(!sub.contains(&"ASK".to_string()));
        assert!(agent["input_schema"]["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("new")));
    }

    /// Long output is saved whole, and the model gets its path, its size and
    /// a preview cut at a line end; short output isn't.
    #[test]
    fn long_output_is_saved_with_a_preview() {
        let out: String = (0..2100).map(|i| format!("line {i:05} of the output\n")).collect();
        assert!(too_large(&out) && !too_large(&out[..PERSIST_CHARS]));
        assert!(!too_large(&"é".repeat(PERSIST_CHARS)), "counts characters, not bytes");
        let dir = std::env::temp_dir().join(format!("oak-persist-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let results = dir.join("tool_results");
        std::fs::create_dir_all(&results).unwrap();
        std::fs::write(results.join("2.txt"), "taken").unwrap();
        let shown = persist(&out, &dir);
        let path = results.join("1.txt");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), out);
        // The next skips the number that is taken and overwrites nothing.
        persist(&out, &dir);
        assert_eq!(std::fs::read_to_string(results.join("2.txt")).unwrap(), "taken");
        assert_eq!(std::fs::read_to_string(results.join("3.txt")).unwrap(), out);
        assert!(shown.starts_with(&format!("<persisted-output>\nOutput too large (51.3KB). Full output saved to: {}\n\nPreview (first 2KB):\nline 00000", path.display())), "{shown}");
        assert!(shown.ends_with("of the output\n...\n</persisted-output>"), "{shown}");
        assert!(shown.len() < PREVIEW_BYTES + 300);
        assert_eq!((file_size(512), file_size(2048), file_size(3 * 1024 * 1024)), ("512 bytes".into(), "2KB".into(), "3MB".into()));
    }

    fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join("oak-tools-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn utf16le(s: &str) -> Vec<u8> {
        [0xFF, 0xFE].into_iter().chain(s.encode_utf16().flat_map(u16::to_le_bytes)).collect()
    }

    #[test]
    fn edit_keeps_utf16_and_crlf() {
        let p = scratch("edit16.txt", &utf16le("one\r\ntwo\r\nthree\r\n"));
        let input = json!({"path": p.to_str().unwrap(), "old_string": "one\ntwo", "new_string": "ONE\ntwö"});
        edit(Path::new("C:\\"), &input).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), utf16le("ONE\r\ntwö\r\nthree\r\n"));
    }

    /// Runs an EDIT on a scratch file holding `text`: the result, and the
    /// file after it.
    fn edit_text(name: &str, text: &str, old: &str, new: &str, all: bool) -> (Result<String, String>, String) {
        let p = scratch(name, text.as_bytes());
        let input = json!({"path": p.to_str().unwrap(), "old_string": old, "new_string": new, "replace_all": all});
        let r = edit(Path::new("C:\\"), &input);
        (r, std::fs::read_to_string(&p).unwrap())
    }

    #[test]
    fn edit_matches_loosely() {
        // Trailing whitespace the model left out.
        let (r, out) = edit_text("loose1.txt", "fn a() {   \n    x = 1;\t\n}\n", "fn a() {\n    x = 1;", "fn a() {\n    x = 2;", false);
        assert!(r.as_ref().unwrap().contains("(1 replacement; lines 1–2)\nold_string matched only once"), "{r:?}");
        assert_eq!(out, "fn a() {\n    x = 2;\t\n}\n");
        // Typographic quotes and dashes: only the matched text changes, the
        // rest of its line (and file) keeps its characters.
        let (r, out) = edit_text("loose2.txt", "say(“hi”) // it’s – fine\nnext\u{a0}\n", "say(\"hi\")", "say(\"bye\")", false);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(out, "say(\"bye\") // it’s – fine\nnext\u{a0}\n");
        let (_, out) = edit_text("loose3.txt", "a = b \u{2212} c\u{2002}d\n", "b - c d", "b + c", false);
        assert_eq!(out, "a = b + c\n");
        // CRLF files: the replacement gets CRLF too, other lines are kept.
        let (r, out) = edit_text("loose4.txt", "a  \r\nb\r\nc \r\n", "a\nb\n", "A\nB\n", false);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(out, "A\r\nB\r\nc \r\n");
        // Unique only when read exactly is still unique; loosely twice isn't.
        let (r, _) = edit_text("loose5.txt", "f(‘a’)\nf(’a’)\n", "f('a')", "g()", false);
        assert!(r.unwrap_err().contains("appears 2 times when trailing whitespace is ignored"));
        let (r, out) = edit_text("loose6.txt", "f(‘a’)\nf(’a’)\n", "f('a')", "g()", true);
        assert!(r.unwrap().contains("(2 replacements; lines 1, 2)"));
        assert_eq!(out, "g()\ng()\n");
        // Exact matches don't go near it, and missing text is still missing.
        let (r, out) = edit_text("loose7.txt", "x  \ny\n", "x  ", "z", false);
        assert!(!r.unwrap().contains("matched only once"));
        assert_eq!(out, "z\ny\n");
        let (r, _) = edit_text("loose8.txt", "abc\n", "abd", "x", false);
        assert_eq!(r.unwrap_err(), "old_string not found in the file");
        // A loose match that changes nothing is refused, not written.
        let (r, out) = edit_text("loose9.txt", "“q”\n", "\"q\"", "“q”", false);
        assert!(r.unwrap_err().contains("unchanged"));
        assert_eq!(out, "“q”\n");
    }

    /// Agents editing one file at once each keep their change.
    #[test]
    fn edits_of_one_file_take_turns() {
        let dir = std::env::temp_dir().join(format!("oak-edit-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "END\n".to_string() + &"filler line\n".repeat(20_000)).unwrap();
        // Named two ways, as one file.
        let names = ["f.txt", "./f.txt"];
        std::thread::scope(|s| {
            for i in 0..16 {
                let dir = &dir;
                s.spawn(move || {
                    let input = json!({"path": names[i % 2], "old_string": "END\n", "new_string": format!("line {i}\nEND\n")});
                    edit(dir, &input).unwrap();
                });
            }
        });
        let out = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        for i in 0..16 {
            assert!(out.contains(&format!("line {i}\n")), "line {i} lost");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

