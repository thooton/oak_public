// Instruction files and project memory, as context the model reads with
// the conversation. CLAUDE.md loads from `~/.claude/` (the user's own,
// every project) and the working folder only (`CLAUDE.md`,
// `.claude/CLAUDE.md`, `CLAUDE.local.md`)—nothing from the folders above
// it or from subfolders. AGENTS.md (`AGENTS.md`, `.claude/AGENTS.md`)
// loads instead, only when none of those three is in the working folder;
// the user's `~/.claude/CLAUDE.md` doesn't count for that and loads
// anyway. `@path` imports are expanded (relative to the file, `~/` for
// home, up to four hops deep); block-level HTML comments are left out.
// Project memory: one `.md` per memory in `.oak/memory`, whose first
// line describes it; the context lists each file with that line.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Names in each directory, in the order they load.
#[cfg(windows)]
const CLAUDE_FILES: [&str; 3] = ["CLAUDE.md", ".claude\\CLAUDE.md", "CLAUDE.local.md"];
#[cfg(windows)]
const AGENTS_FILES: [&str; 2] = ["AGENTS.md", ".claude\\AGENTS.md"];
#[cfg(not(windows))]
const CLAUDE_FILES: [&str; 3] = ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"];
#[cfg(not(windows))]
const AGENTS_FILES: [&str; 2] = ["AGENTS.md", ".claude/AGENTS.md"];
/// Whether instruction files (CLAUDE.md, AGENTS.md and the user's
/// `~/.claude/CLAUDE.md`) are loaded. Off for now: sessions get none.
const INSTRUCTION_FILES: bool = false;
/// Whether project memory is on. Off for now: the context gets no memory
/// index (the system prompt's paragraph about it is commented out too).
const PROJECT_MEMORY: bool = false;
/// Where project memory lives, in the working folder.
#[cfg(windows)]
const MEMORY_DIR: &str = ".oak\\memory";
#[cfg(not(windows))]
const MEMORY_DIR: &str = ".oak/memory";
/// Import chains stop after this many hops.
const MAX_HOPS: usize = 4;
/// Files bigger than this aren't loaded (they would swamp the context).
const MAX_FILE: u64 = 200_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// `~/.claude/CLAUDE.md`.
    User,
    /// CLAUDE.md, .claude/CLAUDE.md or AGENTS.md.
    Project,
    /// CLAUDE.local.md.
    Local,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::User => "user's private global instructions for all projects",
            Kind::Project => "project instructions, checked into the codebase",
            Kind::Local => "user's private project instructions, not checked in",
        }
    }
}

/// An instruction file as it goes to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    pub path: PathBuf,
    pub kind: Kind,
    pub text: String,
}

/// The user's home folder.
pub fn user_home() -> PathBuf {
    let var = if cfg!(windows) { std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) } else { std::env::var_os("HOME") };
    var.map(PathBuf::from).unwrap_or_default()
}

/// `path` as typed, with a leading `~` (alone, or before `/`, or `\\` on
/// Windows) standing for `home`.
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    let sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with(sep) => home.join(rest.trim_start_matches(sep)),
        _ => PathBuf::from(path),
    }
}

/// Where `~/.claude/CLAUDE.md` is looked for (tests use an empty folder,
/// so the user's own file stays out of them).
pub fn home() -> PathBuf {
    if cfg!(test) { std::env::temp_dir().join("oak-test-home") } else { user_home() }
}

fn kind_of(name: &str) -> Kind {
    if name == "CLAUDE.local.md" { Kind::Local } else { Kind::Project }
}

