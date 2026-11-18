//! Protocols (MCP, the Model Context Protocol, as models know it): the
//! programs conversations can work in, each a kit LOAD offers. Oak is the
//! client, as Claude Code and Codex are. Until a conversation loads a
//! protocol it costs nothing and isn't started; then its tool joins the
//! conversation's tools, one tool named after it (BLENDER) whose `action`
//! picks the server's tool (`BLENDER action=scene_info`), and its
//! instructions join the system prompt.
//!
//! Where protocols come from (`configs`):
//! - Oak's own, from `protocols/` in this repository (built into Oak):
//!   Browser and Window are Oak's own kits (`Transport::Native`, run by
//!   tools.rs), Blender a server in Oak's process (`mcp/blender.rs`).
//! - `protocols/` in Oak's folder: a JSON file per server, named after it
//!   (`linear.json`: `{"command": "npx", "args": [...], "env": {...}}`, or
//!   `{"type": "http", "url": "...", "headers": {...}}`), or a file holding
//!   several as Claude Desktop's and Claude Code's `.mcp.json` do
//!   (`{"mcpServers": {"name": {...}}}`), so those can be dropped in as
//!   they are. Optional: `title`, `description` (what LOAD says it is),
//!   `timeout` (seconds a call may take, default 600), `cwd`, `window`
//!   (it shows a program's window). An Oak from before the rename kept them
//!   in `mcp/`, which is moved.
//!
//! Switches (Home > Protocols, `mcp_view.rs`) are kept in `protocols.json`
//! in Oak's folder (`{"off": [names]}`), every protocol on unless switched
//! off.
//!
//! Transports: stdio (a child process, newline-delimited JSON-RPC; ended
//! with Oak, through a job object on Windows), streamable HTTP (POST, the
//! answer as JSON or an event stream, `Mcp-Session-Id` kept), and built in
//! (a function). One connection per server serves every conversation; it
//! is made on first use and made again after the server exits.
//!
//! What a server offers (its tools, instructions and name) is kept in
//! `protocol-cache.json`, so requests can list a loaded server's tool
//! without starting it, and the tab can show them; a connection refreshes
//! it.

pub mod blender;
pub mod blender_run;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// The protocol version Oak asks for.
pub const PROTOCOL: &str = "2025-06-18";

/// How long a server may take to start and answer `initialize`.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// How long other requests (listing tools) may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// A tool call's default limit.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// The longest a server's tool name can be (OpenAI's limit).
const MAX_TOOL_NAME: usize = 64;

/// Oak's own protocols: this repository's `protocols/` files.
const OWN: &[(&str, &str)] = &[
    ("browser", include_str!("../protocols/browser.json")),
    ("window", include_str!("../protocols/window.json")),
    ("blender", include_str!("../protocols/blender.json")),
];

/// The servers `oak --mcp <name>` serves over stdio.
pub const SERVES: &[&str] = &[blender::NAME];

#[derive(Clone, Debug, PartialEq)]
pub enum Transport {
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf> },
    Http { url: String, headers: BTreeMap<String, String> },
    /// A server in Oak's process.
    Builtin(&'static str),
    /// One of Oak's own kits (BROWSER, WINDOW), run by tools.rs.
    Native(&'static str),
}

/// A protocol as configured.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Its name as configured (`blender`), which its switch and cache go by.
    pub name: String,
    /// What the user sees it called (`Blender`).
    pub title: String,
    /// Its kit and its tool's name (`BLENDER`).
    pub family: String,
    pub transport: Transport,
    pub description: String,
    pub timeout: Duration,
    /// It shows a program's window (the live view beside the transcript).
    pub window: bool,
    /// The file it is in (None for Oak's own).
    pub file: Option<PathBuf>,
    /// Why it can't be used (a file that doesn't read, a name taken).
    pub trouble: Option<String>,
}

impl Config {
    /// One of Oak's own (built in or native).
    pub fn builtin(&self) -> bool {
        matches!(self.transport, Transport::Builtin(_) | Transport::Native(_))
    }

    /// One of Oak's own kits, run by tools.rs rather than through MCP.
    pub fn native(&self) -> bool {
        matches!(self.transport, Transport::Native(_))
    }

    /// What it runs or reaches, in a line.
    pub fn target(&self) -> String {
        match &self.transport {
            Transport::Stdio { command, args, .. } => std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
            Transport::Http { url, .. } => url.clone(),
            Transport::Builtin(_) | Transport::Native(_) => "Built into Oak".into(),
        }
    }

    /// What tells whether a cached listing is still this server's.
    fn key(&self) -> String {
        format!("{:?}", self.transport)
    }
}

/// The folder of the user's protocol files (moved from `mcp/`, where an
/// Oak from before the rename kept them).
pub fn folder() -> PathBuf {
    let dir = crate::store::dir().join("protocols");
    let old = crate::store::dir().join("mcp");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

/// A file in Oak's folder, moved from its name before the rename.
fn data_file(name: &str, old: &str) -> PathBuf {
    let path = crate::store::dir().join(name);
    let before = crate::store::dir().join(old);
    if !path.exists() && before.exists() {
        let _ = std::fs::rename(&before, &path);
    }
    path
}

/// A name as tools can carry it: letters, digits, `_` and `-`, no `__`.
fn clean_name(s: &str) -> String {
    let mut out: String = s.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

/// The kit and tool name of protocol `name`: upper case, `-` as `_`.
pub fn family_of_name(name: &str) -> String {
    name.to_uppercase().replace('-', "_")
}

fn blank(name: &str, file: Option<PathBuf>) -> Config {
    Config {
        name: name.to_string(),
        title: String::new(),
        family: family_of_name(name),
        transport: Transport::Builtin(""),
        description: String::new(),
        timeout: CALL_TIMEOUT,
        window: false,
        file,
        trouble: None,
    }
}

/// One protocol's entry of a file.
fn parse_entry(name: &str, v: &Value, file: Option<&std::path::Path>) -> Config {
    let mut c = blank(&clean_name(name), file.map(|f| f.to_path_buf()));
    if c.name.is_empty() || !c.name.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
        c.name = name.to_string();
        c.trouble = Some("Its name has to start with a letter (a tool is named after it).".into());
        return c;
    }
    let strings = |v: &Value| -> BTreeMap<String, String> {
        v.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()
    };
    c.title = v["title"].as_str().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| c.name.clone());
    c.description = v["description"].as_str().unwrap_or("").trim().to_string();
    c.window = v["window"].as_bool().unwrap_or(false);
    if let Some(t) = v["timeout"].as_f64().filter(|t| *t > 0.0) {
        c.timeout = Duration::from_secs_f64(t);
    }
    let kind = v["type"].as_str().or(v["transport"].as_str()).unwrap_or("").to_lowercase();
    if let Some(own) = v["builtin"].as_str() {
        // Only Oak's own files name what is built in.
        c.transport = match (file, own) {
            (None, "browser") => Transport::Native("BROWSER"),
            (None, "window") => Transport::Native("WINDOW"),
            (None, blender::NAME) => Transport::Builtin(blender::NAME),
            _ => {
                c.trouble = Some(format!("\"builtin\": \"{own}\" isn't something Oak has built in."));
                Transport::Builtin("")
            }
        };
    } else if let Some(url) = v["url"].as_str().or(v["serverUrl"].as_str()) {
        if kind == "sse" {
            c.trouble = Some("The old HTTP+SSE transport isn't supported; most servers also answer streamable HTTP (\"type\": \"http\"), often at /mcp.".into());
        }
        c.transport = Transport::Http { url: url.to_string(), headers: strings(&v["headers"]) };
    } else if let Some(command) = v["command"].as_str().filter(|s| !s.trim().is_empty()) {
        let args = v["args"].as_array().into_iter().flatten().map(|a| a.as_str().map(str::to_string).unwrap_or_else(|| a.to_string())).collect();
        let cwd = v["cwd"].as_str().map(PathBuf::from);
        c.transport = Transport::Stdio { command: command.to_string(), args, env: strings(&v["env"]), cwd };
    } else {
        c.trouble = Some("It needs a \"command\" (a program to start) or a \"url\" (an HTTP server).".into());
    }
    c
}

