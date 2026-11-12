//! Freebuff's tools (the CLI's, in assets/freebuff-tools.json), run as OAK's.
//!
//! A Freebuff call streams in under its own name and arguments; `to_oak`
//! turns it into the OAK calls that do the same, so the chat shows, runs and
//! tracks them as OAK's own: a read of three files is three READs, an edit
//! with two replacements two EDITs, a code search an EXECUTE of ripgrep with
//! the CLI's arguments. The first carries the original call under `KEY`; the
//! history goes back to Freebuff as that call, answered the way the CLI
//! answers it (`result`): the same JSON, the same messages, so the model
//! never sees a tool it wasn't given.

use crate::tools::Head;
use serde_json::{Value, json};

/// The input key with the Freebuff call an OAK call came from.
pub const KEY: &str = "_oak_freebuff";

/// The OAK tool a call's first block runs as, when the name alone says so
/// (its block can then open before the arguments are in).
pub fn early_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "read_files" => "READ",
        "str_replace" => "EDIT",
        "write_file" => "WRITE",
        "web_search" => "SEARCH",
        "read_url" => "FETCH",
        "ask_user" => "ASK",
        "code_search" | "glob" | "list_directory" | "skill" | "write_todos" | "suggest_followups" | "render_ui" | "report_project_profile" | "gravity_index" => "EXECUTE",
        _ => return None,
    })
}

/// A Freebuff call as the OAK calls that run it: (id, OAK name, input). The
/// first keeps call `id` and carries the original under `KEY`; the others
/// are `{id}_{k}`, pointing back at it.
pub fn to_oak(id: &str, name: &str, args: &str) -> Vec<(String, String, Value)> {
    let v = parse_args(args);
    // str_replace's input repaired as the CLI repairs it (the call kept as
    // sent when it doesn't validate, so the error can quote it).
    let v = match name {
        "str_replace" => str_replace_input(&v).unwrap_or(v),
        _ => v,
    };
    let (parts, local) = parts(id, name, &v);
    let n = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(k, (oak, mut input))| {
            let pid = if k == 0 { id.to_string() } else { format!("{id}_{k}") };
            input[KEY] = if k == 0 {
                let mut c = json!({"name": name, "args": v, "parts": n});
                if let Some(l) = &local {
                    c["local"] = l.clone();
                }
                c
            } else {
                json!({"of": id, "k": k})
            };
            (pid, oak, input)
        })
        .collect()
}

/// Single-quoted for bash.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `command` for bash -c, written out as it is (a heredoc keeps its quotes).
fn quoted_script(command: &str) -> String {
    format!("bash -c \"$(cat <<'__FREEBUFF_COMMAND__'\n{command}\n__FREEBUFF_COMMAND__\n)\"")
}

fn execute(command: String, summary: &str, timeout: u64) -> (String, Value) {
    ("EXECUTE".into(), json!({"command": command, "summary": summary, "timeout": timeout}))
}

/// A tool the CLI answers without doing anything here: its result is fixed
/// (`local`), and the EXECUTE only shows what was asked.
fn shown_only(lines: &[String], summary: &str) -> (String, Value) {
    let args: Vec<String> = lines.iter().map(|l| q(l)).collect();
    execute(if args.is_empty() { "true".into() } else { format!("printf '%s\\n' {}", args.join(" ")) }, summary, 10)
}

/// Where a BACKGROUND command's output goes, for the model to read.
fn background_log(id: &str) -> String {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    format!("/tmp/freebuff-{safe}.log")
}

/// The hidden folders the CLI's code_search searches besides `.`.
const HIDDEN_DIRS: &str = ".agents .claude .github .gitlab .circleci .husky";

/// The CLI's limits: code_search's timeout and its global result cap.
const SEARCH_SECONDS: u64 = 10;
const SEARCH_MAX: usize = 250;
const SEARCH_CHARS: usize = 20_000;

fn parts(id: &str, name: &str, v: &Value) -> (Vec<(String, Value)>, Option<Value>) {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let cwd = v["cwd"].as_str().filter(|c| !c.trim().is_empty());
    let one = |p: (String, Value)| (vec![p], None);
    match name {
        "read_files" => read_files_parts(v),
        "str_replace" => str_replace_parts(v),
        "write_file" => one(("WRITE".into(), json!({"path": s("path"), "content": s("content")}))),
        "run_terminal_command" => one(terminal_part(id, v, cwd)),
        "code_search" => code_search_parts(v, cwd),
        "glob" => {
            let pattern = s("pattern");
            let p = pattern.trim_start_matches("./");
            // The CLI matches the pattern against the whole path from the
            // project root, so `*.ts` is only the root's: anchored here too.
            let g = if p.contains('/') { p.to_string() } else { format!("/{p}") };
            let mut command = format!("MSYS_NO_PATHCONV=1 rg --files --no-config --hidden --sort path --path-separator / -g '!.git' -g {}", q(&g));
            if let Some(c) = cwd {
                command += &format!(" -- {}", q(c));
            }
            one(execute(command, &format!("Find files matching {pattern}"), 30))
        }
        "list_directory" => {
            let path = v["path"].as_str().filter(|p| !p.trim().is_empty()).unwrap_or(".");
            one(execute(format!("ls -A -p -- {}", q(path)), &format!("List {path}"), 30))
        }
        "web_search" => one(("SEARCH".into(), json!({"objective": s("query"), "queries": [s("query")]}))),
        "read_url" => one(("FETCH".into(), json!({"url": s("url")}))),
        "ask_user" => {
            let questions: Vec<Value> = v["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|q| {
                    let options: Vec<&str> = q["options"].as_array().into_iter().flatten().filter_map(|o| o["label"].as_str().or(o.as_str())).collect();
                    json!({"question": q["question"], "options": options, "multiple": q["multiSelect"].as_bool().unwrap_or(false)})
                })
                .collect();
            one(("ASK".into(), json!({"questions": questions})))
        }
        "write_todos" => {
            let lines: Vec<String> = v["todos"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| format!("[{}] {}", if t["completed"].as_bool().unwrap_or(false) { "x" } else { " " }, t["task"].as_str().unwrap_or("")))
                .collect();
            (vec![shown_only(&lines, "Write todos")], Some(json!({"message": "Todos written"})))
        }
        "suggest_followups" => {
            let lines: Vec<String> = v["followups"].as_array().into_iter().flatten().filter_map(|f| f["prompt"].as_str().map(String::from)).collect();
            (vec![shown_only(&lines, "Suggest followups")], Some(json!({"message": "Followups suggested!"})))
        }
        "render_ui" => {
            let w = &v["widget"];
            let line = format!("{} {}", w["text"].as_str().unwrap_or(""), w["link"].as_str().unwrap_or("")).trim().to_string();
            (vec![shown_only(&[line], "Show a button")], Some(json!({"message": "UI rendered."})))
        }
        "report_project_profile" => (vec![shown_only(&[], "Report the project profile")], Some(json!({"message": "Not requested. Do not call this tool unless explicitly asked."}))),
        "gravity_index" => {
            let m = "Gravity Index is only available in the Freebuff CLI.";
            (vec![shown_only(&[m.into()], "Gravity Index")], Some(json!({"errorMessage": m})))
        }
        "skill" => load_skill(&s("name")),
        _ => one((name.to_string(), v.clone())),
    }
}

