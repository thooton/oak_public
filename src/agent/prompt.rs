//! The system prompt (a role's prompt around the shared tool instructions,
//! built in and filled in for the session) and the session's scratchpad
//! folder.

use super::*;

/// A system prompt the user can start a conversation with (the New
/// screen's "System prompt"): who the model is and how it works with the
/// user. The tool instructions (prompts/tools.md) are the same for all.
pub(crate) struct Role {
    /// Kept in the conversation (`Conversation::role`) and the settings.
    pub(crate) id: &'static str,
    /// The file's name with the first letter capitalized.
    pub(crate) name: &'static str,
    /// prompts/<id>.md: `{{tools}}` marks where the tool instructions go.
    text: &'static str,
    /// A Nerd Font symbol for the tabs of its conversations (a plugin's).
    pub(crate) icon: &'static str,
    /// The New screen offers it (a plugin's may be for its own
    /// conversations only).
    pub(crate) offered: bool,
}

// `ROLES`: every file in prompts/ but tools.md, bundled into the binary
// when OAK is built (build.rs). The first (Normal) is the default, and
// stands in for an unknown id (a role since removed, one whose plugin isn't
// loaded, or none in conversations from before roles). To add one, put its
// file in prompts/, or have a plugin register it (`oak_sdk::Role`).
include!(concat!(env!("OUT_DIR"), "/roles.rs"));

/// The roles plugins added (`add_roles`).
static EXTRA: std::sync::OnceLock<Vec<Role>> = std::sync::OnceLock::new();

/// Takes the roles plugins registered, once, when they were loaded. One
/// with a built-in role's id (or another plugin's) is left out.
pub(crate) fn add_roles(roles: &[oak_sdk::Role]) {
    let keep = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
    let mut extra: Vec<Role> = vec![];
    for r in roles {
        if r.id.is_empty() || ROLES.iter().chain(&extra).any(|x| x.id == r.id) {
            continue;
        }
        extra.push(Role { id: keep(&r.id), name: keep(if r.name.is_empty() { &r.id } else { &r.name }), text: keep(&r.text), icon: keep(&r.icon), offered: r.offered });
    }
    let _ = EXTRA.set(extra);
}

/// Every role: the built-in ones, then the plugins'.
fn all() -> impl Iterator<Item = &'static Role> {
    ROLES.iter().chain(EXTRA.get().into_iter().flatten())
}

/// The roles the New screen offers.
pub(crate) fn offered() -> impl Iterator<Item = &'static Role> {
    all().filter(|r| r.offered)
}

/// The role with this id (the default for an unknown or empty one).
pub(crate) fn role(id: &str) -> &'static Role {
    all().find(|r| r.id == id).unwrap_or(&ROLES[0])
}

/// The tool instructions every role's prompt has.
const TOOLS: &str = include_str!("../../prompts/tools.md");

/// The system prompt: the role's prompt (`role`, an id in `ROLES`) with the
/// tool instructions in it, both bundled into the binary (changing them
/// takes a rebuild). `agent` names a subagent; `model` is the conversation's
/// model id, which the prompt names. A model on the user's own server
/// (`local::serves`) gets only the `{{#local}}` blocks (the tools it has),
/// and `{{#goal}}` only with a goal. Every kit loaded.
#[cfg(test)]
pub(crate) fn system_prompt(folder: &Path, scratch: &Path, model: &str, agent: Option<&str>, goal: bool, role: &str) -> String {
    let mut kits: Vec<String> = ["AGENT", "WINDOW"].map(String::from).to_vec();
    if goal {
        kits.push("GOAL".into());
    }
    system_prompt_on(&crate::time::today_utc(), folder, scratch, model, agent, &kits, role)
}

/// `system_prompt` dated `day`, with `kits` loaded (LOAD; a local model's
/// GOAL with a goal): a kit's block (`{{#window}}`, `{{#agents}}`,
/// `{{#goal}}`) only once it is. A conversation keeps the date and kits its
/// cache was written with (`prefix::Pin`).
pub(crate) fn system_prompt_on(day: &str, folder: &Path, scratch: &Path, model: &str, agent: Option<&str>, kits: &[String], role: &str) -> String {
    let has = |k: &str| kits.iter().any(|x| x == k);
    let local = crate::local::serves(model);
    let text = with_tools(self::role(role).text, TOOLS);
    let text = keep_blocks(&text, &[("full", !local), ("local", local), ("goal", !local || has("GOAL")), ("window", has("WINDOW")), ("agents", has("AGENT"))]);
    fill_prompt(&text, folder, scratch, day, &crate::api::model_name(model), agent)
}