/// The protocols configured in the files of `dir`, sorted by file.
fn read_folder(dir: &std::path::Path) -> Vec<Config> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"))).collect();
    files.sort();
    let mut out = vec![];
    for f in files {
        let stem = f.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let v: Value = match std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
            Ok(v) => v,
            Err(e) => {
                out.push(Config { trouble: Some(format!("Doesn't read: {e}")), ..blank(&clean_name(&stem), Some(f.clone())) });
                continue;
            }
        };
        match v.get("mcpServers").or(v.get("servers")).and_then(Value::as_object) {
            Some(many) => out.extend(many.iter().map(|(name, entry)| parse_entry(name, entry, Some(&f)))),
            None => out.push(parse_entry(&stem, &v, Some(&f))),
        }
    }
    out
}

/// Every protocol: Oak's own, then the user's files' (a name used twice, or
/// one that would name its tool as one of Oak's, marked as trouble). Read
/// again at most every second (a request reads them several times).
pub fn configs() -> Vec<Config> {
    static LAST: Mutex<Option<(std::time::Instant, Vec<Config>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((at, c)) = last.as_ref()
        && at.elapsed() < Duration::from_secs(1)
    {
        return c.clone();
    }
    let c = read_configs();
    *last = Some((std::time::Instant::now(), c.clone()));
    c
}

fn read_configs() -> Vec<Config> {
    let mut out: Vec<Config> = OWN.iter().map(|(name, text)| parse_entry(name, &serde_json::from_str(text).unwrap_or_default(), None)).collect();
    let taken = crate::tools::tool_names();
    for mut c in read_folder(&folder()) {
        if c.trouble.is_none() {
            if taken.contains(&c.family) || ["LOAD", "AGENT", "GOAL"].contains(&c.family.as_str()) {
                c.trouble = Some(format!("Its tool would be called {}, as one of Oak's is: rename it.", c.family));
            } else if let Some(other) = out.iter().find(|o| o.trouble.is_none() && o.family == c.family) {
                c.trouble = Some(match &other.file {
                    None => format!("\"{}\" is one of Oak's own: rename this one to use both.", c.name),
                    Some(f) => format!("{} has one of this name too.", f.display()),
                });
            }
        }
        out.push(c);
    }
    out
}

/// The protocol `name` is (its name or its family, any case).
pub fn named(name: &str) -> Option<Config> {
    let name = name.trim();
    configs().into_iter().find(|c| c.trouble.is_none() && (c.name.eq_ignore_ascii_case(name) || c.family.eq_ignore_ascii_case(name)))
}

// ------------------------------------------------------------- switches

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Switches {
    off: Vec<String>,
}

fn switches_path() -> PathBuf {
    data_file("protocols.json", "mcp.json")
}

static SWITCHES: LazyLock<Mutex<Switches>> = LazyLock::new(|| Mutex::new(crate::store::read_json(&switches_path()).unwrap_or_default()));

/// Whether protocol `name` is switched on (they all are until switched off).
pub fn enabled(name: &str) -> bool {
    !SWITCHES.lock().unwrap().off.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Switches protocols on or off; off disconnects them.
pub fn set_enabled(names: &[String], on: bool) {
    let mut s = SWITCHES.lock().unwrap();
    for n in names {
        s.off.retain(|x| !x.eq_ignore_ascii_case(n));
        if !on {
            s.off.push(n.clone());
        }
    }
    let _ = crate::store::write_json(&switches_path(), &*s);
    drop(s);
    if !on {
        for n in names {
            disconnect(n);
        }
    }
}

// ---------------------------------------------------------------- cache

/// What a server offered at its last connection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Offer {
    /// `Config::key` of the server it came from.
    pub key: String,
    /// Its tools, as MCP lists them.
    pub tools: Vec<Value>,
    pub instructions: String,
    /// "name version", from `serverInfo`.
    pub server: String,
    pub at_ms: i64,
}

fn cache_path() -> PathBuf {
    data_file("protocol-cache.json", "mcp-cache.json")
}

static CACHE: LazyLock<Mutex<HashMap<String, Offer>>> = LazyLock::new(|| Mutex::new(crate::store::read_json(&cache_path()).unwrap_or_default()));

/// What server `c` offers: a built-in one's own, else the last listing
/// (while its configuration is the same). None for Oak's native kits.
pub fn offer(c: &Config) -> Option<Offer> {
    match c.transport {
        Transport::Builtin(name) => builtin_offer(name),
        Transport::Native(_) => None,
        _ => CACHE.lock().unwrap().get(&c.name).filter(|o| o.key == c.key()).cloned(),
    }
}

fn builtin_offer(name: &str) -> Option<Offer> {
    let handler = builtin(name)?;
    let init = handler("initialize", &json!({})).ok()?;
    let tools = handler("tools/list", &json!({})).ok()?;
    Some(Offer {
        key: String::new(),
        tools: tools["tools"].as_array().cloned().unwrap_or_default(),
        instructions: init["instructions"].as_str().unwrap_or("").to_string(),
        server: format!("{} {}", init["serverInfo"]["name"].as_str().unwrap_or(name), init["serverInfo"]["version"].as_str().unwrap_or("")).trim().to_string(),
        at_ms: 0,
    })
}

fn remember(name: &str, o: Offer) {
    let mut c = CACHE.lock().unwrap();
    c.insert(name.to_string(), o);
    let _ = crate::store::write_json(&cache_path(), &*c);
}

// ------------------------------------------------------------ the kits

/// The MCP servers a conversation can load: switched on and usable, not
/// Oak's native kits (tools.rs offers those): (family, description).
pub fn kits() -> Vec<(String, String)> {
    configs()
        .into_iter()
        .filter(|c| c.trouble.is_none() && !c.native() && enabled(&c.name))
        .map(|c| {
            let desc = if !c.description.is_empty() {
                c.description.clone()
            } else {
                offer(&c).map(|o| first_sentence(&o.instructions)).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{} (MCP server)", c.title))
            };
            (c.family, desc)
        })
        .collect()
}

/// What LOAD says Oak's native kit `family` (BROWSER, WINDOW) is, from its
/// protocol file.
pub fn native_description(family: &str) -> String {
    configs().into_iter().find(|c| c.native() && c.family == family).map(|c| c.description).unwrap_or_default()
}

fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let end = s.find(". ").map(|i| i + 1).unwrap_or(s.len()).min(200);
    s[..s.floor_char_boundary(end)].trim().to_string()
}

