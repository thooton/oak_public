// The tools the model gets: one definition per tool OAK runs
// (`run_definitions`), each family's actions merged into one tool
// (`families`). tools.rs runs and checks calls against these.

use super::*;

/// The tools the model gets: the tools OAK runs, with each family's as
/// one tool, every kit's included (LOAD isn't). A sub-orchestrator
/// (`spawner`) sees AGENT as the main session does (its own agents),
/// without ASK and progress meters.
pub fn definitions_as(agent: bool, spawner: bool, local: bool, goal: bool) -> Vec<Value> {
    let mut all = run_definitions(agent, spawner, local, goal);
    all.retain(|d| d["name"] != "LOAD");
    families(all, agent && !spawner)
}

/// The tools OAK runs (one for each family action), which calls are
/// checked and repaired against.
pub fn run_definitions(agent: bool, spawner: bool, local: bool, goal: bool) -> Vec<Value> {
    let Value::Array(mut all) = definitions_with(agent && !spawner) else { unreachable!() };
    if agent {
        all.retain(|d| allowed(d["name"].as_str().unwrap_or(""), agent, spawner));
    }
    if local {
        all.retain(|d| {
            let n = d["name"].as_str().unwrap_or("");
            LOCAL_TOOLS.contains(&n) || (goal && GOAL_TOOLS.contains(&n))
        });
    }
    all
}

/// The tools a session gets with `kits` loaded: a kit's tools only once it
/// is (`kit_of`), and LOAD for the others. A local model has no kits but
/// GOAL, which comes with a goal.
pub fn definitions_with_kits(agent: bool, spawner: bool, local: bool, kits: &[String]) -> Vec<Value> {
    let has = |k: &str| kits.iter().any(|x| x == k);
    let mut all = run_definitions(agent, spawner, local, has("GOAL"));
    let loadable = kits_for(agent);
    all.retain(|d| {
        let name = d["name"].as_str().unwrap_or("");
        match name {
            "LOAD" => !loadable.is_empty(),
            _ => kit_of(name, agent).is_none_or(has),
        }
    });
    if let Some(load) = all.iter_mut().find(|d| d["name"] == "LOAD") {
        *load = load_tool(&loadable);
    }
    families(all, agent && !spawner)
}

/// The kits a session can load: a subagent has AGENT always (to reach the
/// main session) and no browser.
pub fn kits_for(agent: bool) -> Vec<&'static str> {
    KITS.iter().map(|(k, _)| *k).filter(|k| !agent || *k == "WINDOW").collect()
}

/// The kits: what each is, as LOAD's description says.
const KITS: &[(&str, &str)] = &[
    ("AGENT", "subagents (only when the user asks for them)"),
    ("BROWSER", "Oak's browser pane (only when the user explicitly asks you to use the browser tool)"),
    ("WINDOW", "trying a GUI app on a hidden desktop, without touching the user's screen"),
];

/// The kit tool OAK runs `tool` belongs to, for a session (`agent`: a
/// subagent's AGENT is always there).
pub fn kit_of(tool: &str, agent: bool) -> Option<&'static str> {
    if tool.starts_with("BROWSER_") {
        return Some("BROWSER");
    }
    (!agent && tool.starts_with("AGENT_")).then_some("AGENT")
}

/// LOAD, offering `kits`.
fn load_tool(kits: &[&str]) -> Value {
    let list: Vec<String> = KITS.iter().filter(|(k, _)| kits.contains(k)).map(|(k, d)| format!("{k}: {d}")).collect();
    json!({
        "name": "LOAD",
        "description": format!(
            "Loads a kit: tools or instructions you don't have by default. Its instructions come back as the result, and its tool is yours from then on. Load one only when the task needs it. Kits: {}.",
            list.join("; ")
        ),
        "input_schema": {"type": "object", "properties": {
            "name": {"type": "string", "enum": kits},
            "input": {"type": "object", "description": "Only when Oak says a loaded tool isn't in your tool list yet: that tool's input, to call it through LOAD."},
        }, "required": ["name"]},
        "eager_input_streaming": true,
    })
}