/// read_files: a READ per file (an empty list: one READ of nothing,
/// answered with an empty list).
fn read_files_parts(v: &Value) -> (Vec<(String, Value)>, Option<Value>) {
    let mut out: Vec<(String, Value)> = read_entries(v)
        .into_iter()
        .map(|(path, offset, limit)| {
            let mut input = json!({"path": path});
            if let Some(o) = offset {
                input["offset"] = json!(o);
            }
            if let Some(l) = limit {
                input["limit"] = json!(l.min(2000));
            }
            ("READ".into(), input)
        })
        .collect();
    if out.is_empty() {
        out.push(("READ".into(), json!({"path": ""})));
        return (out, Some(json!([])));
    }
    (out, None)
}

/// str_replace: an EDIT per replacement.
fn str_replace_parts(v: &Value) -> (Vec<(String, Value)>, Option<Value>) {
    match str_replace_input(v) {
        Ok(n) => {
            let path = n["path"].as_str().unwrap_or("");
            let out = replacements(&n)
                .iter()
                .map(|r| {
                    ("EDIT".into(), json!({
                        "path": path,
                        "old_string": r["oldString"].as_str().unwrap_or(""),
                        "new_string": r["newString"].as_str().unwrap_or(""),
                        "replace_all": r["allowMultiple"].as_bool().unwrap_or(false),
                    }))
                })
                .collect();
            (out, None)
        }
        // The CLI refuses it before it runs: the EDIT changes nothing
        // (its old string is empty), and the answer is the CLI's error.
        Err(e) => (vec![("EDIT".into(), json!({"path": v["path"].as_str().unwrap_or(""), "old_string": "", "new_string": ""}))], Some(Value::String(e))),
    }
}

/// run_terminal_command: an EXECUTE, or a TASK for a BACKGROUND command.
fn terminal_part(id: &str, v: &Value, cwd: Option<&str>) -> (String, Value) {
    let cd = cwd.map(|c| format!("cd -- {} && ", q(c))).unwrap_or_default();
    let command = v["command"].as_str().unwrap_or("");
    if v["process_type"] == "BACKGROUND" {
        let task = format!("{cd}{} 2>&1 | tee {}", quoted_script(command), background_log(id));
        return ("TASK".into(), json!({"action": "new", "command": task, "summary": "Run a background command"}));
    }
    match terminal_timeout(v) {
        // -1: as long as it takes (OAK's longest wait, then it runs on as a task).
        None => execute(format!("{cd}{command}"), "Run a command", crate::tools::EXECUTE_TIMEOUT_MAX),
        Some(t) => {
            let wait = (t + 10).min(crate::tools::EXECUTE_TIMEOUT_MAX);
            execute(format!("{cd}timeout -k 5 {t} {}", quoted_script(command)), "Run a command", wait)
        }
    }
}

/// code_search: an EXECUTE of rg as the CLI runs it.
fn code_search_parts(v: &Value, cwd: Option<&str>) -> (Vec<(String, Value)>, Option<Value>) {
    let fail = |m: &str| vec![shown_only(&[m.to_string()], "Search the code")];
    let Some(flags) = parse_flags(v["flags"].as_str().unwrap_or("")) else {
        return (fail("Code search flags contain an unterminated quote."), Some(json!({"errorMessage": "Code search flags contain an unterminated quote."})));
    };
    if let Some(m) = flag_refusal(&flags) {
        return (fail(&m), Some(json!({"errorMessage": m})));
    }
    let pattern = v["pattern"].as_str().unwrap_or("");
    let per_file = v["maxResults"].as_u64().unwrap_or(15).max(1);
    let files = flags.iter().take_while(|f| *f != "--").any(|f| f == "--files");
    // One more than asked per file, to know which files were cut.
    let mut rg = format!("MSYS_NO_PATHCONV=1 timeout {SEARCH_SECONDS} rg --no-config -n --with-filename --no-heading --color never --path-separator / -m {}", per_file + 1);
    for f in &flags {
        rg += &format!(" {}", q(f));
    }
    rg += " --";
    if !files {
        rg += &format!(" {}", q(pattern));
    }
    let dirs = format!("$(ls -d {HIDDEN_DIRS} 2>/dev/null)");
    let command = match cwd {
        None => format!("{rg} . {dirs}"),
        Some(c) => format!("if [ -f {c} ]; then cd -- \"$(dirname -- {c})\" && {rg} \"$(basename -- {c})\"; else cd -- {c} && {rg} . {dirs}; fi", c = q(c)),
    };
    (vec![execute(command, &format!("Search the code for {pattern}"), SEARCH_SECONDS + 10)], None)
}

/// skill: an EXECUTE that prints the skill's SKILL.md.
fn load_skill(name: &str) -> (Vec<(String, Value)>, Option<Value>) {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) || name.contains("..") {
        let content = format!("Error: Skill '{name}' not found. No skills are currently available. You can load skills created during this session by name.");
        return (vec![shown_only(&[], "Load a skill")], Some(json!({"name": name, "description": "", "content": content})));
    }
    let command = format!(
        "for d in .agents/skills .claude/skills \"$HOME/.agents/skills\" \"$HOME/.claude/skills\"; do [ -f \"$d/{name}/SKILL.md\" ] && exec cat -- \"$d/{name}/SKILL.md\"; done; echo \"Skill '{name}' not found.\" >&2; exit 1"
    );
    (vec![execute(command, &format!("Load the {name} skill"), 10)], None)
}

/// run_terminal_command's timeout in whole seconds (the CLI's default 30,
/// clamped as it clamps), or None for -1 (no timeout).
fn terminal_timeout(v: &Value) -> Option<u64> {
    let t = v["timeout_seconds"].as_f64().unwrap_or(30.0);
    (t >= 0.0).then(|| (t.ceil() as u64).clamp(1, crate::tools::EXECUTE_TIMEOUT_MAX - 10))
}