/// The kit a conversation stores for server `c`: its family.
pub fn kit_id(c: &Config) -> String {
    c.family.clone()
}

/// The kit LOAD's `name` means: a protocol's family (any case, switched on
/// or not), else what was given, in upper case.
pub fn kit_named(name: &str) -> String {
    named(name).map(|c| c.family).unwrap_or_else(|| name.trim().to_uppercase())
}

/// The MCP server a kit is (not Oak's native kits): by family, or as an
/// Oak from before the rename stored it (`mcp:<name>`).
pub fn server_of_kit(kit: &str) -> Option<Config> {
    let c = match kit.strip_prefix("mcp:") {
        Some(name) => configs().into_iter().find(|c| c.name == name)?,
        None => configs().into_iter().find(|c| c.family == kit)?,
    };
    (c.trouble.is_none() && !c.native()).then_some(c)
}

/// Whether a conversation's `kits` have server `c` loaded.
pub fn loaded_in(kits: &[String], c: &Config) -> bool {
    kits.iter().any(|k| *k == c.family || k.strip_prefix("mcp:") == Some(c.name.as_str()))
}

/// The MCP server LOAD's `name` means (any case), if loadable.
pub fn find(name: &str) -> Option<Config> {
    named(name).filter(|c| !c.native() && enabled(&c.name))
}

/// A server's tool as Oak's older tool names named it (`mcp__<server>__<tool>`).
fn legacy_name(server: &str, tool: &str) -> String {
    let mut n = format!("mcp__{server}__{}", tool.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>());
    n.truncate(MAX_TOOL_NAME);
    n
}

/// A server tool's action: its name in lower case, without the server's
/// name in it or a leading `get_` (`get_scene_info` is `scene_info`,
/// `execute_blender_code` `execute_code`).
fn action_name(server: &str, tool: &str) -> String {
    let words = |s: &str| s.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect::<Vec<_>>();
    let all = words(tool);
    let srv = words(server);
    let mut w = all.clone();
    if !srv.is_empty()
        && let Some(i) = (0..w.len().saturating_sub(srv.len() - 1)).find(|&i| w[i..i + srv.len()] == srv[..])
        && w.len() > srv.len()
    {
        w.drain(i..i + srv.len());
    }
    if w.len() > 1 && w[0] == "get" {
        w.remove(0);
    }
    if w.is_empty() { all.join("_") } else { w.join("_") }
}

/// Server `c`'s tools by action: (action, its tool as MCP lists it).
pub fn actions(c: &Config, o: &Offer) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = vec![];
    for t in &o.tools {
        let name = t["name"].as_str().unwrap_or("");
        let mut a = action_name(&c.name, name);
        if a.is_empty() || out.iter().any(|(x, _)| *x == a) {
            a = name.to_lowercase();
        }
        out.push((a, t.clone()));
    }
    out
}

/// A tool's input schema as Oak passes it on: an object, no `$schema`.
fn schema_of(t: &Value) -> Value {
    let mut schema = t["inputSchema"].clone();
    if !schema.is_object() {
        schema = json!({"type": "object", "properties": {}});
    }
    if let Some(o) = schema.as_object_mut() {
        o.remove("$schema");
        o.insert("type".into(), json!("object"));
        o.entry("properties").or_insert(json!({}));
    }
    schema
}

fn description_of(t: &Value) -> String {
    let d = t["description"].as_str().unwrap_or("").trim();
    if d.is_empty() { t["annotations"]["title"].as_str().or(t["title"].as_str()).unwrap_or("").to_string() } else { d.to_string() }
}

/// Server `name`'s tool as the model gets it: one tool named after the
/// server, its tools as actions (`BLENDER` with `"action": "scene_info"`),
/// each with what it does and its fields, all fields side by side.
pub fn tool_defs(c: &Config) -> Vec<Value> {
    let Some(o) = offer(c) else { return vec![] };
    let acts = actions(c, &o);
    if acts.is_empty() {
        return vec![];
    }
    let mut props = serde_json::Map::new();
    props.insert("action".into(), json!({"type": "string", "enum": acts.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>()}));
    let mut lines = vec![];
    for (a, t) in &acts {
        let schema = schema_of(t);
        let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
        let mut fields = vec![];
        for (k, v) in schema["properties"].as_object().into_iter().flatten() {
            fields.push(format!("{k}{}", if required.contains(&k.as_str()) { "" } else { "?" }));
            match props.get_mut(k) {
                None => {
                    props.insert(k.clone(), v.clone());
                }
                // A field two actions mean differently: any value, each
                // action's own description says which.
                Some(have) if have != v => *have = json!({"description": "Differs by action: see each action's fields"}),
                _ => {}
            }
        }
        let fields = if fields.is_empty() { String::new() } else { format!(" (fields: {})", fields.join(", ")) };
        lines.push(format!("- {a}{fields}: {}", description_of(t).replace('\n', " ")));
    }
    let intro = if c.description.is_empty() { format!("{} (MCP server {}).", c.title, c.name) } else { format!("{}: {}.", c.title, c.description.trim_end_matches('.')) };
    let description = format!("{intro} Fields go beside `action`. `action`:\n{}", lines.join("\n"));
    vec![json!({"name": c.family, "description": description, "input_schema": {"type": "object", "properties": props, "required": ["action"]}, "eager_input_streaming": true})]
}

/// A call of a protocol's tool (`BLENDER` with its action) as Oak runs and
/// names it: `BLENDER_SCENE_INFO`, the action taken out of the input. None
/// when `name` isn't a server's tool; an error for a missing or unknown
/// action.
pub fn canonical(name: &str, input: &Value) -> Option<Result<(String, Value), String>> {
    if !is_family(name) {
        return None;
    }
    let c = configs().into_iter().find(|c| c.family == name && !c.native())?;
    let acts = offer(&c).map(|o| actions(&c, &o)).unwrap_or_default();
    let list = acts.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", ");
    let Some(action) = input["action"].as_str().map(|a| a.trim().to_lowercase()) else {
        return Some(Err(format!("Missing required field `action` (one of {list})")));
    };
    if !acts.iter().any(|(a, _)| *a == action) {
        return Some(Err(format!("Unknown action `{action}` for {name}: use one of {list}")));
    }
    let mut input = input.clone();
    if let Some(o) = input.as_object_mut() {
        o.remove("action");
    }
    Some(Ok((format!("{name}_{}", action.to_uppercase()), input)))
}