/// For a kit loaded where tools can't be added mid-conversation: its tool
/// as text (what it does, its actions and their fields), since the model
/// calls it through LOAD until it joins the list.
pub fn kit_help(kit: &str, agent: bool) -> String {
    if kit == "BROWSER" {
        return browser_help();
    }
    let all = definitions_as(agent, false, false, false);
    let Some(d) = all.iter().find(|d| d["name"] == kit) else { return String::new() };
    let required: Vec<&str> = d["input_schema"]["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
    let fields: Vec<String> = d["input_schema"]["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| {
            let what = v["enum"].as_array().map(|e| e.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("|")).unwrap_or_else(|| v["type"].as_str().unwrap_or("").to_string());
            let desc = v["description"].as_str().map(|d| format!(" ({d})")).unwrap_or_default();
            format!("- {k}{}: {what}{desc}", if required.contains(&k.as_str()) { "" } else { "?" })
        })
        .collect();
    format!("{kit}: {}\nFields:\n{}", d["description"].as_str().unwrap_or(""), fields.join("\n"))
}

/// What each family is, before its actions.
fn family_intro(family: &str, agent: bool) -> String {
    match family {
        "TASK" => format!("Background commands ({}, working folder).", SHELL.1),
        "NOTE" => "Your notes, which stay in your system prompt.".into(),
        "HEARTBEAT" => "Reminders while you are idle.".into(),
        "PROGRESS" => "Only when the user asks for one.".into(),
        "GOAL" => "Only while a goal is set. A goal is separate from the user's ordinary requests: the user sets it explicitly, it arrives in a <goal> message and stays listed under \"Goal\" in your system prompt until finished. With no goal listed there, there is none: don't use this tool, even for a big task.".into(),
        "BROWSER" => BROWSER_INTRO.into(),
        "AGENT" if agent => "The main session (id 0) and its other subagents.".into(),
        "AGENT" => "Subagents: other agents with your tools (except ASK), working in this folder in the background. Their messages, and the final reply each sends when it ends its turn, come to you as <agent-message> blocks with your next request.".into(),
        _ => String::new(),
    }
}

/// Puts the tools of each family into one: at the place of its first, with
/// what the family is and then what each action does (its tool's
/// description), all their fields, and `action`.
fn families(tools: Vec<Value>, agent: bool) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for t in tools {
        let name = t["name"].as_str().unwrap_or("").to_string();
        let Some((family, action)) = family_of(&name) else {
            out.push(t);
            continue;
        };
        let at = match out.iter().position(|d| d["name"] == family) {
            Some(i) => i,
            None => {
                out.push(json!({
                    "name": family,
                    "description": family_intro(family, agent),
                    "input_schema": {"type": "object", "properties": {"action": {"type": "string", "enum": []}}, "required": ["action"]},
                    "eager_input_streaming": true,
                }));
                out.len() - 1
            }
        };
        let d = &mut out[at];
        let desc = d["description"].as_str().unwrap_or("").to_string();
        let sep = if desc.contains("`action`") { "; " } else { " `action`: " };
        d["description"] = json!(format!("{desc}{sep}{action}: {}", t["description"].as_str().unwrap_or("").trim_end_matches('.')));
        d["input_schema"]["properties"]["action"]["enum"].as_array_mut().unwrap().push(json!(action));
        for (k, v) in t["input_schema"]["properties"].as_object().into_iter().flatten() {
            if d["input_schema"]["properties"].get(k).is_none() {
                d["input_schema"]["properties"][k] = v.clone();
            }
        }
    }
    for d in out.iter_mut().filter(|d| FAMILIES.iter().any(|(f, _)| d["name"] == *f)) {
        d["description"] = json!(d["description"].as_str().unwrap_or("").trim().to_string() + ".");
    }
    out
}

/// What BROWSER is, before its actions.
const BROWSER_INTRO: &str = "Drives the page in Oak's browser pane; the user sees and uses it at the same time. One page. Coordinates are viewport CSS pixels, as in its screenshots. Only use it when the user explicitly asks you to use the browser tool.";