/// A kit's block of the prompt (`{{#window}}` for WINDOW, `{{#agents}}`
/// for AGENT), filled in: what LOAD returns of it before it joins the
/// system prompt.
pub(crate) fn kit_prompt(kit: &str, agent: Option<&str>) -> String {
    let block = match kit {
        "WINDOW" => "window",
        "AGENT" => "agents",
        _ => return String::new(),
    };
    let text = strip_comments(TOOLS);
    let (open, close) = (format!("{{{{#{block}}}}}"), format!("{{{{/{block}}}}}"));
    let Some(inner) = text.split_once(&open).and_then(|(_, rest)| rest.split_once(&close)).map(|(inner, _)| inner) else { return String::new() };
    fill_prompt(inner, Path::new(""), Path::new(""), "", "", agent)
}

/// A role's prompt with the tool instructions where it has `{{tools}}`, or
/// after it. The role's comments go first, so a `{{tools}}` mentioned in
/// one isn't taken for the placeholder.
fn with_tools(role: &str, tools: &str) -> String {
    let role = strip_comments(role);
    let tools = strip_comments(tools);
    let tools = tools.trim();
    if role.contains("{{tools}}") {
        role.replacen("{{tools}}", tools, 1)
    } else {
        format!("{}\n\n{tools}", role.trim_end())
    }
}

/// The text without its `<!-- comments -->`.
fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        rest = rest[i..].find("-->").map_or("", |j| &rest[i + j + 3..]);
    }
    out.push_str(rest);
    out
}

/// Leaves out `<!-- comments -->` and fills in {{folder}}, {{scratch}},
/// {{date}}, {{model}}, {{os}} and {{agent}}, keeping the platform, main or
/// agent blocks that apply. The blank lines left where blocks were dropped
/// become one.
pub(crate) fn fill_prompt(text: &str, folder: &Path, scratch: &Path, date: &str, model: &str, agent: Option<&str>) -> String {
    let out = strip_comments(text);
    let out = keep_blocks(&out, &[("windows", cfg!(windows)), ("unix", !cfg!(windows)), ("main", agent.is_none()), ("agent", agent.is_some())]);
    let mut out = out.replace("\r\n", "\n");
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim()
        .replace("{{folder}}", &folder.display().to_string())
        .replace("{{scratch}}", &scratch.display().to_string())
        .replace("{{date}}", date)
        .replace("{{model}}", model)
        .replace("{{os}}", os_name())
        .replace("{{agent}}", agent.unwrap_or(""))
}

/// The name of the system OAK runs on, for the prompt.
fn os_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

/// Keeps the text of each `{{#name}}...{{/name}}` block whose name is kept,
/// and drops the blocks of the others.
fn keep_blocks(text: &str, blocks: &[(&str, bool)]) -> String {
    let mut out = text.to_string();
    for &(name, keep) in blocks {
        let (open, close) = (format!("{{{{#{name}}}}}"), format!("{{{{/{name}}}}}"));
        let mut s = String::new();
        let mut rest = out.as_str();
        while let Some(i) = rest.find(&open) {
            s.push_str(&rest[..i]);
            let body = &rest[i + open.len()..];
            let (inner, after) = match body.find(&close) {
                Some(j) => (&body[..j], &body[j + close.len()..]),
                None => (body, ""),
            };
            if keep {
                s.push_str(inner);
            }
            rest = after;
        }
        s.push_str(rest);
        out = s;
    }
    out
}

/// Where sessions' scratchpads go, in the working folder.
#[cfg(windows)]
pub(crate) const SCRATCH_ROOT: &str = ".oak\\tmp";
#[cfg(not(windows))]
pub(crate) const SCRATCH_ROOT: &str = ".oak/tmp";

/// The session's scratchpad: `.oak\tmp\tempN` in the working folder, made
/// the first time it is needed (N one past the highest there, from 0) and
/// made again if it was deleted. `.oak\.gitignore` keeps `tmp` out of git.
/// If the working folder can't hold it, a folder under the system's temp
/// folder stands in.
pub(crate) fn scratchpad(c: &mut Conversation) -> PathBuf {
    let root = c.folder.join(SCRATCH_ROOT);
    let made = if c.scratch.is_empty() {
        new_scratch(&root).map(|name| {
            c.scratch = name;
            c.save();
        })
    } else {
        std::fs::create_dir_all(root.join(&c.scratch))
    };
    if made.is_err() {
        let fallback = std::env::temp_dir().join("oak").join(&c.id);
        let _ = std::fs::create_dir_all(&fallback);
        return fallback;
    }
    let ignore = root.parent().unwrap().join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(ignore, "# Oak's session scratchpads\ntmp/\n");
    }
    root.join(&c.scratch)
}