/// The protocol families (MCP servers' tools), kept a second at a time:
/// asked for every call shown.
fn families() -> Arc<Vec<String>> {
    static LAST: Mutex<Option<(std::time::Instant, Arc<Vec<String>>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((at, f)) = last.as_ref()
        && at.elapsed() < Duration::from_secs(1)
    {
        return f.clone();
    }
    let mut f: Vec<String> = configs().into_iter().filter(|c| c.trouble.is_none() && !c.native()).map(|c| c.family).collect();
    // The longest first, should one start another.
    f.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let f = Arc::new(f);
    *last = Some((std::time::Instant::now(), f.clone()));
    f
}

/// Whether `name` is an MCP server's tool (`BLENDER`).
pub fn is_family(name: &str) -> bool {
    families().iter().any(|f| f == name)
}

/// A tool Oak runs (`BLENDER_SCENE_INFO`) as its family and action
/// (`BLENDER`, `scene_info`).
pub fn split(tool: &str) -> Option<(String, String)> {
    families().iter().find_map(|f| tool.strip_prefix(f.as_str()).and_then(|r| r.strip_prefix('_')).filter(|r| !r.is_empty()).map(|r| (f.clone(), r.to_lowercase())))
}

/// The server and tool (as the server names it) a tool Oak runs is:
/// `BLENDER_SCENE_INFO`, or an older Oak's `mcp__blender__get_scene_info`.
pub fn resolve_tool(oak_name: &str) -> Option<(Config, String)> {
    if let Some(rest) = oak_name.strip_prefix("mcp__") {
        let mut configs = configs();
        configs.sort_by_key(|c| std::cmp::Reverse(c.name.len()));
        for c in configs.into_iter().filter(|c| !c.native()) {
            let Some(tool) = rest.strip_prefix(&c.name).and_then(|r| r.strip_prefix("__")) else { continue };
            let original = offer(&c).and_then(|o| o.tools.iter().filter_map(|t| t["name"].as_str()).find(|n| legacy_name(&c.name, n) == oak_name).map(str::to_string));
            return Some((c, original.unwrap_or_else(|| tool.to_string())));
        }
        return None;
    }
    let (family, action) = split(oak_name)?;
    let c = configs().into_iter().find(|c| c.family == family && !c.native())?;
    let o = offer(&c)?;
    let tool = actions(&c, &o).into_iter().find(|(a, _)| *a == action).map(|(_, t)| t["name"].as_str().unwrap_or("").to_string())?;
    Some((c, tool))
}

/// A call's arguments as server `c`'s tool `tool` takes them: only its own
/// fields (the model sees every action's side by side), and an error for a
/// required one missing.
pub fn arguments(c: &Config, tool: &str, input: &Value) -> Result<Value, String> {
    let Some(t) = offer(c).and_then(|o| o.tools.into_iter().find(|t| t["name"] == tool)) else { return Ok(input.clone()) };
    let schema = schema_of(&t);
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    let mut args = input.as_object().cloned().unwrap_or_default();
    if !props.is_empty() && schema["additionalProperties"] != true {
        args.retain(|k, _| props.contains_key(k));
    }
    for r in schema["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()) {
        if !args.contains_key(r) {
            return Err(format!("Missing required field `{r}`"));
        }
    }
    Ok(Value::Object(args))
}

/// Server `c`'s text with its tools' names as the model calls them
/// (`get_scene_info` as `BLENDER action=scene_info`).
fn with_oak_names(c: &Config, o: &Offer, text: &str) -> String {
    let mut out = text.to_string();
    let mut acts = actions(c, o);
    acts.sort_by_key(|(_, t)| std::cmp::Reverse(t["name"].as_str().unwrap_or("").len()));
    for (a, t) in acts {
        let name = t["name"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let mut s = String::new();
        let mut rest = out.as_str();
        while let Some(i) = rest.find(name) {
            let before = rest[..i].chars().next_back();
            let after = rest[i + name.len()..].chars().next();
            let word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            s.push_str(&rest[..i]);
            if word(before) || word(after) {
                s.push_str(name);
            } else {
                s.push_str(&format!("{} action={a}", c.family));
            }
            rest = &rest[i + name.len()..];
        }
        s.push_str(rest);
        out = s;
    }
    out
}

/// The instructions of the servers among `kits`, for the system prompt.
pub fn prompt_section(kits: &[String]) -> String {
    let mut parts = vec![];
    for k in kits {
        let Some(c) = server_of_kit(k) else { continue };
        if let Some(o) = offer(&c).filter(|o| !o.instructions.trim().is_empty()) {
            parts.push(format!("## {}\n{}", c.family, with_oak_names(&c, &o, o.instructions.trim())));
        }
    }
    if parts.is_empty() { String::new() } else { format!("# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n{}", parts.join("\n\n")) }
}

/// LOAD's text for server `c`, and with `through_load` how to call its
/// tool until it joins the list.
pub fn load_text(c: &Config, through_load: bool) -> String {
    let o = offer(c).unwrap_or_default();
    let mut out = format!("Loaded {} (MCP server{}).", c.family, if o.server.is_empty() { String::new() } else { format!(": {}", o.server) });
    let def = tool_defs(c);
    if def.is_empty() {
        out += " It lists no tools.";
    } else if through_load {
        out += &format!(
            " Its tool isn't in your tool list until earlier context is summarized: {}, with name={} and the tool's input in `input`, e.g. {{\"name\": \"{}\", \"input\": {{\"action\": \"...\"}}}}.\n\n{}",
            crate::prefix::THROUGH_LOAD,
            c.family,
            c.family,
            def[0]["description"].as_str().unwrap_or("")
        );
    } else {
        out += &format!(" The {} tool is in your tool list now.", c.family);
    }
    if !o.instructions.trim().is_empty() {
        out += &format!("\n\nIts instructions:\n{}", with_oak_names(c, &o, o.instructions.trim()));
    }
    out
}

// -------------------------------------------------------- connections

/// The status of a server's connection.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Connecting,
    Ready,
    Failed(String),
}

trait Conn: Send + Sync {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String>;
    fn notify(&self, method: &str, params: Value);
    fn alive(&self) -> bool;
}

struct Server {
    config: Config,
    conn: Option<Arc<dyn Conn>>,
    status: Status,
}

static SERVERS: LazyLock<Mutex<HashMap<String, Server>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// One connection attempt at a time per server.
static CONNECTING: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Server `name`'s status.
pub fn status(name: &str) -> Status {
    SERVERS.lock().unwrap().get(name).map(|s| match (&s.status, &s.conn) {
        (Status::Ready, Some(c)) if !c.alive() => Status::Idle,
        (st, _) => st.clone(),
    }).unwrap_or(Status::Idle)
}

fn set_status(c: &Config, status: Status, conn: Option<Arc<dyn Conn>>) {
    SERVERS.lock().unwrap().insert(c.name.clone(), Server { config: c.clone(), conn, status });
}

/// Ends server `name`'s connection (a stdio server's process ends).
pub fn disconnect(name: &str) {
    SERVERS.lock().unwrap().remove(name);
}

/// Ends every connection, as Oak closes.
pub fn shutdown() {
    SERVERS.lock().unwrap().clear();
}

/// Ends the connections whose configuration changed or went (the tab's
/// Reload, after the files were edited).
pub fn reload() -> usize {
    let now = configs();
    let mut s = SERVERS.lock().unwrap();
    let before = s.len();
    s.retain(|name, server| now.iter().any(|c| &c.name == name && c.trouble.is_none() && *c == server.config));
    before - s.len()
}

/// The live connection to `c`, made if there is none: started,
/// initialized, its tools listed (and remembered).
fn connection(c: &Config) -> Result<Arc<dyn Conn>, String> {
    if let Some(s) = SERVERS.lock().unwrap().get(&c.name)
        && s.config == *c
        && let Some(conn) = s.conn.as_ref().filter(|x| x.alive())
    {
        return Ok(conn.clone());
    }
    let gate = CONNECTING.lock().unwrap().entry(c.name.clone()).or_default().clone();
    let _one = gate.lock().unwrap();
    // Made meanwhile by another call?
    if let Some(s) = SERVERS.lock().unwrap().get(&c.name)
        && s.config == *c
        && let Some(conn) = s.conn.as_ref().filter(|x| x.alive())
    {
        return Ok(conn.clone());
    }
    set_status(c, Status::Connecting, None);
    match open(c) {
        Ok(conn) => {
            set_status(c, Status::Ready, Some(conn.clone()));
            Ok(conn)
        }
        Err(e) => {
            set_status(c, Status::Failed(e.clone()), None);
            Err(e)
        }
    }
}

fn open(c: &Config) -> Result<Arc<dyn Conn>, String> {
    let conn: Arc<dyn Conn> = match &c.transport {
        Transport::Builtin(name) => Arc::new(Builtin(builtin(name).ok_or("No such built-in server")?)),
        Transport::Native(kit) => return Err(format!("{kit} is one of Oak's own kits, not a server to connect to.")),
        Transport::Stdio { .. } => Arc::new(Stdio::start(c)?),
        Transport::Http { url, headers } => Arc::new(Http::new(url, headers)?),
    };
    let init = conn.request(
        "initialize",
        json!({"protocolVersion": PROTOCOL, "capabilities": {"roots": {"listChanged": false}}, "clientInfo": {"name": "oak", "title": "Oak", "version": crate::update::VERSION}}),
        START_TIMEOUT,
    )?;
    conn.notify("notifications/initialized", json!({}));
    let mut offer = Offer {
        key: c.key(),
        instructions: init["instructions"].as_str().unwrap_or("").to_string(),
        server: format!("{} {}", init["serverInfo"]["name"].as_str().unwrap_or(""), init["serverInfo"]["version"].as_str().unwrap_or("")).trim().to_string(),
        at_ms: crate::api::now_ms(),
        tools: vec![],
    };
    if !init["capabilities"]["tools"].is_null() || init["capabilities"].is_null() {
        offer.tools = list_tools(conn.as_ref())?;
    }
    if !c.builtin() {
        remember(&c.name, offer);
    }
    Ok(conn)
}

fn list_tools(conn: &dyn Conn) -> Result<Vec<Value>, String> {
    let mut tools = vec![];
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let r = conn.request("tools/list", params, LIST_TIMEOUT)?;
        tools.extend(r["tools"].as_array().cloned().unwrap_or_default());
        cursor = r["nextCursor"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

/// Connects to server `name` now (LOAD, the tab's Connect), refreshing
/// what it offers.
pub fn connect(name: &str) -> Result<(), String> {
    let c = configs().into_iter().find(|c| c.name == name).ok_or_else(|| format!("No protocol {name}."))?;
    if let Some(t) = &c.trouble {
        return Err(t.clone());
    }
    connection(&c).map(|_| ())
}

/// Calls tool `tool` (as the server names it) of server `c` with `args`:
/// the result's content as Oak blocks, and whether the server said it
/// failed. A connection that dropped is made again once.
pub fn call(c: &Config, tool: &str, args: Value) -> Result<(Vec<Value>, bool), String> {
    let args = if args.is_object() { args } else { json!({}) };
    let params = json!({"name": tool, "arguments": args});
    let conn = connection(c)?;
    let stale = { let mut s = STALE.lock().unwrap(); let was = s.contains(&c.name); s.retain(|n| n != &c.name); was };
    if stale && let Ok(tools) = list_tools(conn.as_ref()) && let Some(mut o) = offer(c) {
        o.tools = tools;
        o.at_ms = crate::api::now_ms();
        remember(&c.name, o);
    }
    let r = match Ok::<_, String>(conn)?.request("tools/call", params.clone(), c.timeout) {
        Err(e) if e.starts_with(GONE) => connection(c)?.request("tools/call", params, c.timeout)?,
        r => r?,
    };
    Ok(blocks(&r))
}

/// The start of the error when a server's process ended.
const GONE: &str = "The server exited";

/// An MCP tool result as Oak content blocks.
fn blocks(r: &Value) -> (Vec<Value>, bool) {
    let mut out = vec![];
    for item in r["content"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("") {
            "text" => out.push(json!({"type": "text", "text": item["text"].as_str().unwrap_or("")})),
            "image" => out.push(image_block(item["data"].as_str().unwrap_or(""), item["mimeType"].as_str().unwrap_or(""))),
            "audio" => out.push(json!({"type": "text", "text": format!("[audio ({}) left out: Oak can't pass audio on]", item["mimeType"].as_str().unwrap_or("?"))})),
            "resource" => {
                let res = &item["resource"];
                let uri = res["uri"].as_str().unwrap_or("");
                if let Some(text) = res["text"].as_str() {
                    out.push(json!({"type": "text", "text": format!("Resource {uri}:\n{text}")}));
                } else if res["mimeType"].as_str().is_some_and(|m| m.starts_with("image/")) {
                    out.push(image_block(res["blob"].as_str().unwrap_or(""), res["mimeType"].as_str().unwrap_or("")));
                } else {
                    out.push(json!({"type": "text", "text": format!("[resource {uri} ({}) left out: binary]", res["mimeType"].as_str().unwrap_or("?"))}));
                }
            }
            "resource_link" => out.push(json!({"type": "text", "text": format!("Resource link: {} {}{}", item["name"].as_str().unwrap_or(""), item["uri"].as_str().unwrap_or(""), item["description"].as_str().map(|d| format!(" ({d})")).unwrap_or_default())})),
            other => out.push(json!({"type": "text", "text": format!("[{other} content left out]")})),
        }
    }
    if out.is_empty()
        && let Some(s) = r.get("structuredContent").filter(|s| !s.is_null())
    {
        out.push(json!({"type": "text", "text": serde_json::to_string_pretty(s).unwrap_or_default()}));
    }
    (out, r["isError"] == true)
}

fn image_block(data: &str, mime: &str) -> Value {
    use base64::Engine;
    let img = base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|e| e.to_string()).and_then(|b| crate::images::from_bytes(&b));
    match img {
        Ok(img) => img.block(),
        Err(e) => json!({"type": "text", "text": format!("[an image ({mime}) that didn't come through: {e}]")}),
    }
}

// ---------------------------------------------------------- transports

type Handler = fn(&str, &Value) -> Result<Value, (i64, String)>;

fn builtin(name: &str) -> Option<Handler> {
    match name {
        blender::NAME => Some(blender::handle),
        _ => None,
    }
}

/// A built-in server: its function, called in Oak's process.
struct Builtin(Handler);

impl Conn for Builtin {
    fn request(&self, method: &str, params: Value, _timeout: Duration) -> Result<Value, String> {
        (self.0)(method, &params).map_err(|(code, m)| format!("{m} ({code})"))
    }
    fn notify(&self, _method: &str, _params: Value) {}
    fn alive(&self) -> bool {
        true
    }
}

/// What waits for the answers to requests: by id.
type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

/// The folder of stdio servers' stderr logs (moved from `mcp-logs/`).
pub fn logs_dir() -> PathBuf {
    data_file("protocol-logs", "mcp-logs")
}

/// A server run as a child process, speaking JSON-RPC on its stdin and
/// stdout, one message per line; its stderr goes to `protocol-logs/<name>.log`.
struct Stdio {
    stdin: Stdin,
    child: Mutex<std::process::Child>,
    pending: Pending,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
    log: PathBuf,
    #[cfg(windows)]
    job: usize,
}

/// A program as the system would find it from a shell: on Windows, `npx`
/// is `npx.cmd` somewhere on the PATH.
fn find_program(command: &str, path_var: Option<&std::ffi::OsStr>) -> PathBuf {
    let p = PathBuf::from(command);
    if p.components().count() > 1 || !cfg!(windows) {
        return p;
    }
    let exts: Vec<String> = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into()).split(';').map(|e| e.to_lowercase()).filter(|e| !e.is_empty()).collect();
    let has_ext = p.extension().is_some();
    for dir in path_var.map(std::env::split_paths).into_iter().flatten() {
        if has_ext && dir.join(command).is_file() {
            return dir.join(command);
        }
        for e in &exts {
            let c = dir.join(format!("{command}{e}"));
            if c.is_file() {
                return c;
            }
        }
    }
    p
}

impl Stdio {
    fn start(c: &Config) -> Result<Stdio, String> {
        let Transport::Stdio { command, args, env, cwd } = &c.transport else { unreachable!() };
        let path_var = env.get("PATH").map(std::ffi::OsString::from).or_else(|| std::env::var_os("PATH"));
        let program = find_program(command, path_var.as_deref());
        let mut cmd = std::process::Command::new(&program);
        cmd.args(args).envs(env).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {}: {e}", program.display()))?;
        #[cfg(windows)]
        let job = crate::cdp::job_object(&child);
        let (stdin, stdout, stderr) = (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let logs = logs_dir();
        let _ = std::fs::create_dir_all(&logs);
        let log = logs.join(format!("{}.log", c.name));
        let alive = Arc::new(AtomicBool::new(true));
        let pending: Pending = Arc::default();
        // Its stderr, to the log (the newest run only).
        {
            let log = log.clone();
            std::thread::Builder::new().name(format!("mcp-{}-stderr", c.name)).spawn(move || {
                let mut f = std::fs::File::create(&log).ok();
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if let Some(f) = f.as_mut() {
                        let _ = writeln!(f, "{line}");
                    }
                }
            }).map_err(|e| e.to_string())?;
        }
        let stdin: Stdin = Arc::new(Mutex::new(stdin));
        let s = Stdio { stdin: stdin.clone(), child: Mutex::new(child), pending: pending.clone(), next: AtomicU64::new(1), alive: alive.clone(), log: log.clone(), #[cfg(windows)] job };
        // Its stdout: answers to Oak's requests, and its own requests.
        let name = c.name.clone();
        std::thread::Builder::new().name(format!("mcp-{name}")).spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else { continue };
                if let Some(reply) = handle_incoming(&msg, &pending, &name) {
                    let mut w = stdin.lock().unwrap();
                    let _ = writeln!(w, "{reply}").and_then(|_| w.flush());
                }
            }
            alive.store(false, Ordering::SeqCst);
            let tail = log_tail(&log);
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Err(format!("{GONE}.{tail}")));
            }
        }).map_err(|e| e.to_string())?;
        Ok(s)
    }
}