/// BROWSER action=help: each action, what it does and its fields.
pub fn browser_help() -> String {
    let defs = definitions_with(false);
    let mut out = String::from(
        "BROWSER drives the page in Oak's browser pane; the user sees and uses it at the same time. One page. Coordinates are viewport CSS pixels, as in its screenshots. Fields go beside `action`.\n",
    );
    let Some((_, actions)) = FAMILIES.iter().find(|(f, _)| *f == "BROWSER") else { return out };
    for (action, tool) in actions.iter().filter(|(a, _)| *a != "help") {
        let Some(d) = defs.as_array().into_iter().flatten().find(|d| d["name"] == *tool) else { continue };
        let required: Vec<&str> = d["input_schema"]["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
        let fields: Vec<String> = d["input_schema"]["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| format!("{k}{}: {}", if required.contains(&k.as_str()) { "" } else { "?" }, v["type"].as_str().unwrap_or("")))
            .collect();
        let fields = if fields.is_empty() { String::new() } else { format!(" ({})", fields.join(", ")) };
        out += &format!("- {action}{fields}: {}\n", d["description"].as_str().unwrap_or(""));
    }
    out
}

fn definitions_with(agent: bool) -> Value {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "input_schema": {"type": "object", "properties": properties, "required": required},
            "eager_input_streaming": true,
        })
    };

    let (send, list) = if agent {
        (
            "message the main session (id 0) or another subagent; it arrives with their next request, even mid-work, and wakes them if idle. Your final reply reaches the main session by itself, so use this for what it should know sooner: progress, findings it is waiting for, or a question",
            "the main session (id 0) and its subagents: ID, state (working, idle or paused), name",
        )
    } else {
        (
            "message agent `id`, to steer it, answer it or give it more to do; it arrives with its next request, even mid-work (after the tools it is running), and wakes it if idle",
            "your subagents: ID, state (working, idle or paused), when it last changed, name",
        )
    };
    json!([
        tool("READ", "Read a text file, with line numbers. Use offset/limit for large files (default: first 2000 lines). Images (PNG, JPEG, GIF, WebP) come back as the image, so you can look at them.",
            json!({"path": {"type": "string", "description": "File path, absolute or relative to the working folder"},
                   "offset": {"type": "integer", "description": "1-based line to start at"},
                   "limit": {"type": "integer", "description": "Number of lines to read"}}), &["path"]),
        tool("WRITE", "Create or overwrite a file with the given content. Parent folders are created.",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"]),
        tool("EDIT", "Replace an exact string in a file. old_string must appear exactly once unless replace_all is true. Read the file first.",
            json!({"path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"},
                   "replace_all": {"type": "boolean"}}), &["path", "old_string", "new_string"]),
        tool("EXECUTE", &format!("Run a command in {} in the working folder and wait up to `timeout` seconds ({EXECUTE_TIMEOUT_MIN} to {EXECUTE_TIMEOUT_MAX}). If it is still running then, or the user sends a message, it keeps running as a background task and you get its task ID.", SHELL.0),
            json!({"command": {"type": "string"},
                   "summary": {"type": "string", "description": SUMMARY},
                   "timeout": {"type": "integer", "description": "Seconds to wait; pick one that fits the command"}}), &["command", "summary", "timeout"]),
        tool("TASK_NEW", "start `command` (for servers, watchers and other long-running commands); returns its task ID, and you are notified when it finishes",
            json!({"command": {"type": "string"}, "summary": {"type": "string", "description": SUMMARY}}), &["command", "summary"]),
        tool("TASK_GET_NEW_OUTPUT", "the lines task `id` printed since the last call, and whether it is still running",
            json!({"id": {"type": "integer"}}), &["id"]),
        tool("TASK_WAIT", &format!("like output, but first wait for the task to finish, at most `timeout` seconds (1 to {TASK_WAIT_MAX}); the wait also ends when the user sends a message"),
            json!({"id": {"type": "integer"},
                   "timeout": {"type": "integer", "description": format!("For wait: seconds, 1 to {TASK_WAIT_MAX}")}}), &["id", "timeout"]),
        tool("TASK_LIST", "the tasks: ID, running time, status, start of the command", json!({}), &[]),
        tool("TASK_DELETE", "stop task `id` (SIGINT, then SIGKILL after 2 seconds) and remove it",
            json!({"id": {"type": "integer"}}), &["id"]),
        tool("SEARCH", "Search the web, like a search engine: returns a ranked list of pages, each with its title, link and a short summary (not the page text). FETCH the links worth reading. Give the objective in plain words and one or more keyword queries; the queries run in parallel and their results are merged.",
            json!({"objective": {"type": "string", "description": "What you are trying to find out"},
                   "queries": {"type": "array", "items": {"type": "string"}, "description": "1-5 keyword queries"},
                   "max_results": {"type": "integer", "description": "How many results, 1-20 (default 10)"}}), &["objective", "queries"]),
        tool("NOTE_NEW", "save `text`; returns its ID",
            json!({"text": {"type": "string"}}), &["text"]),
        tool("NOTE_UPDATE", "replace note `id`'s text, in full, so it holds the current state of what it tracks",
            json!({"id": {"type": "integer"}, "text": {"type": "string"}}), &["id", "text"]),
        tool("NOTE_DELETE", "delete note `id`", json!({"id": {"type": "integer"}}), &["id"]),
        tool("HEARTBEAT_START", "have Oak send you `message` every `interval_minutes` minutes, e.g. to check on a long job; returns its ID",
            json!({"message": {"type": "string", "description": "What to tell you each time, e.g. what to check"},
                   "interval_minutes": {"type": "integer", "description": "1 to 10080"}}), &["message", "interval_minutes"]),
        tool("HEARTBEAT_STOP", "stop heartbeat `id`, as soon as it is no longer needed", json!({"id": {"type": "integer"}}), &["id"]),
        tool("PROGRESS_SET", &format!("a meter under the goal showing `label` and a value: the last line `command` prints ({}, at most {}s; run now and every `interval_minutes`), starting with `x/y`, `N%` or a number. The same label replaces one; returns its ID and the first reading", SHELL.1, crate::progress::RUN_TIMEOUT.as_secs()),
            json!({"label": {"type": "string", "description": "A few words, e.g. \"Tests passing\""},
                   "command": {"type": "string"},
                   "interval_minutes": {"type": "integer", "description": "1 to 10080"}}), &["label", "command", "interval_minutes"]),
        tool("PROGRESS_REMOVE", "remove meter `id`", json!({"id": {"type": "integer"}}), &["id"]),
        tool("GOAL_JUSTIFICATION", "once you have completed all of it, record `justification`: how you did, in its entirety (calling it again replaces it)",
            json!({"justification": {"type": "string"}}), &["justification"]),
        tool("GOAL_FINISH", "finish the goal, once the justification accurately reflects how you fulfilled it in its entirety. Until then, Oak asks you to continue whenever you end your turn; after this, control goes back to the user when you end your turn",
            json!({}), &[]),
        tool("FETCH", "Fetch a web page as readable text (headings start with #, links show as `text (url)`). Long pages come in parts of 30000 characters.",
            json!({"url": {"type": "string"},
                   "raw": {"type": "boolean", "description": "Return the page source (HTML, CSS, JS) instead"},
                   "render": {"type": "boolean", "description": "Run the page's scripts in headless Chrome first, for pages built by JavaScript (slower)"},
                   "wait_ms": {"type": "integer", "description": "With render: how long scripts may run, default 2000"},
                   "start": {"type": "integer", "description": "Character to start at, to read on in a long page"}}), &["url"]),
        tool("SCREENSHOT", "Screenshot a web page (http(s):// or file:///) in headless Chrome and see it. The window is 1280x800 unless you give width and height (a bigger height shows more of a long page); wait_ms (default 2000) lets scripts and animations run first.",
            json!({"url": {"type": "string"}, "width": {"type": "integer"}, "height": {"type": "integer"},
                   "wait_ms": {"type": "integer"},
                   "path": {"type": "string", "description": "Also save the PNG here (relative to the working folder)"}}), &["url"]),
        tool("ASK", "Ask the user questions when a decision is genuinely theirs (not for things you can find out yourself), and wait for the answers. Give options to click when there are clear choices; they can always write their own answer instead. While a goal is active, ASK works only after the user accepts a REQUEST_ASK, and then once.",
            json!({"questions": {"type": "array", "description": "1-6 questions", "items": {"type": "object", "properties": {
                       "question": {"type": "string"},
                       "options": {"type": "array", "items": {"type": "string"}, "description": "Choices to click (may be empty)"},
                       "multiple": {"type": "boolean", "description": "Allow picking several options"}},
                   "required": ["question"]}}}), &["questions"]),
        tool("REQUEST_ASK", "While a goal is active, call this instead of ASK, with what you want to ask the user about. It returns at once and you keep working; the request is shown to the user, and a later message tells you whether they accepted (then you may call ASK once) or declined. One request at a time.",
            json!({"about": {"type": "string", "description": "What you want to ask the user about, in a sentence or two (shown to them)"}}), &["about"]),
        tool("DISPLAY", &format!("Show files to the user in the chat: images as pictures, text files as a preview, others as a link. Use it to show results (screenshots, charts, reports) instead of only naming their paths. Only the user sees them. 1 to {DISPLAY_MAX} files."),
            json!({"paths": {"type": "array", "items": {"type": "string"}, "description": "File paths, absolute or relative to the working folder"}}), &["paths"]),
        tool("AGENT_NEW", "start one on the task in `prompt`, which must be complete and self-contained (it sees nothing of this conversation; give agents that change files separate parts); returns its ID at once",
            agent_new_properties(), &["name", "prompt"]),
        tool("AGENT_SEND", send,
            json!({"id": {"type": "integer", "description": if agent { "0 for the main session, or another agent's ID" } else { "The agent's ID" }},
                   "message": {"type": "string"}}), &["id", "message"]),
        tool("AGENT_WAIT", &format!("wait until a message from {} arrives (it follows this tool's result), the user writes, or `timeout` seconds pass (1 to {AGENT_WAIT_MAX})", if agent { "the main session or another agent" } else { "a subagent (a final reply counts)" }),
            json!({"timeout": {"type": "integer", "description": format!("For wait: seconds, 1 to {AGENT_WAIT_MAX}")}}), &["timeout"]),
        tool("AGENT_LIST", list, json!({}), &[]),
        tool("AGENT_SET", "switch agent `id` to another `model` and/or `effort` (the same choices as new), from its next request",
            json!({"id": {"type": "integer"}, "model": {"type": "string"}, "effort": {"type": "string"}}), &["id"]),
        tool("AGENT_PAUSE", "pause agent `id` after its current request: it does nothing (messages wait) until resume",
            json!({"id": {"type": "integer"}}), &["id"]),
        tool("AGENT_RESUME", "let paused agent `id` carry on", json!({"id": {"type": "integer"}}), &["id"]),
        tool("ACCOUNTS", "The OpenRouter credit, now: how much is left, of how much was bought, and spent through Oak since it started.",
            json!({}), &[]),
        tool("BROWSER_GOTO", "open `url`", json!({"url": {"type": "string"}}), &["url"]),
        tool("BROWSER_BACK", "go back", json!({}), &[]),
        tool("BROWSER_FORWARD", "go forward", json!({}), &[]),
        tool("BROWSER_RELOAD", "reload", json!({}), &[]),
        tool("BROWSER_READ", "the page as text, its interactive elements numbered [ref kind \"label\"]; `start` reads on in a long page",
            json!({"start": {"type": "integer"}}), &[]),
        tool("BROWSER_CLICK", "click element `ref` (from read) or the point `x`,`y`",
            json!({"ref": {"type": "integer"},
                   "x": {"type": "number"}, "y": {"type": "number"}}), &[]),
        tool("BROWSER_TYPE", "type `text` where the focus is, or into element `ref` (replacing its value; for a select, picks the option reading `text`)",
            json!({"text": {"type": "string"}, "ref": {"type": "integer"}}), &["text"]),
        tool("BROWSER_KEY", "press `key`, e.g. Enter, Escape, Tab, ArrowDown, PageDown, Control+a",
            json!({"key": {"type": "string"}}), &["key"]),
        tool("BROWSER_SCROLL", "scroll by `dy` pixels (default most of a screen down; negative is up), at `x`,`y` if given",
            json!({"dy": {"type": "number"}, "x": {"type": "number"}, "y": {"type": "number"}}), &[]),
        tool("BROWSER_SCREENSHOT", "see the viewport", json!({}), &[]),
        tool("BROWSER_EVAL", "run JavaScript `script` in the page; returns its value (promises awaited)",
            json!({"script": {"type": "string"}}), &["script"]),
        tool("BROWSER_DIALOG", "answer the page's alert, confirm or prompt: `accept` (default true), `text` for a prompt",
            json!({"accept": {"type": "boolean"}, "text": {"type": "string"}}), &[]),
        tool("BROWSER_UPLOAD", "set file input `ref` to the files at `paths` (relative to the working folder)",
            json!({"ref": {"type": "integer"}, "paths": {"type": "array", "items": {"type": "string"}}}), &["ref", "paths"]),
        tool("BROWSER_HELP", "this help", json!({}), &[]),
        // Its description and kits are the session's (`load_tool`).
        tool("LOAD", "", json!({"name": {"type": "string"}, "input": {"type": "object"}}), &["name"]),
        tool("AGENT_STOP", "stop agent `id` while it works: it goes idle without a final reply (a running command becomes one of its tasks) and keeps its conversation, so send can give it more to do",
            json!({"id": {"type": "integer"}}), &["id"]),
        tool("AGENT_DELETE", "delete agent `id` you no longer need: it stops, its tasks are killed and its conversation is removed, with no final reply",
            json!({"id": {"type": "integer"}}), &["id"]),
    ])
}

