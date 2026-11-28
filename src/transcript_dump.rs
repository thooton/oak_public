//! `oak --transcript`: a conversation file as readable text, for whoever
//! debugs Oak (an agent, mostly). A transcript on disk is not what the model
//! is sent: a summary replaces its start, old tool output is cleared, other
//! providers' reasoning is left out, the context block goes at the start of
//! the first message, the prefix's system messages are put in, and the
//! transcript's lines (marks) are the user's only. So there are two views:
//! the transcript as stored (every message and line, with times), and with
//! `--sent` what the latest request sent, built by the agent's own code
//! (`agent::sent_messages`) from the saved state.
//!
//! The file is only read: nothing is written back (pictures stay where they
//! are, an older format is upgraded in memory only), so it is safe on the
//! conversations of an Oak that is running.

use crate::store::{self, Conversation};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
oak --transcript                     the newest conversations (--all: every one): id, size, title
oak --transcript <id|file|text> [options]
                                     a conversation as stored, readable: every message and
                                     transcript line, with times (<text>: a unique part of
                                     its file name)
  --sent       what the latest request sent the model instead: the system prompt and session
               memory, the tools, and the messages as the agent builds them (summary in place
               of the start, cleared tool output, context block, prefix messages), in the Oak
               format, before the provider's wire format
  --tools      every tool's whole definition, not only its name
  --json       JSON instead of text: the stored messages, or with --sent {system, tools, messages}
  --from <n>   only messages from transcript index n (with --sent: from sent index n)
  --to <n>     only messages before index n
  --full       blocks in full (otherwise each is cut at 3000 characters)
";

/// The default cut of a block's text.
const SHORT: usize = 3000;

struct Opts {
    target: Option<String>,
    sent: bool,
    tools: bool,
    json: bool,
    full: bool,
    all: bool,
    from: usize,
    to: usize,
}

/// Runs `oak --transcript <args>`; the exit code.
pub fn main(args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) if e == "usage" => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("oak --transcript: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let out = match &opts.target {
        None => list(opts.all),
        Some(t) => find(t).and_then(|path| {
            let c = load(&path)?;
            Ok(if opts.sent { sent(&c, &opts) } else { stored(&c, &path, &opts) })
        }),
    };
    match out {
        Ok(text) => {
            use std::io::Write;
            // (A closed pipe, `| head`, isn't an error.)
            let _ = std::io::stdout().lock().write_all(text.as_bytes());
            0
        }
        Err(e) => {
            eprintln!("oak --transcript: {e}");
            1
        }
    }
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { target: None, sent: false, tools: false, json: false, full: false, all: false, from: 0, to: usize::MAX };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut number = |name: &str| -> Result<usize, String> {
            it.next().and_then(|v| v.parse().ok()).ok_or_else(|| format!("{name} takes a number"))
        };
        match a.as_str() {
            "--sent" => o.sent = true,
            "--tools" => o.tools = true,
            "--json" => o.json = true,
            "--full" => o.full = true,
            "--all" => o.all = true,
            "--from" => o.from = number("--from")?,
            "--to" => o.to = number("--to")?,
            "-h" | "--help" => return Err("usage".into()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s if o.target.is_none() => o.target = Some(s.to_string()),
            s => return Err(format!("one conversation at a time ({s}?)")),
        }
    }
    Ok(o)
}

/// What a listing reads of a file: not its messages.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Head {
    id: String,
    title: String,
    model: String,
    parent: Option<String>,
}

fn files() -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(store::conversations_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((p.clone(), std::fs::metadata(&p).ok()?.modified().ok()?)))
        .collect();
    v.sort_by_key(|b| std::cmp::Reverse(b.1));
    v
}

fn list(all: bool) -> Result<String, String> {
    let files = files();
    let shown = if all { files.len() } else { files.len().min(30) };
    let mut out = format!("{} conversations in {} (newest first)\n\n", files.len(), store::conversations_dir().display());
    for (path, modified) in &files[..shown] {
        let Ok(buf) = std::fs::read(path) else { continue };
        let h: Head = serde_json::from_slice(&buf).unwrap_or_default();
        let at = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let sub = if h.parent.is_some() { " (subagent)" } else { "" };
        let _ = writeln!(out, "{}  {:>8}  {}  {}{sub}  [{}]", when(at), crate::look::size_text(buf.len() as u64), h.id, h.title, h.model);
    }
    if shown < files.len() {
        let _ = writeln!(out, "\n... {} more (--all)", files.len() - shown);
    }
    Ok(out)
}