/// read_files' entries, as the CLI takes them: (path, offset, limit), with
/// repeats dropped.
fn read_entries(v: &Value) -> Vec<(String, Option<u64>, Option<u64>)> {
    let list = match &v["paths"] {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        one => vec![one.clone()],
    };
    let mut out: Vec<(String, Option<u64>, Option<u64>)> = vec![];
    for e in list {
        let entry = match &e {
            Value::String(p) => (p.clone(), None, None),
            _ => (e["path"].as_str().unwrap_or("").to_string(), e["offset"].as_u64(), e["limit"].as_u64()),
        };
        if !entry.0.is_empty() && !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// The tool arguments as the CLI takes them: double-encoded JSON is parsed
/// again (up to three times); anything but an object is `{}`.
fn parse_args(args: &str) -> Value {
    let mut v: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    for _ in 0..3 {
        let Value::String(s) = &v else { break };
        match serde_json::from_str(s) {
            Ok(p) => v = p,
            Err(_) => break,
        }
    }
    if v.is_object() { v } else { json!({}) }
}

/// str_replace's replacements, repaired as the CLI repairs them (see
/// `str_replace_input`), whether or not they validate.
fn replacements(v: &Value) -> Vec<Value> {
    let v = lift_single_replacement(v);
    coerce_to_array(v.get("replacements")).unwrap_or_default().iter().map(replacement_aliases).collect()
}

/// The CLI's `normalizeReplacementAliases`: `old_string`, `new_str` and the
/// like stand for oldString / newString when those are absent.
fn replacement_aliases(v: &Value) -> Value {
    let Value::Object(o) = v else { return v.clone() };
    let mut o = o.clone();
    for (target, aliases) in [("oldString", &["old", "old_str", "old_string"][..]), ("newString", &["new", "new_str", "new_string", "replacement"][..])] {
        if o.contains_key(target) {
            continue;
        }
        if let Some(a) = aliases.iter().find_map(|k| o.get(*k).filter(|x| x.is_string()).cloned()) {
            o.insert(target.into(), a);
        }
    }
    Value::Object(o)
}

/// The CLI's `liftSingleReplacement`: one edit sent at the top level
/// (`{path, oldString, newString}`) becomes `replacements: [it]`.
fn lift_single_replacement(v: &Value) -> Value {
    let edit = replacement_aliases(v);
    if edit.get("replacements").is_some() || !edit["oldString"].is_string() {
        return v.clone();
    }
    let mut rest = edit.as_object().cloned().unwrap_or_default();
    let mut r = serde_json::Map::new();
    for k in ["oldString", "newString", "allowMultiple"] {
        if let Some(x) = rest.remove(k) {
            r.insert(k.into(), x);
        }
    }
    rest.insert("replacements".into(), json!([r]));
    Value::Object(rest)
}

/// The CLI's `coerceToArray`: an array as it is, a string holding a JSON
/// array parsed, anything else (but absent or null) wrapped in one.
fn coerce_to_array(v: Option<&Value>) -> Option<Vec<Value>> {
    match v? {
        Value::Array(a) => Some(a.clone()),
        Value::Null => None,
        Value::String(s) => match serde_json::from_str(s) {
            Ok(Value::Array(a)) => Some(a),
            _ => Some(vec![Value::String(s.clone())]),
        },
        other => Some(vec![other.clone()]),
    }
}

/// zod's name for a value's type, as in its "received …".
fn zod_type(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

/// One zod issue, its keys in zod's order, as compact JSON.
fn invalid_type(expected: &str, path: &[Value], got: Option<&Value>) -> String {
    format!(
        r#"{{"expected":{},"code":"invalid_type","path":{},"message":{}}}"#,
        json!(expected),
        json!(path),
        json!(format!("Invalid input: expected {expected}, received {}", zod_type(got)))
    )
}

fn too_small(origin: &str, path: &[Value], message: &str) -> String {
    format!(r#"{{"origin":{},"code":"too_small","minimum":1,"inclusive":true,"path":{},"message":{}}}"#, json!(origin), json!(path), json!(message))
}

/// str_replace's input as the CLI validates it: repaired (top-level edit
/// lifted, `replacements` coerced to an array, key aliases taken), then
/// checked against its schema. Ok: the input the CLI runs, unknown keys
/// dropped and allowMultiple filled in. Err: the CLI's refusal, word for word.
fn str_replace_input(v: &Value) -> Result<Value, String> {
    let lifted = lift_single_replacement(v);
    let mut issues: Vec<String> = vec![];
    let mut missing: Vec<String> = vec![];
    let path = lifted.get("path");
    match path {
        Some(Value::String(p)) if p.is_empty() => issues.push(too_small("string", &[json!("path")], "Path cannot be empty")),
        Some(Value::String(_)) => {}
        other => issues.push(invalid_type("string", &[json!("path")], other)),
    }
    let mut reps = vec![];
    match coerce_to_array(lifted.get("replacements")) {
        None => issues.push(invalid_type("array", &[json!("replacements")], lifted.get("replacements"))),
        Some(a) if a.is_empty() => issues.push(too_small("array", &[json!("replacements")], "Replacements cannot be empty")),
        Some(a) => {
            for (i, r) in a.iter().enumerate() {
                let r = replacement_aliases(r);
                let at = |k: &str| [json!("replacements"), json!(i), json!(k)];
                let Value::Object(o) = &r else {
                    issues.push(invalid_type("object", &[json!("replacements"), json!(i)], Some(&r)));
                    continue;
                };
                for k in ["oldString", "newString"] {
                    match o.get(k) {
                        Some(Value::String(s)) if s.is_empty() && k == "oldString" => issues.push(too_small("string", &at(k), "oldString cannot be empty")),
                        Some(Value::String(_)) => {}
                        other => {
                            if other.is_none() {
                                missing.push(format!("replacements[{i}].{k}"));
                            }
                            issues.push(invalid_type("string", &at(k), other));
                        }
                    }
                }
                let multiple = match o.get("allowMultiple") {
                    None => false,
                    Some(Value::Bool(b)) => *b,
                    other => {
                        issues.push(invalid_type("boolean", &at("allowMultiple"), other));
                        false
                    }
                };
                reps.push(json!({"oldString": o.get("oldString"), "newString": o.get("newString"), "allowMultiple": multiple}));
            }
        }
    }
    if issues.is_empty() {
        return Ok(json!({"path": path, "replacements": reps}));
    }
    let details = pretty_json(&format!("[{}]", issues.join(",")));
    let body = if missing.len() == issues.len() {
        let list: Vec<String> = missing.iter().map(|m| format!("- {m}")).collect();
        format!("Missing required replacement fields:\n{}\n\nIf the intent is deletion, set \"newString\": \"\" explicitly.\n\nRaw validation issues:\n{details}", list.join("\n"))
    } else {
        details
    };
    let mut input = pretty_json(&v.to_string());
    if input.len() > 500 {
        let mut cut = 500;
        while !input.is_char_boundary(cut) {
            cut -= 1;
        }
        input = format!("{}...(truncated)", &input[..cut]);
    }
    Err(format!(
        "Invalid parameters for str_replace: {body}\n\nExpected shape: {{ \"path\": string, \"replacements\": [{{ \"oldString\": string, \"newString\": string, \"allowMultiple\"?: boolean }}] }}.\n\nOriginal tool call input:\n{input}"
    ))
}

/// Compact JSON laid out as JavaScript's `JSON.stringify(v, null, 2)` lays
/// it out, its keys left in their order.
fn pretty_json(compact: &str) -> String {
    let (mut out, mut depth, mut in_str, mut esc) = (String::new(), 0usize, false, false);
    let mut chars = compact.chars().peekable();
    let nl = |out: &mut String, d: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(d));
    };
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                if matches!(chars.peek(), Some('}' | ']')) {
                    out.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    nl(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                nl(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                nl(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// code_search's flags as the CLI splits them: on whitespace, quotes
/// keeping a value whole. None when a quote is left open.
fn parse_flags(flags: &str) -> Option<Vec<String>> {
    let (mut out, mut token, mut quote, mut started) = (vec![], String::new(), None, false);
    for c in flags.chars() {
        match quote {
            Some(qc) if c == qc => quote = None,
            Some(_) => token.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut token));
                    started = false;
                }
            }
            None => {
                token.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        out.push(token);
    }
    Some(out)
}

/// The CLI's refusals: flags that make ripgrep run another program, or
/// that leave no results to read.
fn flag_refusal(flags: &[String]) -> Option<String> {
    for f in flags.iter().take_while(|f| *f != "--") {
        if let Some(long) = f.strip_prefix("--") {
            let name = format!("--{}", long.split('=').next().unwrap_or(""));
            if ["--pre", "--pre-glob", "--hostname-bin", "--search-zip"].contains(&name.as_str()) {
                return Some(format!("Code search flag {name} is not allowed: it makes ripgrep run another program. Search again without it."));
            }
            if ["--generate", "--help", "--no-json", "--pcre2-version", "--quiet", "--type-list", "--version"].contains(&name.as_str()) {
                return Some(format!("Code search flag {name} gives this tool no search results to read. Search again without it."));
            }
        } else if let Some(short) = f.strip_prefix('-') {
            for c in short.chars() {
                if c == 'z' {
                    return Some("Code search flag -z is not allowed: it makes ripgrep run another program. Search again without it.".into());
                }
                if "hqV".contains(c) {
                    return Some(format!("Code search flag -{c} gives this tool no search results to read. Search again without it."));
                }
                if "efEmjgdtTABCMr".contains(c) {
                    break; // the rest is its value
                }
            }
        }
    }
    None
}

/// Whether code_search's flags make ripgrep list files or counts rather
/// than matching lines.
fn lists_records(flags: &[String]) -> bool {
    flags.iter().take_while(|f| *f != "--").any(|f| {
        ["--files", "--files-with-matches", "--files-without-match", "--count", "--count-matches"].contains(&f.split('=').next().unwrap_or(""))
            || (f.starts_with('-') && !f.starts_with("--") && f[1..].chars().take_while(|c| !"efEmjgdtTABCMr".contains(*c)).any(|c| c == 'l' || c == 'c'))
    })
}

/// The lowercased name of the file `path` points at.
fn file_name(path: &str) -> String {
    path.replace('\\', "/").rsplit('/').next().unwrap_or("").to_lowercase()
}

/// A `.env` file the CLI refuses to read or search (its templates aside).
fn sensitive_env(path: &str) -> bool {
    let base = file_name(path);
    let base = base.split(':').next().unwrap_or("").trim_end_matches([' ', '.']);
    (base == ".env" || base.starts_with(".env.")) && !env_template(path)
}

fn env_template(path: &str) -> bool {
    [".env.example", ".env.sample", ".env.template"].contains(&file_name(path).as_str())
}

// ------------------------------------------------------------ the way back

/// The Freebuff call an Oak tool_call block stands for.
pub enum Origin {
    /// The call itself (its first OAK call, or an OAK call of OAK's own,
    /// put the way the CLI would have made it).
    Call(Call),
    /// One of the calls after the first, answered with it.
    Part,
}

pub struct Call {
    id: String,
    pub name: String,
    pub args: Value,
    pub parts: usize,
    local: Option<Value>,
    /// Made by OAK's own tools rather than streamed from Freebuff.
    native: bool,
}

pub fn origin(b: &Value) -> Origin {
    let input = &b["input"];
    let c = &input[KEY];
    let id = b["id"].as_str().unwrap_or("").to_string();
    if c["of"].is_string() {
        return Origin::Part;
    }
    if let Some(name) = c["name"].as_str() {
        return Origin::Call(Call { id, name: name.into(), args: c["args"].clone(), parts: c["parts"].as_u64().unwrap_or(1) as usize, local: c.get("local").cloned(), native: false });
    }
    // OAK's own tools, from before the conversation moved to Freebuff.
    let mut clean = input.clone();
    if let Some(o) = clean.as_object_mut() {
        o.remove(KEY);
    }
    let s = |k: &str| input[k].clone();
    let (name, args) = match b["name"].as_str().unwrap_or("") {
        "READ" => {
            let mut e = json!({"path": s("path")});
            for k in ["offset", "limit"] {
                if input[k].is_u64() {
                    e[k] = s(k);
                }
            }
            ("read_files", json!({"paths": [e]}))
        }
        "WRITE" => ("write_file", json!({"path": s("path"), "content": s("content")})),
        "EDIT" => ("str_replace", json!({"path": s("path"), "replacements": [{"oldString": s("old_string"), "newString": s("new_string"), "allowMultiple": input["replace_all"].as_bool().unwrap_or(false)}]})),
        "EXECUTE" => ("run_terminal_command", json!({"command": s("command")})),
        "SEARCH" => ("web_search", json!({"query": input["queries"][0].as_str().or(input["objective"].as_str()).unwrap_or("")})),
        "FETCH" => ("read_url", json!({"url": s("url")})),
        other => return Origin::Call(Call { id, name: other.into(), args: clean, parts: 1, local: None, native: true }),
    };
    Origin::Call(Call { id, name: name.into(), args, parts: 1, local: None, native: true })
}

/// One OAK call's result: its text, whether it failed, and its images.
pub struct Part {
    pub text: String,
    pub error: bool,
    pub images: Vec<Value>,
}

impl Part {
    pub fn of(r: &Value) -> Part {
        let (mut text, mut images) = (String::new(), vec![]);
        match &r["content"] {
            Value::String(s) => text = s.clone(),
            Value::Array(items) => {
                for i in items {
                    match i["type"].as_str() {
                        Some("text") => text += i["text"].as_str().unwrap_or(""),
                        Some("image") => images.push(i.clone()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Part { text, error: r["is_error"].as_bool().unwrap_or(false), images }
    }

    fn missing() -> Part {
        Part { text: "Not run.".into(), error: true, images: vec![] }
    }
}

/// The CLI's statuses for a file read_files didn't read.
const DOES_NOT_EXIST: &str = "[FILE_DOES_NOT_EXIST]";
const BLOCKED: &str = "[BLOCKED]";
const READ_ERROR: &str = "[FILE_READ_ERROR]";

/// What the CLI would have answered `call` with, given what its OAK calls
/// returned: the tool message's content. Freebuff's models see no images,
/// so a read image is only described.
#[cfg(test)]
pub fn result(call: &Call, parts: Vec<Option<Part>>) -> String {
    answer(call, parts).0
}

/// The CLI's most images one read_files call attaches.
const MAX_IMAGES_PER_READ: usize = 4;

/// `result`, with the images the CLI attaches after it (read_files' first
/// four), each as a chat-completions content part. The CLI sends each in a
/// user message of its own once every call in the batch is answered.
pub fn answer(call: &Call, parts: Vec<Option<Part>>) -> (String, Vec<Value>) {
    let mut parts: Vec<Part> = parts.into_iter().map(|p| p.unwrap_or_else(Part::missing)).collect();
    if parts.is_empty() {
        parts.push(Part::missing());
    }
    // Old output OAK cleared from the context stays cleared.
    if let Some(p) = parts.iter().find(|p| p.text == crate::rolling::CLEARED) {
        return (p.text.clone(), vec![]);
    }
    let mut attached: Vec<Value> = vec![];
    let v = &call.args;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let value = if let Some(l) = &call.local {
        // A refusal the CLI gives as text (str_replace's invalid input).
        if let Some(text) = l.as_str() {
            return (text.to_string(), vec![]);
        }
        l.clone()
    } else {
        match call.name.as_str() {
            "read_files" => read_files_result(v, &parts, &mut attached),
            "str_replace" => match str_replace_result(v, &parts) {
                Ok(v) => v,
                Err(text) => return (text, vec![]),
            },
            "write_file" => match parts[0].error {
                false => json!({"file": s("path"), "message": "Wrote file successfully."}),
                true => json!({"file": s("path"), "errorMessage": "Failed to write to file: file path caused an error or file could not be written"}),
            },
            "run_terminal_command" => terminal_result(call, &parts[0]),
            "code_search" => search_result(v, &parts[0]),
            "glob" => glob_result(v, &parts[0]),
            "list_directory" => {
                let p = &parts[0];
                let (out, code) = split_exit(&p.text);
                if p.error || code != Some(0) {
                    json!({"errorMessage": format!("Failed to list directory: {}", out.trim())})
                } else {
                    let (mut files, mut directories) = (vec![], vec![]);
                    for l in out.lines().filter(|l| !l.is_empty()) {
                        match l.strip_suffix('/') {
                            Some(d) => directories.push(d.to_string()),
                            None => files.push(l.to_string()),
                        }
                    }
                    json!({"files": files, "directories": directories, "path": v["path"]})
                }
            }
            "web_search" => match parts[0].error {
                false => json!({"result": parts[0].text}),
                true => json!({"errorMessage": parts[0].text}),
            },
            "read_url" => {
                let p = &parts[0];
                if p.error {
                    json!({"url": s("url"), "errorMessage": p.text})
                } else {
                    let max = v["max_chars"].as_u64().unwrap_or(20_000).clamp(1000, 50_000) as usize;
                    let truncated = p.text.chars().count() > max;
                    let text: String = p.text.head(max);
                    json!({"url": s("url"), "text": text, "truncated": truncated})
                }
            }
            "ask_user" => ask_result(v, &parts[0]),
            "skill" => {
                let p = &parts[0];
                let name = s("name");
                if p.error || split_exit(&p.text).1 != Some(0) {
                    json!({"name": name, "description": "", "content": format!("Error: Skill '{name}' not found. No skills are currently available. You can load skills created during this session by name.")})
                } else {
                    let (text, _) = split_exit(&p.text);
                    let (description, content) = skill_parts(&text);
                    json!({"name": name, "description": description, "content": content})
                }
            }
            // Not one of the CLI's: OAK's answer as it is.
            _ => return (parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n\n"), vec![]),
        }
    };
    (value.to_string(), attached)
}

/// read_files' answer: each file's content (the text files first, then
/// the images, whose data goes to `attached`).
fn read_files_result(v: &Value, parts: &[Part], attached: &mut Vec<Value>) -> Value {
    // The CLI lists the text files first, then the images.
    let (mut files, mut images) = (Vec::<(String, String)>::new(), Vec::<(String, String)>::new());
    for ((path, offset, _), p) in read_entries(v).into_iter().zip(parts) {
        let shown = path.replace('\\', "/").trim_start_matches("./").to_string();
        if let Some(img) = p.images.first().filter(|_| !sensitive_env(&path)) {
            if images.iter().any(|(f, _)| *f == shown) {
                continue;
            }
            let content = if attached.len() < MAX_IMAGES_PER_READ {
                let mt = img["media_type"].as_str().unwrap_or("image/png");
                let data = img["data"].as_str().unwrap_or("");
                let bytes = data.len() / 4 * 3 - data.bytes().rev().take_while(|b| *b == b'=').count();
                let kb = ((bytes as f64 / 1024.0).round() as u64).max(1);
                attached.push(json!({"type": "image_url", "image_url": {"url": format!("data:{mt};base64,{data}")}}));
                format!("[IMAGE] {mt}, {kb} KB. The image is attached after this result.")
            } else {
                format!("{READ_ERROR} [Not read: read_files attaches at most {MAX_IMAGES_PER_READ} images per call. Read this image in a later call.]")
            };
            images.push((shown, content));
            continue;
        }
        let content = if sensitive_env(&path) {
            BLOCKED.to_string()
        } else {
            let c = read_content(p, offset);
            if env_template(&path) && !c.starts_with('[') { format!("[TEMPLATE]\n{c}") } else { c }
        };
        match files.iter_mut().find(|(f, _)| *f == shown) {
            Some((_, c)) => *c = format!("{c}\n\n{content}"),
            None => files.push((shown, content)),
        }
    }
    Value::Array(files.into_iter().chain(images).map(|(path, content)| json!({"path": path, "content": content})).collect())
}

/// str_replace's answer; Err for one the CLI gives as it is (the file
/// doesn't exist).
fn str_replace_result(v: &Value, parts: &[Part]) -> Result<Value, String> {
    let file = v["path"].as_str().unwrap_or("");
    let reps = replacements(v);
    let mut messages = vec![];
    let mut applied = false;
    for (k, p) in parts.iter().enumerate() {
        if !p.error {
            applied = true;
            continue;
        }
        let old = reps.get(k).and_then(|r| r["oldString"].as_str()).unwrap_or("");
        if missing_file(&p.text) {
            return Err(json!({"file": file, "errorMessage": "The file does not exist, skipping. Please use the write_file tool to create the file."}).to_string());
        }
        if let Some(m) = edit_error(&p.text, old) {
            messages.push(m);
        }
    }
    Ok(if applied {
        messages.push("String replace applied successfully.".into());
        json!({"file": file, "message": messages.join("\n\n")})
    } else {
        messages.push("No change to the file".into());
        json!({"file": file, "errorMessage": messages.join("\n\n")})
    })
}

/// READ's output as read_files gives it: the lines as they are, with the
/// CLI's note when only some of them are shown.
fn read_content(p: &Part, offset: Option<u64>) -> String {
    if p.error {
        if missing_file(&p.text) {
            return DOES_NOT_EXIST.into();
        }
        // "Offset 90 is past the end of the file, which has 12 lines."
        if let Some(n) = p.text.strip_prefix("Offset ").and_then(|r| r.split("which has ").nth(1)).and_then(|r| r.split(' ').next()) {
            return format!("[read_files: {n} lines total; offset {} is beyond the end of the file.]", offset.unwrap_or(1));
        }
        return READ_ERROR.into();
    }
    if p.text == "(empty file)" {
        return String::new();
    }
    // "[12 more lines; use offset 2001 to continue]"
    let (body, more) = match p.text.rfind("\n[") {
        Some(i) if p.text[i..].contains(" more lines; use offset ") => {
            let n = p.text[i + 2..].split(' ').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
            (&p.text[..i], n)
        }
        _ => (p.text.as_str(), 0),
    };
    let (mut first, mut last) = (None, 0u64);
    let mut lines = vec![];
    for l in body.lines() {
        match l.split_once('\t').and_then(|(n, rest)| n.trim().parse::<u64>().ok().map(|n| (n, rest))) {
            Some((n, rest)) => {
                first.get_or_insert(n);
                last = n;
                lines.push(rest);
            }
            None => lines.push(l),
        }
    }
    let start = first.unwrap_or(1);
    let total = last + more;
    let text = lines.join("\n");
    if start <= 1 && more == 0 {
        return text + "\n";
    }
    let next = if last < total {
        format!(" Use code_search to locate the part you need and read a window around it, or call read_files again with offset={} to continue.", last + 1)
    } else {
        String::new()
    };
    format!("{text}\n\n[read_files: showing lines {start}-{last} of {total}.{next}]")
}

fn missing_file(err: &str) -> bool {
    err.contains("os error 2)") || err.contains("os error 3)")
}

/// EDIT's refusal in the CLI's words (None: the replacement changed nothing).
fn edit_error(err: &str, old: &str) -> Option<String> {
    let count = |marker: &str| err.split(marker).nth(1).and_then(|r| r.split(' ').next()).and_then(|n| n.parse::<u64>().ok());
    let old_json = Value::String(old.to_string()).to_string();
    if let Some(n) = count("old_string appears ").or_else(|| count("and appears ")) {
        return Some(format!("Found {n} occurrences of {old_json} in the file. Please try again with a longer (more specified) old string or set allowMultiple to true."));
    }
    if err.contains("old_string not found") {
        return Some(format!("The old string {old_json} was not found in the file, skipping. Please try again with a different old string that matches the file content exactly."));
    }
    if err.contains("old_string is empty") {
        return Some("The old string was empty, which does not match any content, skipping.".into());
    }
    if err.contains("are the same") || err.contains("leaves the file unchanged") {
        return None;
    }
    Some(err.to_string())
}

/// EXECUTE's output without its `[exit code N]` line (`[exit code N, ran 3.2s]`), and N.
fn split_exit(text: &str) -> (String, Option<i64>) {
    if let Some(i) = text.rfind("[exit code ")
        && let Ok(code) = text[i + 11..].split([']', ',']).next().unwrap_or("").trim().parse::<i64>()
    {
        return (text[..i].trim_end_matches('\n').to_string(), Some(code));
    }
    (text.to_string(), None)
}

/// "Output so far:\n…\n\nThe command was still running…": what it printed.
fn moved_to_background(text: &str) -> Option<String> {
    let rest = text.strip_prefix("Output so far:\n")?;
    let out = rest.rsplit_once("\n\nThe ").map_or(rest, |(o, _)| o);
    Some(if out == "(none)" { String::new() } else { out.to_string() })
}

fn terminal_result(call: &Call, p: &Part) -> Value {
    let command = call.args["command"].clone();
    if call.args["process_type"] == "BACKGROUND" && !call.native {
        if p.error {
            return json!({"command": command, "errorMessage": p.text});
        }
        let log = background_log(&call.id);
        return json!({"command": command, "backgroundProcessStatus": "running", "stdoutPath": log, "stderrPath": log, "message": format!("Started background command. Check readiness and logs at {log}; starting is not proof of success. Use subsequent terminal calls to inspect it.")});
    }
    if p.error {
        return json!({"command": command, "errorMessage": p.text});
    }
    if let Some(out) = moved_to_background(&p.text) {
        return json!({"command": command, "stdout": out, "message": "The command is still running in the background; its later output is not returned."});
    }
    let (out, code) = split_exit(&p.text);
    if code == Some(124) && !call.native {
        let t = terminal_timeout(&call.args).unwrap_or(0);
        return json!({"command": command, "stdout": out, "errorMessage": format!("Command timed out after {t} seconds")});
    }
    let mut v = json!({"command": command, "stdout": out});
    if let Some(c) = code {
        v["exitCode"] = json!(c);
    }
    v
}

fn search_result(v: &Value, p: &Part) -> Value {
    if p.error {
        return json!({"errorMessage": p.text});
    }
    if let Some(out) = moved_to_background(&p.text) {
        return json!({"errorMessage": format!("Code search timed out after {SEARCH_SECONDS} seconds. The search may be too broad or the pattern too complex. Try narrowing your search with more specific flags or a more specific pattern."), "stdout": out});
    }
    let (out, code) = split_exit(&p.text);
    if code == Some(127) {
        return json!({"errorMessage": "Failed to execute ripgrep: rg isn't installed (install ripgrep and put it on PATH)."});
    }
    if code == Some(124) {
        return json!({"errorMessage": format!("Code search timed out after {SEARCH_SECONDS} seconds. The search may be too broad or the pattern too complex. Try narrowing your search with more specific flags or a more specific pattern.")});
    }
    let flags = parse_flags(v["flags"].as_str().unwrap_or("")).unwrap_or_default();
    let per_file = v["maxResults"].as_u64().unwrap_or(15).max(1) as usize;
    let truncate = |s: String| if s.len() > SEARCH_CHARS { format!("{}\n\n[Output truncated]", &s[..s.floor_char_boundary(SEARCH_CHARS)]) } else { s };
    let message = code.map(|c| format!("Exit code: {c}")).unwrap_or_default();
    if lists_records(&flags) {
        let records: Vec<&str> = out.lines().filter(|l| !l.is_empty() && !sensitive_env(l.split(':').next().unwrap_or(l))).take(SEARCH_MAX).collect();
        return json!({"stdout": truncate(format!("Found {} matches\n{}", records.len(), records.join("\n")).trim_end().to_string()), "message": message});
    }
    // Each file's lines, in the order the files came: (file, its lines
    // (number, text), how many matched, whether some were cut).
    #[allow(clippy::type_complexity)]
    let mut files: Vec<(String, Vec<(String, String)>, usize, bool)> = vec![];
    let mut other = vec![];
    let mut total = 0;
    let mut capped = false;
    for line in out.lines() {
        if line == "--" || line.is_empty() {
            continue;
        }
        let Some((file, num, text, is_match)) = rg_line(line) else {
            other.push(line.to_string());
            continue;
        };
        if sensitive_env(file) {
            continue;
        }
        let at = match files.iter().position(|f| f.0 == file) {
            Some(i) => i,
            None => {
                files.push((file.to_string(), vec![], 0, false));
                files.len() - 1
            }
        };
        let f = &mut files[at];
        if is_match {
            if f.2 >= per_file {
                f.3 = true;
                continue;
            }
            if total >= SEARCH_MAX {
                capped = true;
                break;
            }
            f.2 += 1;
            total += 1;
        }
        f.1.push((num.to_string(), text.to_string()));
    }
    let mut lines = vec![format!("Found {total} matches")];
    for (i, (file, entries, _, _)) in files.iter().enumerate() {
        if entries.is_empty() {
            continue;
        }
        if i > 0 {
            lines.push(String::new());
        }
        lines.push(format!("{file}:"));
        for (num, text) in entries {
            lines.push(format!("  Line {num}: {text}"));
        }
    }
    lines.extend(other);
    let mut stdout = if total == 0 && lines.len() == 1 { "Found 0 matches".to_string() } else { lines.join("\n") };
    let cut: Vec<String> = files.iter().filter(|f| f.3).map(|f| format!("{}: limited to {per_file} results per file", f.0)).collect();
    let mut notes = vec![];
    if !cut.is_empty() {
        notes.push(format!("Results limited to {per_file} per file. Truncated files:\n{}", cut.join("\n")));
    }
    if capped {
        notes.push(format!("Global limit of {SEARCH_MAX} results reached."));
    }
    if !notes.is_empty() {
        stdout += &format!("\n\n[{}]", notes.join("\n\n"));
    }
    json!({"stdout": truncate(stdout), "message": message})
}

/// A ripgrep line: `file:12:text` (a match) or `file-12-text` (context).
fn rg_line(line: &str) -> Option<(&str, &str, &str, bool)> {
    for (sep, is_match) in [(':', true), ('-', false)] {
        let mut from = 0;
        while let Some(i) = line[from..].find(sep).map(|i| i + from) {
            let rest = &line[i + 1..];
            let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && rest[digits..].starts_with(sep) {
                return Some((&line[..i], &rest[..digits], &rest[digits + 1..], is_match));
            }
            from = i + 1;
        }
    }
    None
}

fn glob_result(v: &Value, p: &Part) -> Value {
    let (out, code) = split_exit(&p.text);
    if p.error || code == Some(127) || code.is_some_and(|c| c > 1) {
        return json!({"errorMessage": format!("Failed to search for files: {}", out.trim())});
    }
    let all: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(|l| l.trim_start_matches("./").to_string()).collect();
    let shown: Vec<&String> = match v["max_results"].as_u64() {
        Some(n) => all.iter().take(n as usize).collect(),
        None => all.iter().collect(),
    };
    let pattern = v["pattern"].as_str().unwrap_or("");
    let mut message = format!("Found {} file(s) matching pattern \"{pattern}\"", all.len());
    if let Some(c) = v["cwd"].as_str().filter(|c| !c.is_empty()) {
        message += &format!(" in directory \"{c}\"");
    }
    if shown.len() < all.len() {
        message += &format!(". Showing the first {}; narrow the pattern or set cwd to see the rest.", shown.len());
    }
    json!({"files": shown, "count": all.len(), "message": message})
}

/// ASK's answers ("1. Question\n   Answer: a; b") as ask_user's.
fn ask_result(v: &Value, p: &Part) -> Value {
    if p.error {
        return json!({"errorMessage": p.text});
    }
    if p.text.starts_with("The user skipped") {
        return json!({"answers": [], "skipped": true});
    }
    let qs = v["questions"].as_array().cloned().unwrap_or_default();
    let mut answers = vec![];
    for (i, line) in p.text.lines().filter_map(|l| l.trim_start().strip_prefix("Answer:")).enumerate() {
        let picks: Vec<&str> = line.trim().split("; ").filter(|s| !s.is_empty()).collect();
        let mut a = json!({"questionIndex": i});
        let (other, options): (Vec<&str>, Vec<&str>) = picks.into_iter().partition(|s| s.starts_with("Other: "));
        if let Some(o) = other.first() {
            a["otherText"] = json!(o.trim_start_matches("Other: "));
        }
        if qs.get(i).is_some_and(|q| q["multiSelect"].as_bool().unwrap_or(false)) {
            if !options.is_empty() {
                a["selectedOptions"] = json!(options);
            }
        } else if let Some(o) = options.first() {
            a["selectedOption"] = json!(o);
        }
        answers.push(a);
    }
    json!({"answers": answers})
}

/// A SKILL.md's description (from its front matter) and its body.
fn skill_parts(text: &str) -> (String, String) {
    let Some(rest) = text.strip_prefix("---") else { return (String::new(), text.to_string()) };
    let Some((front, body)) = rest.split_once("\n---") else { return (String::new(), text.to_string()) };
    let description = front.lines().find_map(|l| l.strip_prefix("description:")).map(|d| d.trim().trim_matches('"').to_string()).unwrap_or_default();
    (description, body.trim_start_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(text: &str, error: bool) -> Option<Part> {
        Some(Part { text: text.into(), error, images: vec![] })
    }

    fn call(name: &str, args: Value) -> Call {
        Call { id: "c".into(), name: name.into(), args, parts: 1, local: None, native: false }
    }

    /// Every read_files entry is read (with its window), and answered the
    /// way the CLI answers: the lines as they are, its statuses and notes.
    #[test]
    fn read_files_round_trip() {
        let args = r#"{"paths": ["a.rs", {"path": "b.rs", "offset": 3, "limit": 2}, "a.rs", "gone.rs", ".env"]}"#;
        let calls = to_oak("call_1", "read_files", args);
        let ids: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(ids, ["call_1", "call_1_1", "call_1_2", "call_1_3"]);
        assert!(calls.iter().all(|c| c.1 == "READ"));
        assert_eq!(calls[1].2["offset"], 3);
        assert_eq!(calls[0].2[KEY]["parts"], 4);
        assert_eq!(calls[2].2[KEY]["of"], "call_1");
        let Origin::Call(c) = origin(&json!({"name": "READ", "input": calls[0].2})) else { panic!() };
        let out = result(
            &c,
            vec![
                part("     1\tfn a() {}\n     2\t}\n", false),
                part("     3\tthree\n     4\tfour\n\n[6 more lines; use offset 5 to continue]", false),
                part("C:\\x\\gone.rs: The system cannot find the file specified. (os error 2)", true),
                part("     1\tSECRET=1\n", false),
            ],
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0], json!({"path": "a.rs", "content": "fn a() {}\n}\n"}));
        assert_eq!(v[1]["content"], "three\nfour\n\n[read_files: showing lines 3-4 of 10. Use code_search to locate the part you need and read a window around it, or call read_files again with offset=5 to continue.]");
        assert_eq!(v[2]["content"], DOES_NOT_EXIST);
        assert_eq!(v[3]["content"], BLOCKED);
    }

    /// Each replacement is its own EDIT; the answer is the CLI's.
    #[test]
    fn str_replace_round_trip() {
        let args = r#"{"path": "a.rs", "replacements": [{"oldString": "x", "newString": "y", "allowMultiple": true}, {"oldString": "q", "newString": "r"}]}"#;
        let calls = to_oak("c", "str_replace", args);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2["replace_all"], true);
        let c = call("str_replace", serde_json::from_str(args).unwrap());
        let out = result(&c, vec![part("Edited a.rs (1 replacement; line 1)", false), part("old_string not found in the file", true)]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["file"], "a.rs");
        assert!(v["message"].as_str().unwrap().starts_with("The old string \"q\" was not found"), "{v}");
        assert!(v["message"].as_str().unwrap().ends_with("String replace applied successfully."));
    }

    /// A read image is listed after the text files and attached as the CLI
    /// attaches it (at most four per call), in a user message after the answers.
    #[test]
    fn read_files_images() {
        let img = |data: &str| Some(Part { text: String::new(), error: false, images: vec![json!({"type": "image", "media_type": "image/png", "data": data})] });
        let names = ["a.png", "b.png", "c.png", "d.png", "e.png"];
        let args = json!({"paths": names.iter().copied().chain(["t.rs"]).collect::<Vec<_>>()});
        let c = call("read_files", args);
        let mut parts: Vec<Option<Part>> = names.iter().map(|_| img("AAAA")).collect();
        parts.push(part("     1\tx\n", false));
        let (out, attached) = answer(&c, parts);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["path"], "t.rs");
        assert_eq!(v[1], json!({"path": "a.png", "content": "[IMAGE] image/png, 1 KB. The image is attached after this result."}));
        assert!(v[5]["content"].as_str().unwrap().starts_with("[FILE_READ_ERROR] [Not read: read_files attaches at most 4"));
        assert_eq!(attached.len(), 4);
        assert_eq!(attached[0], json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}));
        let read = to_oak("r", "read_files", r#"{"paths": ["a.png"]}"#);
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "tool_call", "id": "r", "name": "READ", "input": read[0].2}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "call_id": "r", "content": [{"type": "image", "media_type": "image/png", "data": "AAAA"}]}, {"type": "text", "text": "next"}]}),
        ];
        let msgs = crate::freebuff::chat_messages(&messages);
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["assistant", "tool", "user", "user"]);
        assert_eq!(msgs[2]["content"][0]["type"], "image_url");
        assert_eq!(msgs[3]["content"], "next");
    }

    /// The shapes the CLI repairs run as it runs them: a top-level edit,
    /// snake_case aliases, a stringified array, double-encoded arguments.
    #[test]
    fn str_replace_repairs() {
        let shapes = [
            r#"{"path": "a.rs", "oldString": "x", "newString": "y"}"#,
            r#"{"path": "a.rs", "old_string": "x", "new_string": "y"}"#,
            r#"{"path": "a.rs", "replacements": [{"old_str": "x", "new_str": "y"}]}"#,
            r#"{"path": "a.rs", "replacements": {"old": "x", "replacement": "y"}}"#,
            r#"{"path": "a.rs", "replacements": "[{\"oldString\": \"x\", \"newString\": \"y\"}]"}"#,
            r#""{\"path\": \"a.rs\", \"replacements\": [{\"oldString\": \"x\", \"newString\": \"y\"}]}""#,
        ];
        for args in shapes {
            let calls = to_oak("c", "str_replace", args);
            assert_eq!(calls.len(), 1, "{args}");
            assert_eq!(calls[0].2["path"], "a.rs", "{args}");
            assert_eq!(calls[0].2["old_string"], "x", "{args}");
            assert_eq!(calls[0].2["new_string"], "y", "{args}");
            assert!(calls[0].2[KEY].get("local").is_none(), "{args}");
            assert_eq!(calls[0].2[KEY]["args"]["replacements"][0]["oldString"], "x", "{args}");
        }
        // What doesn't validate is refused in the CLI's words, nothing edited.
        let calls = to_oak("c", "str_replace", r#"{"path": "a.rs", "replacements": [{"oldString": "x"}]}"#);
        assert_eq!(calls[0].2["old_string"], "");
        let Origin::Call(c) = origin(&json!({"name": "EDIT", "input": calls[0].2})) else { panic!() };
        let out = result(&c, vec![part("old_string is empty", true)]);
        assert!(out.starts_with("Invalid parameters for str_replace: Missing required replacement fields:\n- replacements[0].newString\n"), "{out}");
        assert!(out.contains("\"path\": [\n      \"replacements\",\n      0,\n      \"newString\"\n    ],"), "{out}");
        assert!(out.contains("Original tool call input:\n{\n  \"path\": \"a.rs\","), "{out}");
        let out = str_replace_input(&json!({"path": "", "replacements": []})).unwrap_err();
        assert!(out.contains("Path cannot be empty") && out.contains("Replacements cannot be empty"), "{out}");
    }

    /// code_search runs ripgrep with the CLI's arguments, quoted, and its
    /// output is grouped by file, capped per file as the CLI caps it.
    #[test]
    fn code_search_round_trip() {
        let args = r#"{"pattern": "fn main(", "flags": "-i -g '*.rs'", "maxResults": 2}"#;
        let calls = to_oak("c", "code_search", args);
        let cmd = calls[0].2["command"].as_str().unwrap();
        assert!(cmd.contains("-m 3 '-i' '-g' '*.rs' -- 'fn main(' ."), "{cmd}");
        assert!(to_oak("c", "code_search", r#"{"pattern": "x", "flags": "--pre cat"}"#)[0].2[KEY]["local"]["errorMessage"].is_string());
        let c = call("code_search", serde_json::from_str(args).unwrap());
        let out = "./src/a.rs:1:fn main() {\n./src/a.rs:5:fn main(x)\n./src/a.rs:9:fn main(y)\n./b.rs-2-context\n./b.rs:3:fn main(z)\n[exit code 0]";
        let out = result(&c, vec![part(out, false)]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["stdout"],
            "Found 3 matches\n./src/a.rs:\n  Line 1: fn main() {\n  Line 5: fn main(x)\n\n./b.rs:\n  Line 2: context\n  Line 3: fn main(z)\n\n[Results limited to 2 per file. Truncated files:\n./src/a.rs: limited to 2 results per file]"
        );
        assert_eq!(v["message"], "Exit code: 0");
    }

    /// The other tools: what each runs as, and the CLI's answer.
    #[test]
    fn other_tools() {
        let t = to_oak("c", "run_terminal_command", r#"{"command": "echo 'hi'", "cwd": "sub", "timeout_seconds": 5}"#);
        assert_eq!(t[0].1, "EXECUTE");
        assert_eq!(t[0].2["timeout"], 15);
        assert!(t[0].2["command"].as_str().unwrap().starts_with("cd -- 'sub' && timeout -k 5 5 bash -c"));
        assert_eq!(to_oak("c", "run_terminal_command", r#"{"command": "srv", "process_type": "BACKGROUND"}"#)[0].1, "TASK");
        let c = call("run_terminal_command", json!({"command": "x", "timeout_seconds": 5}));
        assert_eq!(serde_json::from_str::<Value>(&result(&c, vec![part("partial\n[exit code 124]", false)])).unwrap()["errorMessage"], "Command timed out after 5 seconds");
        let c = call("list_directory", json!({"path": "src"}));
        let v: Value = serde_json::from_str(&result(&c, vec![part("a.rs\nsub/\n[exit code 0]", false)])).unwrap();
        assert_eq!(v, json!({"files": ["a.rs"], "directories": ["sub"], "path": "src"}));
        let g = to_oak("c", "glob", r#"{"pattern": "*.ts"}"#);
        assert!(g[0].2["command"].as_str().unwrap().contains("-g '/*.ts'"));
        let todo = to_oak("c", "write_todos", r#"{"todos": [{"task": "a", "completed": true}]}"#);
        assert_eq!(todo[0].1, "EXECUTE");
        let Origin::Call(c) = origin(&json!({"name": "EXECUTE", "input": todo[0].2})) else { panic!() };
        assert_eq!(result(&c, vec![part("[x] a\n[exit code 0]", false)]), r#"{"message":"Todos written"}"#);
        // Every tool the CLI gives the model maps onto one OAK has.
        let defs = crate::tools::definitions_as(false, false, false, false);
        let defs = defs.as_array().unwrap();
        for t in serde_json::from_str::<Value>(include_str!("../assets/freebuff-tools.json")).unwrap().as_array().unwrap() {
            let name = t["function"]["name"].as_str().or(t["name"].as_str()).unwrap();
            let first = &to_oak("c", name, "{}")[0];
            assert!(defs.iter().any(|d| d["name"] == first.1.as_str()), "{name} -> {}", first.1);
            if let Some(e) = early_name(name) {
                assert_eq!(e, first.1, "{name}");
            }
        }
    }
}