/// AGENT new's input: the name and prompt, and the model and effort to
/// pick from (the models this OAK can use, by provider, each with what it
/// suits).
fn agent_new_properties() -> Value {
    // The user's own llama.cpp server isn't offered: it serves one request
    // at a time, so an agent there would wait on the session that started it.
    let models: Vec<crate::api::Model> = crate::api::known_models().into_iter().filter(|m| m.provider != crate::api::Provider::Local).collect();
    let mut effort = "How hard the agent thinks; leave it out for your own. Lower is faster and cheaper.".to_string();
    for m in models.iter().filter_map(|m| crate::openrouter::model(&m.id)) {
        effort += &fewer_efforts(m.name, m.efforts);
    }
    effort += &ultra_note(&models);
    let mut props = json!({
        "name": {"type": "string", "description": "A short name for the agent, a few words (shown to the user)"},
        "prompt": {"type": "string"},
        "effort": {"type": "string", "enum": crate::agent::EFFORTS, "description": effort},
        "can_spawn": {"type": "boolean", "description": "Only from the main session: the agent may start, stop and delete agents of its own (a sub-orchestrator); theirs can't."},
    });
    if !models.is_empty() {
        props["model"] = json!({
            "type": "string",
            "enum": models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            "description": format!("Leave it out for your own model. {}", model_list(&models)),
        });
    }
    props
}