/// The instruction files for a session in `folder`, in the order the
/// model reads them, each followed by what it imports. Imports leading
/// outside the working folder are only followed from the user's own file
/// (Claude Code asks first for those in a project's files; OAK leaves them
/// out); `skipped` gets them.
pub fn load(folder: &Path, home: &Path, skipped: &mut Vec<PathBuf>) -> Vec<File> {
    if !INSTRUCTION_FILES && !cfg!(test) {
        return vec![];
    }
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    let user = home.join(".claude").join("CLAUDE.md");
    add(&mut out, &mut seen, &user, Kind::User, folder, true, 0, skipped);
    // The working folder's own files only. With the home folder as the
    // working folder, its `.claude\CLAUDE.md` is the user's file: only that.
    let users = [user, user_home().join(".claude").join("CLAUDE.md")];
    let present = |names: &[&'static str]| -> Vec<&'static str> {
        names.iter().copied().filter(|n| folder.join(n).is_file() && !users.iter().any(|u| same(u, &folder.join(n)))).collect()
    };
    let claude = present(&CLAUDE_FILES);
    let names = if claude.is_empty() { present(&AGENTS_FILES) } else { claude };
    for n in names {
        add(&mut out, &mut seen, &folder.join(n), kind_of(n), folder, false, 0, skipped);
    }
    out
}

fn same(a: &Path, b: &Path) -> bool {
    crate::tools::path_key(a) == crate::tools::path_key(b)
}

/// Adds `path` (if it is there and not added yet), then what it imports.
#[allow(clippy::too_many_arguments)]
fn add(out: &mut Vec<File>, seen: &mut BTreeSet<String>, path: &Path, kind: Kind, folder: &Path, trusted: bool, hops: usize, skipped: &mut Vec<PathBuf>) {
    let key = crate::tools::path_key(path);
    if seen.contains(&key) || !path.is_file() || path.metadata().is_ok_and(|m| m.len() > MAX_FILE) {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    seen.insert(key);
    let text = strip_comments(&raw);
    let imports = imports(&text);
    out.push(File { path: path.to_path_buf(), kind, text: text.trim().to_string() });
    if hops >= MAX_HOPS {
        return;
    }
    let base = path.parent().unwrap_or(Path::new(""));
    for i in imports {
        let target = resolve_import(&i, base);
        if !trusted && !target.starts_with(folder) {
            if target.is_file() && !skipped.contains(&target) {
                skipped.push(target);
            }
            continue;
        }
        add(out, seen, &target, kind, folder, trusted, hops + 1, skipped);
    }
}

fn resolve_import(p: &str, base: &Path) -> PathBuf {
    let path = expand_home(&crate::tools::native_seps(p), &user_home());
    let path = if path.is_absolute() { path } else { base.join(path) };
    // Tidy `a\.\b` and `a\x\..\b` so the same file is recognised.
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether `line` opens or closes a fenced code block.
pub fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Leaves out block-level `<!-- comments -->` (ones starting a line),
/// except in fenced code blocks.
fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    // What follows a comment's end on its line stays.
    let after = |out: &mut String, rest: &str| {
        if !rest.trim().is_empty() {
            *out += rest;
            out.push('\n');
        }
    };
    let mut fence = false;
    let mut in_comment = false;
    for line in text.lines() {
        let t = line.trim_start();
        if in_comment {
            if let Some(i) = line.find("-->") {
                in_comment = false;
                after(&mut out, &line[i + 3..]);
            }
            continue;
        }
        if is_fence(line) {
            fence = !fence;
        } else if !fence && t.starts_with("<!--") {
            match t.find("-->") {
                Some(i) => after(&mut out, &t[i + 3..]),
                None => in_comment = true,
            }
            continue;
        }
        out += line;
        out.push('\n');
    }
    out
}

/// The `@path` imports in a file: an `@` at the start or after whitespace,
/// up to the next whitespace (a backslash before a space keeps it), not
/// in code spans or fenced code blocks.
fn imports(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut fence = false;
    for line in text.lines() {
        if is_fence(line) {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut code = false;
        while i < chars.len() {
            let ch = chars[i];
            if ch == '`' {
                code = !code;
            } else if ch == '@' && !code && (i == 0 || chars[i - 1].is_whitespace()) {
                let mut p = String::new();
                let mut j = i + 1;
                while j < chars.len() && !chars[j].is_whitespace() {
                    if chars[j] == '\\' && chars.get(j + 1) == Some(&' ') {
                        p.push(' ');
                        j += 2;
                        continue;
                    }
                    p.push(chars[j]);
                    j += 1;
                }
                // A path ends before trailing punctuation of the sentence.
                let p = p.trim_end_matches([',', ';', ')', '.', ':', '!', '?']).to_string();
                if !p.is_empty() && !p.starts_with(['"', '\'']) && !p.contains('@') {
                    out.push(p);
                }
                i = j;
                continue;
            }
            i += 1;
        }
    }
    out
}

/// The memory index: every `.md` file in `.oak/memory` with its first
/// line (the description). Empty when there are none.
pub fn memory_index(folder: &Path) -> Vec<(String, String)> {
    if !PROJECT_MEMORY && !cfg!(test) {
        return vec![];
    }
    let Ok(dir) = std::fs::read_dir(folder.join(MEMORY_DIR)) else { return vec![] };
    let mut out: Vec<(String, String)> = dir
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !name.to_lowercase().ends_with(".md") {
                return None;
            }
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            let description = first.trim_start_matches('#').trim().to_string();
            Some((name, description))
        })
        .collect();
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out
}

/// "Contents of P (label):" and the text, for each file.
fn render_files(files: &[File]) -> String {
    files
        .iter()
        .map(|f| format!("Contents of {} ({}):\n\n{}", f.path.display(), f.kind.label(), f.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The context block for the start of the conversation, worded as Claude
/// Code words it; empty with no instruction files and no memories.
pub fn render(files: &[File], memory: &[(String, String)]) -> String {
    if files.is_empty() && memory.is_empty() {
        return String::new();
    }
    let mut s = String::from("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n");
    if !files.is_empty() {
        s += "# claudeMd\nCodebase and user instructions are shown below. Be sure to adhere to these instructions. \
              IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.\n\n";
        s += &render_files(files);
        s += "\n";
    }
    if !memory.is_empty() {
        s += &format!("# projectMemory ({MEMORY_DIR})\n");
        for (name, description) in memory {
            s += &format!("- {name}: {description}\n");
        }
    }
    s += "\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context \
          unless it is highly relevant to your task.\n</system-reminder>\n";
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_folders_expand_home() {
        let home = Path::new("/home/me");
        assert_eq!(expand_home("~", home), PathBuf::from("/home/me"));
        assert_eq!(expand_home("~/projx", home), PathBuf::from("/home/me/projx"));
        assert_eq!(expand_home("~//a/b", home), PathBuf::from("/home/me/a/b"));
        assert_eq!(expand_home("~other/x", home), PathBuf::from("~other/x"));
        assert_eq!(expand_home("/srv/~/x", home), PathBuf::from("/srv/~/x"));
    }

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("oak-instructions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn put(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Only the user's file and the working folder's own files load: not
    /// the folders above it, nor subfolders; CLAUDE.local.md after the
    /// others; AGENTS.md only when there are none.
    #[test]
    fn loads_only_the_working_folder() {
        let root = tree("loads");
        let (home, work) = (root.join("home"), root.join("a").join("b"));
        std::fs::create_dir_all(&work).unwrap();
        put(&home.join(".claude").join("CLAUDE.md"), "user rules @extra.md");
        put(&home.join(".claude").join("extra.md"), "user extra");
        put(&root.join("a").join("CLAUDE.md"), "a rules");
        put(&root.join("a").join("AGENTS.md"), "a agents");
        put(&work.join("tmp").join("CLAUDE.md"), "sub rules");
        put(&work.join("CLAUDE.local.md"), "<!-- note for people -->\nlocal rules");
        put(&work.join(".claude").join("CLAUDE.md"), "b rules @docs/x.md @../../../home/secret.md");
        // Relative to the importing file.
        put(&work.join(".claude").join("docs").join("x.md"), "x @y.md");
        put(&work.join(".claude").join("docs").join("y.md"), "y @x.md"); // a loop
        put(&home.join("secret.md"), "secret");
        let mut skipped = vec![];
        let files = load(&work, &home, &mut skipped);
        let got: Vec<(String, Kind, &str)> = files
            .iter()
            .map(|f| (f.path.strip_prefix(&root).unwrap().display().to_string().replace('\\', "/"), f.kind, f.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("home/.claude/CLAUDE.md".to_string(), Kind::User, "user rules @extra.md"),
                ("home/.claude/extra.md".into(), Kind::User, "user extra"),
                ("a/b/.claude/CLAUDE.md".into(), Kind::Project, "b rules @docs/x.md @../../../home/secret.md"),
                ("a/b/.claude/docs/x.md".into(), Kind::Project, "x @y.md"),
                ("a/b/.claude/docs/y.md".into(), Kind::Project, "y @x.md"),
                ("a/b/CLAUDE.local.md".into(), Kind::Local, "local rules"),
            ][..]
        );
        // The import outside the working folder, from a project file, waits.
        assert_eq!(skipped, [home.join("secret.md")]);

        // No CLAUDE.md files in the working folder: the parent's CLAUDE.md
        // and AGENTS.md stay out, and so does the subfolder's.
        std::fs::remove_file(work.join("CLAUDE.local.md")).unwrap();
        std::fs::remove_dir_all(work.join(".claude")).unwrap();
        let files = load(&work, &home, &mut vec![]);
        let names: Vec<String> = files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["CLAUDE.md", "extra.md"]);

        // Only AGENTS.md in the working folder: it loads instead.
        put(&work.join("AGENTS.md"), "b agents");
        let files = load(&work, &home, &mut vec![]);
        let names: Vec<String> = files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["CLAUDE.md", "extra.md", "AGENTS.md"]);
        let block = render(&files, &[]);
        assert!(block.starts_with("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\n"));
        assert!(block.contains(&format!("Contents of {} (project instructions, checked into the codebase):\n\nb agents", work.join("AGENTS.md").display())));
        assert!(block.contains("(user's private global instructions for all projects):\n\nuser rules"));
        assert!(block.trim_end().ends_with("</system-reminder>"));
        assert_eq!(render(&[], &[]), "");
        let _ = std::fs::remove_dir_all(&root);
    }
}