/// The file `t` names: a path, a conversation's id, or a unique part of a
/// file name.
fn find(t: &str) -> Result<PathBuf, String> {
    let p = Path::new(t);
    if p.is_file() {
        return Ok(p.to_path_buf());
    }
    let files = files();
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let by_id: Vec<_> = files
        .iter()
        .filter(|(p, _)| {
            let n = name(p);
            n == format!("{t}.json") || n.ends_with(&format!(" - {t}.json"))
        })
        .collect();
    if let Some((p, _)) = by_id.first() {
        return Ok(p.clone());
    }
    let low = t.to_lowercase();
    let by_name: Vec<_> = files.iter().filter(|(p, _)| name(p).to_lowercase().contains(&low)).collect();
    match by_name.as_slice() {
        [(p, _)] => Ok(p.clone()),
        [] => Err(format!("no conversation file, id or file name matching {t:?} in {}", store::conversations_dir().display())),
        many => {
            let names: Vec<String> = many.iter().take(10).map(|(p, _)| format!("  {}", name(p))).collect();
            Err(format!("{} files match {t:?}; say which (its id):\n{}", many.len(), names.join("\n")))
        }
    }
}

/// The conversation in `path`, read whole and never written back.
fn load(path: &Path) -> Result<Conversation, String> {
    let buf = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut c: Conversation = serde_json::from_slice(&buf).map_err(|e| format!("{}: not a conversation: {e}", path.display()))?;
    c.upgrade();
    Ok(c)
}