/// "a, b and c".
fn and(v: &[&str]) -> String {
    match v.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// " DeepSeek V4.1 Flash 1017: only low, high and max.": for a model with
/// fewer of the standard effort levels than Claude (`agent::effort_for`;
/// the others go up to the next), else "".
fn fewer_efforts(name: &str, levels: &[&str]) -> String {
    let have: Vec<&str> = levels.iter().copied().filter(|e| crate::agent::STANDARD_EFFORTS.contains(e)).collect();
    if crate::agent::STANDARD_EFFORTS.iter().all(|e| levels.contains(e)) {
        return String::new();
    }
    format!(" {name}: only {}.", and(&have))
}

/// " ultra: GPT-6 Astra and GPT-6.1 Sol only.": which of `models` have
/// ultra (the others take their highest), when any do.
fn ultra_note(models: &[crate::api::Model]) -> String {
    let names: Vec<&str> = models.iter().filter(|m| crate::agent::efforts(&m.id).contains(&"ultra")).map(|m| m.name.as_str()).collect();
    if names.is_empty() {
        return String::new();
    }
    format!(" ultra: {} only.", and(&names))
}

/// AGENT new's model list: the models by provider, each with what it
/// suits, e.g. "Claude: claude-opus-5-5 most capable, claude-sonnet-5-5
/// for most work. OpenAI, on the user's ChatGPT plan: gpt-6-astra most
/// capable."
fn model_list(models: &[crate::api::Model]) -> String {
    use crate::api::Provider;
    let mut groups: Vec<(Provider, Vec<String>)> = vec![];
    for m in models {
        let item = match model_note(m) {
            "" => m.id.clone(),
            note => format!("{} {note}", m.id),
        };
        match groups.iter_mut().find(|(p, _)| *p == m.provider) {
            Some((_, items)) => items.push(item),
            None => groups.push((m.provider, vec![item])),
        }
    }
    groups
        .iter()
        .map(|(p, items)| {
            let head = match p {
                Provider::OpenRouter => "Via OpenRouter, paid per token, far cheaper, for bulk or simple work",
                Provider::Codex => "OpenAI, on the user's ChatGPT plan",
                Provider::Freebuff => "Freebuff, billed in hours, a pipe with its own agent that can't start subagents",
                Provider::Local => "The user's own server",
                Provider::NodeClub => "NodeClub, an OpenAI-compatible API",
                Provider::Claude => "Claude",
            };
            format!("{head}: {}.", items.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a model suits, for AGENT new's model list (its provider is said
/// once, before its models).
fn model_note(m: &crate::api::Model) -> &'static str {
    let id = m.id.to_lowercase();
    if matches!(m.provider, crate::api::Provider::OpenRouter | crate::api::Provider::Freebuff | crate::api::Provider::Local | crate::api::Provider::NodeClub) {
        ""
    } else if id.contains("astra") || id.contains("opus") {
        "most capable"
    } else if id.contains("sol") || id.contains("sonnet") {
        "for most work"
    } else if id.contains("terra") {
        "lighter"
    } else if id.contains("luna") || id.contains("haiku") {
        "fastest"
    } else {
        ""
    }
}