/// A server's stdin, shared by the requests and the reader thread (which
/// answers the server's own requests on it).
type Stdin = Arc<Mutex<std::process::ChildStdin>>;

/// A message from a server: an answer goes to the request waiting for it;
/// a request of the server's gets the answer to send back (ping, roots;
/// sampling and elicitation are declined); notifications are dropped
/// (`tools/list_changed` marks its listing stale: `STALE`).
fn handle_incoming(msg: &Value, pending: &Pending, name: &str) -> Option<Value> {
    let Some(method) = msg["method"].as_str() else {
        if let Some(id) = msg["id"].as_u64()
            && let Some(tx) = pending.lock().unwrap().remove(&id)
        {
            let _ = tx.send(rpc_result(msg));
        }
        return None;
    };
    if method == "notifications/tools/list_changed" {
        STALE.lock().unwrap().push(name.to_string());
        return None;
    }
    let id = msg.get("id").filter(|i| !i.is_null())?;
    let result = match method {
        "ping" => json!({}),
        "roots/list" => json!({"roots": []}),
        "elicitation/create" => json!({"action": "decline"}),
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Oak doesn't support {method}")}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Servers that said their tools changed: listed again before their next use.
static STALE: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(vec![]));

fn log_tail(log: &std::path::Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let tail = lines[lines.len().saturating_sub(8)..].join("\n");
    format!(" Its last output:\n{tail}")
}

impl Stdio {
    fn write(&self, msg: &Value) -> Result<(), String> {
        let mut w = self.stdin.lock().unwrap();
        writeln!(w, "{msg}").and_then(|_| w.flush()).map_err(|e| format!("{GONE} (couldn't write to it: {e}).{}", log_tail(&self.log)))
    }
}

impl Conn for Stdio {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if !self.alive() {
            return Err(format!("{GONE}.{}", log_tail(&self.log)));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                self.notify("notifications/cancelled", json!({"requestId": id, "reason": "timed out"}));
                Err(format!("No answer to {method} within {}s.", timeout.as_secs()))
            }
        }
    }
    fn notify(&self, method: &str, params: Value) {
        let _ = self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && self.child.lock().unwrap().try_wait().ok().flatten().is_none()
    }
}