/// Makes the next `tempN` folder in `root`: N is one past the highest
/// there (0 when there are none). Two sessions starting at once get
/// different folders, as making one that exists fails.
fn new_scratch(root: &Path) -> std::io::Result<String> {
    std::fs::create_dir_all(root)?;
    let mut n = std::fs::read_dir(root)?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("temp")?.parse::<u64>().ok().map(|n| n + 1))
        .max()
        .unwrap_or(0);
    loop {
        let name = format!("temp{n}");
        match std::fs::create_dir(root.join(&name)) {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each role's prompt has the tool instructions in it, filled in, with
    /// no placeholder, comment or run of blank lines left.
    #[test]
    fn roles_get_the_tool_instructions() {
        assert_eq!(role("").id, ROLES[0].id);
        assert_eq!(role("no such role").id, ROLES[0].id);
        // A plugin's role: looked up by its id, offered only if it says
        // so, and never in a built-in one's place.
        add_roles(&[
            oak_sdk::Role { id: "coordinator".into(), name: "Coordinator".into(), text: "You coordinate.\n\n{{tools}}\n\nRespect the user.".into(), offered: false, icon: "x".into() },
            oak_sdk::Role { id: "normal".into(), name: "Impostor".into(), text: "No.".into(), offered: true, icon: String::new() },
        ]);
        assert_eq!((role("coordinator").name, role("coordinator").icon), ("Coordinator", "x"));
        assert_eq!(role("normal").name, "Normal");
        assert!(offered().all(|r| r.id != "coordinator") && offered().count() == ROLES.len());
        let p = system_prompt(Path::new("F"), Path::new("S"), "claude-opus-5-5", None, false, "coordinator");
        assert!(p.starts_with("You coordinate.") && p.contains("Every EXECUTE and TASK command starts in a fresh shell") && !p.contains("{{"));
        for r in ROLES.iter() {
            for agent in [None, Some("agent 1 (x)")] {
                for model in ["claude-opus-5-5", "local/qwen"] {
                    let p = system_prompt(Path::new("F"), Path::new("S"), model, agent, false, r.id);
                    assert!(p.contains("Every EXECUTE and TASK command starts in a fresh shell"), "{}", r.id);
                    assert!(p.contains("Working folder: F"), "{}", r.id);
                    assert!(p.contains("never condescend or overexplain"), "{}", r.id);
                    assert!(p.contains("Never use bullets, middle dots, or em dashes as separators"), "{}", r.id);
                    assert!(!p.contains("{{") && !p.contains("<!--") && !p.contains("\n\n\n"), "{}: {p}", r.id);
                    assert_eq!(p.contains("You are a subagent"), agent.is_some());
                }
            }
        }
        let p = system_prompt(Path::new("F"), Path::new("S"), "claude-opus-5-5", None, true, "normal");
        let (tools, user) = (p.find("Every EXECUTE").unwrap(), p.find("Respect the user").unwrap());
        assert!(p.starts_with("You are Oak, a coding agent") && tools < user && p.contains("Goals: the user"));
    }

    /// A kit's block is in the prompt only once it is loaded; LOAD returns
    /// it until then.
    #[test]
    fn kits_add_their_blocks() {
        let p = |kits: &[&str]| system_prompt_on("2026-11-06", Path::new("F"), Path::new("S"), "claude-opus-5-5", None, &kits.iter().map(|k| k.to_string()).collect::<Vec<_>>(), "normal");
        let (none, all) = (p(&[]), p(&["WINDOW", "AGENT"]));
        assert!(!none.contains("GUI app") && !none.contains("Subagents (AGENT) suit") && none.contains("Goals: the user"));
        assert!(all.contains("GUI app") && all.contains("Subagents (AGENT) suit"));
        assert!(!none.contains("\n\n\n") && !none.contains("{{"), "{none}");
        let w = kit_prompt("WINDOW", None);
        assert!(w.contains("GUI app") && !w.contains("{{") && all.contains(&w), "{w}");
        assert!(kit_prompt("AGENT", None).starts_with("Subagents (AGENT) suit"));
    }

    /// A role without `{{tools}}` gets them after its text.
    #[test]
    fn tools_go_after_a_role_without_the_placeholder() {
        assert_eq!(with_tools("Be brief.\n", "<!-- c -->\nTOOLS\n"), "Be brief.\n\nTOOLS");
        assert_eq!(with_tools("A\n{{tools}}\nB", "TOOLS"), "A\nTOOLS\nB");
        // A `{{tools}}` named in a comment isn't the placeholder.
        assert_eq!(with_tools("<!-- goes where {{tools}} is -->\nA\n{{tools}}", "TOOLS"), "\nA\nTOOLS");
    }
}