fn when(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .filter(|_| ms > 0)
        .map_or_else(|| "-".into(), |t| t.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// `s`, cut at `SHORT` characters unless `full`.
fn cut(s: &str, full: bool) -> String {
    // (Control characters shown, not written: a NUL makes the output read
    // as binary to grep and pagers.)
    let s = &if s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        s.chars()
            .map(|c| if c.is_control() && c != '\n' && c != '\t' { format!("\\u{{{:x}}}", c as u32) } else { c.to_string() })
            .collect()
    } else {
        s.to_string()
    };
    if full {
        return s.to_string();
    }
    match s.char_indices().nth(SHORT) {
        Some((i, _)) => format!("{}\n[... {} more characters; --full shows them]", &s[..i], s[i..].chars().count()),
        None => s.to_string(),
    }
}

fn indent(s: &str, by: &str) -> String {
    s.lines().map(|l| format!("{by}{l}")).collect::<Vec<_>>().join("\n")
}

/// A tool call's input: each key on a line of its own, a string as it is
/// (so commands and file contents read as they are, not escaped).
fn input_text(input: &Value, full: bool) -> String {
    let Some(o) = input.as_object() else { return cut(&input.to_string(), full) };
    let mut out = String::new();
    for (k, v) in o {
        match v {
            Value::String(s) if s.contains('\n') => {
                let _ = writeln!(out, "{k}:\n{}", indent(&cut(s, full), "  | "));
            }
            Value::String(s) => {
                let _ = writeln!(out, "{k}: {}", cut(s, full));
            }
            v => {
                let _ = writeln!(out, "{k}: {}", cut(&v.to_string(), full));
            }
        }
    }
    out.trim_end().to_string()
}

/// Oak's own keys on a block worth showing (`_oak_`: never sent).
fn oak_keys(b: &Value) -> String {
    let Some(o) = b.as_object() else { return String::new() };
    let keys: Vec<String> = o
        .iter()
        .filter(|(k, _)| k.starts_with("_oak_") && !matches!(k.as_str(), "_oak_at"))
        .map(|(k, v)| format!("{}={}", &k[5..], match v { Value::String(s) => cut(s, false), v => v.to_string() }))
        .collect();
    if keys.is_empty() { String::new() } else { format!(" {{{}}}", keys.join(", ")) }
}

fn block_text(b: &Value, full: bool) -> String {
    let s = |k: &str| b[k].as_str().unwrap_or("");
    let extra = oak_keys(b);
    match s("type") {
        "text" => format!("[text]{extra}\n{}", indent(&cut(s("text"), full), "  ")),
        "reasoning" => {
            let by = b["by"].as_str().map_or(String::new(), |by| format!(" by {by}"));
            let sig = if b["sig"].is_string() { ", signed" } else { "" };
            let red = if b["redacted"] == true { ", redacted" } else { "" };
            format!("[reasoning{by}{sig}{red}]{extra}\n{}", indent(&cut(s("text"), full), "  "))
        }
        "tool_call" => format!("[call {} id={}]{extra}\n{}", s("name"), s("id"), indent(&input_text(&b["input"], full), "  ")),
        "tool_result" => {
            let err = if b["is_error"] == true { " ERROR" } else { "" };
            let content = match &b["content"] {
                Value::String(t) => cut(t, full),
                Value::Array(blocks) => blocks.iter().map(|x| block_text(x, full)).collect::<Vec<_>>().join("\n"),
                v => cut(&v.to_string(), full),
            };
            format!("[result id={}{err}]{extra}\n{}", s("call_id"), indent(&content, "  "))
        }
        "image" => {
            let size = b["data"].as_str().map_or(String::new(), |d| format!(", {} base64 characters", d.len()));
            let blob = b["blob"].as_str().map_or(String::new(), |f| format!(", blob {f}"));
            format!("[image {}{blob}{size}]{extra}", s("media_type"))
        }
        "document" => {
            let body = if s("media_type") == "text/plain" { format!("\n{}", indent(&cut(s("data"), full), "  ")) } else { String::new() };
            format!("[document {:?} {}]{extra}{body}", s("title"), s("media_type"))
        }
        _ => format!("[{}]\n{}", s("type"), indent(&cut(&b.to_string(), full), "  ")),
    }
}

/// When a message came in: its first block's time.
fn message_time(m: &Value) -> i64 {
    m["content"].as_array().and_then(|c| c.iter().find_map(|b| b["_oak_at"].as_i64())).unwrap_or(0)
}

fn message_text(out: &mut String, label: &str, m: &Value, full: bool) {
    let role = m["role"].as_str().unwrap_or("?");
    let t = message_time(m);
    let time = if t > 0 { format!("  {}", when(t)) } else { String::new() };
    let _ = writeln!(out, "\n=== {label} {role}{time}{} ===", oak_keys(m));
    match &m["content"] {
        Value::String(s) => {
            let _ = writeln!(out, "{}", cut(s, full));
        }
        Value::Array(blocks) => {
            for b in blocks {
                let _ = writeln!(out, "{}", block_text(b, full));
            }
        }
        Value::Null => {
            let _ = writeln!(out, "(not readable)");
        }
        v => {
            let _ = writeln!(out, "{}", cut(&v.to_string(), full));
        }
    }
}

fn header(out: &mut String, c: &Conversation) {
    let _ = writeln!(out, "# {}", c.title);
    let _ = writeln!(out, "id {}, {} messages, format {}", c.id, c.messages.len(), c.format.0);
    let _ = writeln!(out, "folder {}", c.folder.display());
    let effort = if c.effort.is_empty() { String::new() } else { format!(", effort {}", c.effort) };
    let _ = writeln!(out, "provider {}, model {}{effort}", c.provider().id(), c.model);
    if let Some(p) = &c.parent {
        let _ = writeln!(out, "subagent {} of {p}", c.agent_id);
    }
    if c.chat || c.code_mode || c.mix.is_some() {
        let _ = writeln!(out, "chat {}, code mode {}, mix {}", c.chat, c.code_mode, c.mix.is_some());
    }
    if let Some(s) = c.rolling.summary.as_ref() {
        let _ = writeln!(out, "summary in use: messages before #{} are replaced by it (made {})", s.cut, when(s.made_at));
    }
    if !c.goal.is_empty() {
        let _ = writeln!(out, "goal{}: {}", if c.goal_finished { " (finished)" } else { "" }, c.goal);
    }
    if c.paused {
        let _ = writeln!(out, "paused");
    }
    for n in &c.notes {
        let _ = writeln!(out, "note: {}", serde_json::to_string(n).unwrap_or_default());
    }
    if !c.inbox.is_empty() {
        let _ = writeln!(out, "inbox: {} message(s) waiting for the next request", c.inbox.len());
    }
}

fn mark_text(out: &mut String, m: &store::Mark, full: bool, tools: bool) {
    let kind = serde_json::to_value(m.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    let _ = writeln!(out, "\n--- line ({kind}) {} ---", when(m.time));
    if !m.text.is_empty() {
        let _ = writeln!(out, "{}", indent(&cut(&m.text, full), "  "));
    }
    let parts = store::split_sent(&m.sent);
    // (A tools line's parts are the definitions: names, unless asked.)
    if m.kind == store::MarkKind::Tools && !tools {
        let names: Vec<&str> = parts.iter().map(|(t, _)| *t).collect();
        let _ = writeln!(out, "  sent with it: {} (--tools for their definitions)", names.join(", "));
        return;
    }
    for (title, part) in parts {
        let _ = writeln!(out, "  sent with it, {title}:\n{}", indent(&cut(part, full), "    | "));
    }
}

/// The transcript as stored: messages and the lines before each.
fn stored(c: &Conversation, path: &Path, o: &Opts) -> String {
    let (from, to) = (o.from.min(c.messages.len()), o.to.min(c.messages.len()));
    if o.json {
        return serde_json::to_string_pretty(&c.messages[from..to.max(from)]).unwrap_or_default() + "\n";
    }
    let mut out = String::new();
    header(&mut out, c);
    let _ = writeln!(out, "file {}", path.display());
    let _ = writeln!(out, "\nThe transcript as stored (not what the model is sent: --sent shows that). Lines are the user's only; their \"sent with it\" parts are what the model got with them.");
    for i in from..to.max(from) {
        for m in c.marks.iter().filter(|m| m.at == i) {
            mark_text(&mut out, m, o.full, o.tools);
        }
        if c.rolling.summary.as_ref().is_some_and(|s| s.cut == i && i > 0) {
            let _ = writeln!(out, "\n--- the summary in use replaces everything before this ---");
        }
        message_text(&mut out, &format!("#{i}"), &c.messages[i], o.full);
    }
    if to == c.messages.len() {
        for m in c.marks.iter().filter(|m| m.at >= to) {
            mark_text(&mut out, m, o.full, o.tools);
        }
    }
    out
}

/// What the latest request sent, as the agent builds it from this state.
fn sent(c: &Conversation, o: &Opts) -> String {
    let pin = c.prefix.pin.as_ref();
    let system = pin.map_or(String::new(), |p| p.system.clone()) + c.memory.as_ref().map_or("", |m| m.text.as_str());
    let tools = pin.map_or(vec![], |p| p.tools.clone());
    let messages = crate::oak::clean(crate::agent::sent_messages(c));
    let total = messages.len();
    let (from, to) = (o.from.min(total), o.to.min(total));
    let messages = &messages[from..to.max(from)];
    if o.json {
        let v = json!({"system": system, "tools": tools, "messages": messages});
        return serde_json::to_string_pretty(&v).unwrap_or_default() + "\n";
    }
    let mut out = String::new();
    header(&mut out, c);
    let _ = writeln!(
        out,
        "\nWhat the latest request sent, rebuilt from the saved state by the agent's own code, in the Oak format (each provider turns it into its wire format). \
         A request made now can differ where the state would change first (a new summary renews the prompt, tools and memory)."
    );
    if c.provider() == crate::providers::Kind::Quick {
        let _ = writeln!(out, "Quick keeps the conversation on its own server: a request sends only what Quick hasn't seen; this is Oak's copy of the whole.");
    }
    if let Some(m) = c.marks.iter().rev().find(|m| m.kind == store::MarkKind::Identity) {
        for (title, part) in store::split_sent(&m.sent) {
            let _ = writeln!(out, "\n## Before the system prompt: {title}\n{part}");
        }
    }
    let day = pin.map_or("?", |p| p.day.as_str());
    let effort = pin.and_then(|p| p.effort.as_deref()).unwrap_or("none");
    let _ = writeln!(out, "\n## System prompt (as pinned on {day}, then the session memory; effort {effort})\n{}", cut(&system, o.full));
    let _ = writeln!(out, "\n## Tools ({})", tools.len());
    for t in &tools {
        if o.tools {
            let _ = writeln!(out, "{}", serde_json::to_string_pretty(t).unwrap_or_default());
        } else {
            let _ = writeln!(out, "{}", t["name"].as_str().unwrap_or("?"));
        }
    }
    let some = if messages.len() < total { format!(", {} shown", messages.len()) } else { String::new() };
    let _ = writeln!(out, "\n## Messages ({total} sent{some})");
    for (i, m) in messages.iter().enumerate() {
        message_text(&mut out, &format!("sent #{}", i + from), m, o.full);
    }
    out
}