impl Drop for Stdio {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job as _);
        }
    }
}

/// A server over streamable HTTP: each message POSTed, the answer as JSON
/// or as an event stream.
struct Http {
    url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::blocking::Client,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    next: AtomicU64,
}

impl Http {
    fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<Http, String> {
        let client = reqwest::blocking::Client::builder().timeout(None).connect_timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
        Ok(Http { url: url.into(), headers: headers.clone(), client, session: Mutex::new(None), protocol: Mutex::new(None), next: AtomicU64::new(1) })
    }

    fn post(&self, body: &Value, timeout: Duration) -> Result<reqwest::blocking::Response, String> {
        let mut req = self.client.post(&self.url).timeout(timeout).header("Content-Type", "application/json").header("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k, expand_env(v));
        }
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            req = req.header("Mcp-Session-Id", s);
        }
        if let Some(p) = self.protocol.lock().unwrap().as_ref() {
            req = req.header("MCP-Protocol-Version", p);
        }
        let r = req.body(body.to_string()).send().map_err(|e| format!("Couldn't reach {}: {e}", self.url))?;
        if let Some(s) = r.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(s.to_string());
        }
        let status = r.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!("{} refused ({status}): it needs a login. Oak sends only the headers in its file (e.g. \"headers\": {{\"Authorization\": \"Bearer ...\"}}); it can't sign in through a browser (OAuth) yet.", self.url));
        }
        if !status.is_success() {
            let text = r.text().unwrap_or_default();
            return Err(format!("{} answered {status}: {}", self.url, crate::tools::one_line(&text).chars().take(300).collect::<String>()));
        }
        Ok(r)
    }
}

/// `${VAR}` in a header, from the environment.
fn expand_env(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                out.push_str(&std::env::var(&rest[i + 2..i + 2 + j]).unwrap_or_default());
                rest = &rest[i + 2 + j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The answer to request `id` from an event stream.
fn from_events(r: reqwest::blocking::Response, id: u64) -> Result<Value, String> {
    let mut data = String::new();
    for line in BufReader::new(r).lines() {
        let line = line.map_err(|e| format!("The event stream broke off: {e}"))?;
        if let Some(d) = line.strip_prefix("data:") {
            data.push_str(d.trim_start());
            data.push('\n');
        } else if line.is_empty() && !data.is_empty() {
            if let Ok(msg) = serde_json::from_str::<Value>(data.trim())
                && msg["id"].as_u64() == Some(id)
                && msg.get("method").is_none()
            {
                return rpc_result(&msg);
            }
            data.clear();
        }
    }
    if let Ok(msg) = serde_json::from_str::<Value>(data.trim())
        && msg["id"].as_u64() == Some(id)
    {
        return rpc_result(&msg);
    }
    Err("The event stream ended without an answer.".into())
}

fn rpc_result(msg: &Value) -> Result<Value, String> {
    match msg.get("error").filter(|e| !e.is_null()) {
        Some(e) => Err(format!("{} ({})", e["message"].as_str().unwrap_or("error"), e["code"])),
        None => Ok(msg["result"].clone()),
    }
}

impl Conn for Http {
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let r = self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), timeout)?;
        let events = r.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("text/event-stream"));
        let result = if events {
            from_events(r, id)
        } else {
            let v: Value = r.json().map_err(|e| format!("The answer isn't JSON: {e}"))?;
            let msg = match v {
                Value::Array(a) => a.into_iter().find(|m| m["id"].as_u64() == Some(id)).unwrap_or_default(),
                v => v,
            };
            rpc_result(&msg)
        }?;
        if method == "initialize"
            && let Some(p) = result["protocolVersion"].as_str()
        {
            *self.protocol.lock().unwrap() = Some(p.to_string());
        }
        Ok(result)
    }
    fn notify(&self, method: &str, params: Value) {
        let _ = self.post(&json!({"jsonrpc": "2.0", "method": method, "params": params}), Duration::from_secs(20));
    }
    fn alive(&self) -> bool {
        true
    }
}

/// How long ago a cached listing was made, for the tab.
pub fn age(at_ms: i64) -> String {
    if at_ms <= 0 {
        return String::new();
    }
    let secs = ((crate::api::now_ms() - at_ms) / 1000).max(0) as u64;
    crate::tasks::fmt_took(Duration::from_secs(secs))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oak-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_server_files_as_claude_code_writes_them() {
        let d = scratch("files");
        std::fs::write(d.join("own.json"), r#"{"builtin": "blender"}"#).unwrap();
        std::fs::write(d.join("linear.json"), r#"{"command": "npx", "args": ["-y", "mcp-remote", "https://mcp.linear.app/sse"], "env": {"A": "1"}, "timeout": 30}"#).unwrap();
        std::fs::write(d.join("claude.json"), r#"{"mcpServers": {"web": {"type": "http", "url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"}}, "old one": {"type": "sse", "url": "https://x/sse"}, "nothing": {}}}"#).unwrap();
        std::fs::write(d.join("broken.json"), "{nope").unwrap();
        let c = read_folder(&d);
        let by = |n: &str| c.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("{n} in {c:?}"));
        assert!(matches!(&by("linear").transport, Transport::Stdio { command, args, env, .. } if command == "npx" && args.len() == 3 && env["A"] == "1"));
        assert_eq!(by("linear").timeout, Duration::from_secs(30));
        assert!(matches!(&by("web").transport, Transport::Http { url, headers } if url.ends_with("/mcp") && headers.contains_key("Authorization")));
        assert!(by("old_one").trouble.as_deref().unwrap().contains("SSE"));
        assert!(by("nothing").trouble.is_some());
        assert!(by("broken").trouble.as_deref().unwrap().starts_with("Doesn't read"));
        assert!(by("own").trouble.as_deref().unwrap().contains("built in"));
        assert_eq!((by("old_one").family.as_str(), by("linear").title.as_str()), ("OLD_ONE", "linear"));
    }

    #[test]
    fn tools_are_named_after_their_protocol() {
        assert_eq!(action_name("blender", "get_scene_info"), "scene_info");
        assert_eq!(action_name("blender", "execute_blender_code"), "execute_code");
        assert_eq!(action_name("blender", "get_blender_info"), "info");
        assert_eq!(action_name("blender", "get"), "get");
        assert_eq!(action_name("github", "create_issue"), "create_issue");
        assert_eq!(action_name("x", "Read.File"), "read_file");
        assert_eq!(family_of_name("my-server"), "MY_SERVER");
        assert_eq!(legacy_name("blender", "get_scene_info"), "mcp__blender__get_scene_info");
        assert!(legacy_name("a-very-long-server-name", &"t".repeat(80)).len() <= MAX_TOOL_NAME);
        assert_eq!(clean_name(" My  server!"), "My_server");
        let schema = schema_of(&json!({"name": "t", "inputSchema": {"$schema": "x", "type": "object"}}));
        assert!(schema.get("$schema").is_none());
        assert!(schema["properties"].is_object());
    }

    #[test]
    fn oaks_own_protocols_are_listed() {
        let c = read_configs();
        let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&names[..3], ["browser", "window", "blender"]);
        assert!(c[0].native() && c[1].native() && !c[2].native() && c[2].builtin());
        assert_eq!((c[2].title.as_str(), c[2].family.as_str()), ("Blender", "BLENDER"));
        assert!(c.iter().take(3).all(|c| c.window && c.trouble.is_none()));
        assert!(server_of_kit("BROWSER").is_none());
        assert_eq!(server_of_kit("mcp:blender").unwrap().name, "blender");
    }

    #[test]
    fn results_become_blocks() {
        let (b, err) = blocks(&json!({"content": [{"type": "text", "text": "hi"}, {"type": "audio", "mimeType": "audio/wav", "data": ""}, {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}}], "isError": true}));
        assert!(err);
        assert_eq!(b[0]["text"], "hi");
        assert!(b[1]["text"].as_str().unwrap().contains("audio"));
        assert!(b[2]["text"].as_str().unwrap().contains("body"));
        let (b, _) = blocks(&json!({"content": [], "structuredContent": {"n": 1}}));
        assert!(b[0]["text"].as_str().unwrap().contains("\"n\": 1"));
    }

    #[test]
    fn answers_what_servers_ask() {
        let pending: Pending = Arc::default();
        let (tx, rx) = channel();
        pending.lock().unwrap().insert(7, tx);
        assert!(handle_incoming(&json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}), &pending, "s").is_none());
        assert_eq!(rx.recv().unwrap().unwrap()["ok"], true);
        assert_eq!(handle_incoming(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}), &pending, "s").unwrap()["result"], json!({}));
        assert_eq!(handle_incoming(&json!({"jsonrpc": "2.0", "id": 2, "method": "sampling/createMessage"}), &pending, "s").unwrap()["error"]["code"], -32601);
        assert!(handle_incoming(&json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}), &pending, "s").is_none());
        assert!(STALE.lock().unwrap().contains(&"s".to_string()));
    }

    #[test]
    fn headers_take_the_environment() {
        unsafe { std::env::set_var("OAK_MCP_TEST_TOKEN", "abc") };
        assert_eq!(expand_env("Bearer ${OAK_MCP_TEST_TOKEN}!"), "Bearer abc!");
        assert_eq!(expand_env("${unclosed"), "${unclosed");
    }

    fn stdio(name: &str, command: &str, args: &[&str]) -> Config {
        Config { transport: Transport::Stdio { command: command.into(), args: args.iter().map(|s| s.to_string()).collect(), env: BTreeMap::new(), cwd: None }, ..blank(name, None) }
    }

    /// Oak's own Blender server over stdio (`oak --mcp blender`), as Claude
    /// Code would run it: `cargo build`, then `cargo test oak_over_stdio -- --ignored`.
    #[test]
    #[ignore]
    fn oak_over_stdio() {
        unsafe { std::env::set_var("OAK_DIR", scratch("stdio")) };
        let exe = std::env::current_dir().unwrap().join("target/debug/oak.exe");
        let c = stdio("oakblender", exe.to_str().unwrap(), &["--mcp", "blender"]);
        let conn = connection(&c).unwrap();
        let o = offer(&c).unwrap();
        assert!(o.tools.len() >= 9, "{o:?}");
        assert!(o.instructions.contains("get_scene_info"));
        let (b, err) = call(&c, "get_scene_info", json!({})).unwrap();
        println!("{err} {}", b[0]["text"].as_str().unwrap_or("").chars().take(300).collect::<String>());
        assert!(conn.alive());
        disconnect("oakblender");
    }

    /// The reference server, over stdio and over streamable HTTP (needs
    /// npx and the network): `cargo test reference_server -- --ignored`.
    #[test]
    #[ignore]
    fn reference_server() {
        unsafe { std::env::set_var("OAK_DIR", scratch("everything")) };
        let c = stdio("everything", "npx", &["-y", "@modelcontextprotocol/server-everything"]);
        connection(&c).unwrap();
        let o = offer(&c).unwrap();
        let names: Vec<&str> = o.tools.iter().filter_map(|t| t["name"].as_str()).collect();
        println!("stdio: {} {names:?}", o.server);
        let echo = names.iter().find(|n| n.contains("echo")).unwrap();
        let (b, err) = call(&c, echo, json!({"message": "hello"})).unwrap();
        assert!(!err && b[0]["text"].as_str().unwrap().contains("hello"), "{b:?}");
        if let Some(img) = names.iter().find(|n| n.to_lowercase().contains("image")) {
            let (b, _) = call(&c, img, json!({})).unwrap();
            println!("{img}: {:?}", b.iter().map(|b| b["type"].clone()).collect::<Vec<_>>());
        }
        disconnect("everything");

        // Its streamable HTTP mode, on port 3001.
        let mut child = std::process::Command::new(find_program("npx", std::env::var_os("PATH").as_deref())).args(["-y", "@modelcontextprotocol/server-everything", "streamableHttp"]).env("PORT", "3001").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let h = Config { transport: Transport::Http { url: "http://localhost:3001/mcp".into(), headers: BTreeMap::new() }, ..blank("everything_http", None) };
        let mut ok = Err("never tried".to_string());
        for _ in 0..60 {
            ok = connection(&h).map(|_| ());
            if ok.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let _ = child.kill();
        let _ = child.wait();
        ok.unwrap();
        let o = offer(&h).unwrap();
        println!("http: {} {} tools", o.server, o.tools.len());
        assert!(!o.tools.is_empty());
    }
}


#[cfg(test)]
mod kit_tests {
    use super::*;

    #[test]
    fn a_loaded_server_joins_the_tools() {
        let plain = crate::tools::definitions_with_kits(true, false, false, &[]);
        let load = plain.iter().find(|d| d["name"] == "LOAD").unwrap();
        assert!(load["input_schema"]["properties"]["name"]["enum"].as_array().unwrap().iter().any(|n| n == "BLENDER"), "{load}");
        assert!(load["description"].as_str().unwrap().contains("Kits for a program"));
        assert!(!plain.iter().any(|d| d["name"] == "BLENDER"));
        let kits = ["BLENDER".to_string()];
        let loaded = crate::tools::definitions_with_kits(true, false, false, &kits);
        let tool = loaded.iter().find(|d| d["name"] == "BLENDER").unwrap();
        let acts = tool["input_schema"]["properties"]["action"]["enum"].as_array().unwrap();
        assert!(acts.iter().any(|a| a == "scene_info") && acts.iter().any(|a| a == "execute_code"), "{acts:?}");
        assert!(tool["input_schema"]["properties"]["code"].is_object());

        // Calls: canonical names, shown names, the server's own tool, its arguments.
        let (name, input) = crate::tools::canonical("BLENDER", &json!({"action": "object_info", "object_name": "Cube", "code": "x"})).unwrap();
        assert_eq!(name, "BLENDER_OBJECT_INFO");
        assert_eq!(crate::tools::shown_name(&name), "BLENDER object_info");
        assert_eq!(crate::tools::call_name(&name), "BLENDER action=object_info");
        let (c, tool) = resolve_tool(&name).unwrap();
        assert_eq!((c.name.as_str(), tool.as_str()), ("blender", "get_object_info"));
        assert_eq!(arguments(&c, &tool, &input).unwrap(), json!({"object_name": "Cube"}));
        assert!(arguments(&c, &tool, &json!({})).unwrap_err().contains("object_name"));
        assert!(crate::tools::canonical("BLENDER", &json!({"action": "nope"})).unwrap_err().contains("scene_info"));
        assert_eq!(resolve_tool("mcp__blender__get_scene_info").unwrap().1, "get_scene_info");
        let through = json!({"name": "blender", "input": {"action": "scene_info"}});
        let (n, _) = crate::tools::canonical_cow("LOAD", &through);
        assert_eq!(n, "BLENDER_SCENE_INFO");

        // Instructions name the tools as the model calls them.
        let section = prompt_section(&kits);
        assert!(section.starts_with("# MCP Server Instructions"));
        assert!(section.contains("BLENDER action=scene_info") && !section.contains("get_scene_info"), "{section}");
        assert!(load_text(&c, true).contains("name=BLENDER"));
        // A local model gets none of it.
        let local = crate::tools::definitions_with_kits(true, false, true, &kits);
        assert!(!local.iter().any(|d| d["name"] == "BLENDER"));
    }
}
