// A quick check of EXECUTE and TASK commands for likely mistakes that are
// hard to undo: a recursive rm of /, a top-level directory (`C:/Windows`
// too), the home folder or a folder in it, the working folder (or one of
// its parents) or a repository's .git; find -delete, chmod -R and rsync
// --delete there; git commands that throw away work (reset --hard, clean
// -f, force push, checkout/restore of everything, reflog expiry); writes
// to disks, formatting, shutdown, fork bombs, killing every process or
// Oak itself by name. Inspired by opencode-bash-classifier and Claude
// Code's critical-path rm check. It isn't a sandbox and doesn't try to be:
// it catches accidents, and a blocked command tells the model how to reach
// its goal another way (a narrower, reversible command), not to get the
// same command past the check. Words come from the sleeps tokenizer
// (quotes, comments and heredoc bodies aren't commands); `bash -c`,
// `eval`, `powershell -Command`, `cmd /c` and prefixes like sudo, env,
// timeout and xargs are looked into. Windows paths match whatever their
// case and in MSYS form (`/c/Users/me`).

use crate::sleeps::simple_commands;
use std::path::Path;

/// What is wrong with a command, and what to do instead.
struct Danger {
    why: String,
    instead: &'static str,
}

impl Danger {
    fn new(why: impl Into<String>, instead: &'static str) -> Self {
        Danger { why: why.into(), instead }
    }
}

// What to do instead, by kind of command.
const RM: &str = "remove only the files or folders you mean, each by its own path (the folder inside, not its parent), or move them aside (`mv x x.bak`) rather than deleting them.";
const GIT_DIR: &str = "remove only what you mean inside it; if the history really has to go, move it aside (`mv .git .git.bak`).";
const GIT_DISCARD: &str = "keep the work first (`git stash` or a commit), or restore just the files you mean, by name (`git checkout -- path/to/file`).";
const GIT_CLEAN: &str = "list what it would delete with `git clean -n`, then delete the files you mean by name.";
const FORCE_PUSH: &str = "push without rewriting the remote's history, or use `--force-with-lease` on a branch only you use.";
const STASH: &str = "drop just the stashes you mean (`git stash drop stash@{N}`).";
const REFLOG: &str = "leave the reflog alone: it is what lets lost commits be recovered.";
const DISK: &str = "work on files, not on disks or partitions: anything at the device level is for the user to do.";
const POWER: &str = "leave the computer running: the user stops it.";
const FORK: &str = "leave it out.";
const KILL: &str = "stop only the processes you started, by their PIDs (a background task with TASK delete). Oak itself runs as oak.exe: killing processes by that name closes the user's Oak too.";

/// Err with the message for the model when the command looks destructive.
pub fn check(command: &str, folder: &Path) -> Result<(), String> {
    let folder = parts(&folder.display().to_string(), &[]);
    let home = dirs::home_dir().map(|h| parts(&h.display().to_string(), &[])).unwrap_or_default();
    match find(command, &folder, &home, 0) {
        None => Ok(()),
        Some((d, text)) => Err(format!(
            "Blocked by Oak's command guard: {} (`{}`). This would be hard or impossible to undo, so don't look for another way to run the same \
             thing that this check misses. Work around it: {} If nothing narrower does the job, stop and ask the user to run it themselves.",
            d.why,
            crate::tools::ellipsize(&text, 200),
            d.instead
        )),
    }
}

/// The command without its heredocs' bodies (text, not commands: a file
/// being written may well hold a fork bomb's characters).
fn without_heredocs(s: &str) -> String {
    let mut out = String::new();
    let mut lines = s.split('\n');
    while let Some(line) = lines.next() {
        out.push_str(line);
        out.push('\n');
        let mut delims = vec![];
        let mut rest = line;
        while let Some(i) = rest.find("<<") {
            let after = &rest[i + 2..];
            if let Some(here_string) = after.strip_prefix('<') {
                rest = here_string;
                continue;
            }
            let tabs = after.starts_with('-');
            let after = after.trim_start_matches('-').trim_start();
            let d: String = after.chars().take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | ')' | '<' | '>')).collect();
            let d = d.replace(['\'', '"', '\\'], "");
            if !d.is_empty() {
                delims.push((d, tabs));
            }
            rest = after;
        }
        for (d, tabs) in delims {
            for l in lines.by_ref() {
                let l = if tabs { l.trim_start_matches('\t') } else { l };
                if l.trim_end_matches('\r') == d {
                    break;
                }
            }
        }
    }
    out
}

/// The first problem in `command`, and the simple command it is in.
fn find(command: &str, folder: &[String], home: &[String], depth: usize) -> Option<(Danger, String)> {
    if depth > 3 {
        return None;
    }
    // Freebuff runs commands as `bash -c "$(cat <<'__FREEBUFF_COMMAND__' ... )"`.
    const OPEN: &str = "<<'__FREEBUFF_COMMAND__'\n";
    if let Some(i) = command.find(OPEN) {
        let body = &command[i + OPEN.len()..];
        let body = body.find("\n__FREEBUFF_COMMAND__").map_or(body, |e| &body[..e]);
        if let Some(f) = find(body, folder, home, depth + 1) {
            return Some(f);
        }
    }
    let compact: String = without_heredocs(command).chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if compact.contains(":(){:|:&};:") {
        return Some((Danger::new("a fork bomb", FORK), command.trim().into()));
    }
    for dev in [">/dev/sd", ">/dev/nvme", ">/dev/hd", ">/dev/vd", ">/dev/mmcblk", ">/dev/disk", r">\\.\physicaldrive", ">//./physicaldrive"] {
        if compact.contains(dev) {
            return Some((Danger::new("it writes straight to a disk device", DISK), command.trim().into()));
        }
    }
    for c in simple_commands(command) {
        if let Some(d) = simple(&c.words, folder, home, depth) {
            return Some((d, c.text));
        }
    }
    None
}

/// The program's name without its directory or `.exe`, in lower case
/// (PowerShell's and cmd's commands ignore case).
fn name(w: &str) -> String {
    let w = w.rsplit(['/', '\\']).next().unwrap_or(w).to_ascii_lowercase();
    w.strip_suffix(".exe").map(str::to_string).unwrap_or(w)
}

/// A cmd-style switch (`/s`, or `//s` as MSYS passes it), in lower case.
fn switch(a: &str) -> Option<String> {
    let s = a.strip_prefix("//").or(a.strip_prefix('/'))?;
    (!s.is_empty() && s.len() <= 3 && s.chars().all(|c| c.is_ascii_alphabetic() || c == '?')).then(|| s.to_ascii_lowercase())
}

/// A process name that is Oak's (as `taskkill /IM`, `pkill` or
/// `Stop-Process -Name` take it).
fn is_oak(a: &str) -> bool {
    let a = a.trim_start_matches('^').trim_end_matches('$').to_ascii_lowercase();
    matches!(a.strip_suffix(".exe").unwrap_or(&a), "oak" | "oak*")
}

/// The script a shell runs given `args`: the words after the first of
/// `flags` (`-c`, or `/c` and `//c` for `-c`).
fn script<'a>(args: &[&'a str], flags: &[&str]) -> Option<Vec<&'a str>> {
    let i = args.iter().position(|a| {
        let a = a.to_ascii_lowercase();
        flags.iter().any(|f| a == *f || a.strip_prefix("//").or(a.strip_prefix('/')).is_some_and(|s| s == &f[1..]))
    })?;
    Some(args[i + 1..].to_vec())
}

/// `words` without the prefixes that run the rest as a command (`sudo -u
/// root`, `env X=1`, `timeout 5`, …).
fn without_prefixes(words: &[String]) -> &[String] {
    let mut w = words;
    loop {
        let Some(first) = w.first() else { return w };
        match name(first).as_str() {
            p @ ("sudo" | "doas" | "env" | "command" | "builtin" | "exec" | "nohup" | "nice" | "time" | "xargs" | "stdbuf") => {
                // Options that take the next word as their value (`sudo -u root`).
                let valued: &[&str] = match p {
                    "sudo" => &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U", "-T", "--user", "--group", "--chdir", "--host", "--prompt", "--role", "--type", "--other-user", "--close-from", "--command-timeout"],
                    "doas" => &["-u", "-C"],
                    "env" => &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
                    "nice" => &["-n", "--adjustment"],
                    "xargs" => &["-I", "-i", "-n", "-P", "-d", "-E", "-L", "-l", "-s", "-a", "--max-args", "--max-procs", "--delimiter", "--arg-file", "--max-lines", "--max-chars", "--replace", "--eof"],
                    "stdbuf" => &["-i", "-o", "-e", "--input", "--output", "--error"],
                    "exec" => &["-a"],
                    _ => &[],
                };
                w = &w[1..];
                while let Some(a) = w.first().filter(|a| a.starts_with('-') || a.contains('=')) {
                    w = &w[if valued.contains(&a.as_str()) { 2 } else { 1 }.min(w.len())..];
                }
            }
            "timeout" => {
                w = &w[1..];
                while w.first().is_some_and(|a| a.starts_with('-')) {
                    w = &w[1..];
                }
                w = w.get(1..).unwrap_or(&[]);
            }
            _ => return w,
        }
    }
}

fn simple(words: &[String], folder: &[String], home: &[String], depth: usize) -> Option<Danger> {
    let w = without_prefixes(words);
    let (cmd, args) = w.split_first()?;
    let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let flag = |c: char| args.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(c));
    let long = |l: &str| args.contains(&l);
    let operands = || {
        let mut out = Vec::new();
        let mut after = false;
        for a in &args {
            if !after && *a == "--" {
                after = true;
            } else if after || !a.starts_with('-') {
                out.push(*a);
            }
        }
        out
    };
    let critical = |t: &str, with_folder: bool| critical(t, folder, home, with_folder);
    // A recursive removal of `targets`: of a .git, or of a critical folder.
    let removal = |targets: Vec<&str>| {
        let git_dir = |t: &str| t.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().is_some_and(|n| n.eq_ignore_ascii_case(".git"));
        if let Some(t) = targets.iter().find(|t| git_dir(t)) {
            return Some(Danger::new(format!("a recursive rm of {t} (the repository's history)"), GIT_DIR));
        }
        targets.into_iter().find_map(|t| critical(t, true)).map(|w| Danger::new(format!("a recursive rm of {w}"), RM))
    };
    // Windows' own shells: backslashes are path separators there, not escapes.
    let windows_script = |s: Vec<&str>| find(&s.join(" ").replace('\\', "/"), folder, home, depth + 1).map(|(d, _)| d);
    match name(cmd).as_str() {
        "bash" | "sh" | "zsh" | "dash" | "ksh" => {
            let i = args.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))?;
            find(args.get(i + 1)?, folder, home, depth + 1).map(|(d, _)| d)
        }
        "eval" => find(&args.join(" "), folder, home, depth + 1).map(|(d, _)| d),
        "powershell" | "pwsh" => windows_script(script(&args, &["-c", "-command"])?),
        "cmd" => windows_script(script(&args, &["/c", "/k"])?),
        "rm" if flag('r') || flag('R') || long("--recursive") || args.iter().any(|a| a.eq_ignore_ascii_case("-recurse")) => {
            if long("--no-preserve-root") {
                return Some(Danger::new("rm --no-preserve-root", RM));
            }
            removal(operands())
        }
        // PowerShell's (and its aliases') and cmd's recursive removals.
        "remove-item" | "ri" | "del" | "erase" | "rd" | "rmdir" => {
            let lower: Vec<String> = args.iter().map(|a| a.to_ascii_lowercase()).collect();
            let recursive = lower.iter().any(|a| (a.len() >= 2 && "-recurse".starts_with(a.as_str())) || switch(a).as_deref() == Some("s"));
            if !recursive || lower.iter().any(|a| a == "-whatif") {
                return None;
            }
            removal(args.iter().copied().filter(|a| !a.starts_with('-') && switch(a).is_none()).collect())
        }
        "find" if args.iter().any(|a| matches!(*a, "-delete" | "-exec" | "-execdir" | "-ok"))
            && (args.contains(&"-delete") || args.iter().any(|a| matches!(name(a).as_str(), "rm" | "shred"))) =>
        {
            // Without a test that picks files, all of them go (the working
            // folder's too).
            const PICKS: [&str; 18] = [
                "-name", "-iname", "-path", "-ipath", "-wholename", "-regex", "-iregex", "-newer", "-mtime", "-mmin", "-atime", "-amin", "-ctime", "-cmin", "-size",
                "-empty", "-user", "-perm",
            ];
            let all = !args.iter().any(|a| PICKS.contains(a));
            args.iter()
                .take_while(|a| !a.starts_with('-') && !matches!(**a, "(" | "!"))
                .find_map(|t| critical(t, all))
                .map(|w| Danger::new(format!("a find that deletes under {w}"), RM))
        }
        "chmod" | "chown" | "chgrp" if flag('R') || long("--recursive") => operands()
            .into_iter()
            .skip(1)
            .find_map(|t| critical(t, false))
            .map(|w| Danger::new(format!("a recursive {} of {w}", name(cmd)), RM)),
        "rsync" if args.iter().any(|a| a.starts_with("--delete")) => {
            critical(operands().last()?, true).map(|w| Danger::new(format!("an rsync --delete onto {w}"), RM))
        }
        "dd" => {
            let of = args.iter().find_map(|a| a.strip_prefix("of=/dev/"))?;
            (!matches!(of, "null" | "stdout" | "stderr" | "tty" | "zero")).then(|| Danger::new(format!("dd onto the device /dev/{of}"), DISK))
        }
        "shred" if args.iter().any(|a| a.starts_with("/dev/")) => Some(Danger::new("shred of a device", DISK)),
        n if n.starts_with("mkfs") || n == "wipefs" => Some(Danger::new(format!("{n} formats or wipes a filesystem"), DISK)),
        "format" if args.iter().any(|a| is_drive(a)) => Some(Danger::new("format of a drive", DISK)),
        n @ ("diskpart" | "format-volume" | "clear-disk" | "initialize-disk" | "remove-partition") => Some(Danger::new(format!("{n} works on disks"), DISK)),
        // (`shutdown /a` calls one off.)
        n @ ("shutdown" | "reboot" | "poweroff" | "halt" | "stop-computer" | "restart-computer") => {
            (!args.iter().any(|a| switch(a).as_deref() == Some("a"))).then(|| Danger::new(format!("{n} stops the computer"), POWER))
        }
        "systemctl" if args.iter().any(|a| matches!(*a, "poweroff" | "reboot" | "halt" | "suspend" | "hibernate")) => {
            Some(Danger::new("systemctl stops the computer", POWER))
        }
        "kill" if args.len() >= 2 && args.last() == Some(&"-1") => Some(Danger::new("kill -1 ends every process you can reach", KILL)),
        "killall5" => Some(Danger::new("killall5 ends every process", KILL)),
        "taskkill" | "pkill" | "killall" | "kill" | "stop-process" | "spps" if args.iter().any(|a| is_oak(a)) => {
            Some(Danger::new("it kills Oak by name, the user's running Oak included", KILL))
        }
        "git" => git(&args),
        _ => None,
    }
}

/// Git commands that throw away work that may exist nowhere else.
fn git(args: &[&str]) -> Option<Danger> {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        i += if matches!(args[i], "-C" | "-c" | "--git-dir" | "--work-tree") { 2 } else { 1 };
    }
    let sub = *args.get(i)?;
    let rest = &args[i + 1..];
    let short = |c: char| rest.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(c));
    let all = || rest.iter().any(|a| matches!(*a, "." | "*" | ":/" | "./"));
    let force = || short('f') || rest.contains(&"--force");
    let discards = |what: &str| Some(Danger::new(format!("{what} discards uncommitted changes"), GIT_DISCARD));
    match sub {
        "reset" if rest.contains(&"--hard") => discards("git reset --hard"),
        "clean" if force() && !short('n') && !rest.contains(&"--dry-run") => Some(Danger::new("git clean -f deletes untracked files", GIT_CLEAN)),
        "push" if force() || rest.contains(&"--mirror") || rest.iter().any(|a| a.starts_with('+')) => {
            Some(Danger::new("a force push can overwrite history on the remote", FORCE_PUSH))
        }
        "checkout" if all() && !short('b') => discards("git checkout of everything"),
        "checkout" if force() => discards("git checkout -f"),
        "switch" if force() || rest.contains(&"--discard-changes") => discards("git switch --discard-changes"),
        "restore" if all() && !(rest.contains(&"--staged") && !rest.contains(&"--worktree")) => discards("git restore of everything"),
        "stash" if rest.first().is_some_and(|a| *a == "clear") => Some(Danger::new("git stash clear drops every stash", STASH)),
        "reflog" if rest.first() == Some(&"expire") && rest.iter().any(|a| a.ends_with("=now") || a.ends_with("=all")) => {
            Some(Danger::new("expiring the reflog now makes lost commits unrecoverable", REFLOG))
        }
        _ => None,
    }
}

/// Whether a path component is a Windows drive (`C:`).
fn is_drive(p: &str) -> bool {
    p.len() == 2 && p.ends_with(':') && p.as_bytes()[0].is_ascii_alphabetic()
}

/// A path's components: `\` as `/`, `.` and `..` resolved, relative paths
/// joined to `base`. On Windows in lower case, and an MSYS drive (`/c/`)
/// as the drive (`c:`).
fn parts(path: &str, base: &[String]) -> Vec<String> {
    let mut path = path.replace('\\', "/");
    if cfg!(windows) {
        path = path.to_lowercase();
        let b = path.as_bytes();
        if b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/') {
            path = format!("{}:{}", b[1] as char, &path[2..]);
        }
    }
    let absolute = path.starts_with('/') || path.as_bytes().get(1) == Some(&b':');
    let mut out: Vec<String> = if absolute { Vec::new() } else { base.to_vec() };
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p.to_string()),
        }
    }
    out
}

/// What makes a removal target critical, if it is: the root, a top-level
/// directory (a drive's, on Windows), the home folder or a parent of it,
/// a folder in it (not a hidden one, bar keys and settings), or a parent
/// of the working folder (the working folder itself too, with
/// `with_folder`). A glob at the end (`dir/*`) counts as the directory.
fn critical(target: &str, folder: &[String], home: &[String], with_folder: bool) -> Option<String> {
    let mut t = target.replace('\\', "/");
    // The working folder by name: `$PWD`, `$(pwd)`, `` `pwd` ``.
    for var in ["${PWD}", "$PWD", "$(pwd)", "`pwd`", "$(pwd -P)", "`pwd -P`", "$(pwd -L)"] {
        if let Some(rest) = t.strip_prefix(var).filter(|r| r.is_empty() || r.starts_with('/')) {
            t = format!(".{rest}");
        }
    }
    if t.contains("$(") || t.contains('`') {
        return None;
    }
    for var in ["${HOME}", "$HOME", "${USERPROFILE}", "$USERPROFILE", "$env:USERPROFILE", "$env:HOME", "%USERPROFILE%", "%HOMEDRIVE%%HOMEPATH%"] {
        if t.get(..var.len()).is_some_and(|v| v.eq_ignore_ascii_case(var)) {
            t = format!("~{}", &t[var.len()..]);
        }
    }
    if t.starts_with('$') {
        // `$DIR/*` is `/*` when DIR is empty or unset.
        let end = t.find('/').unwrap_or(t.len());
        let guarded = t[..end].contains(":?");
        return (!guarded && matches!(&t[end..], "/" | "/*" | "/.")).then(|| format!("{target} (all of / if the variable is empty)"));
    }
    if t.contains('$') || t.contains('%') {
        return None;
    }
    if t == "~" || t.starts_with("~/") {
        if home.is_empty() {
            return Some(target.into());
        }
        let root = if is_drive(&home[0]) { "" } else { "/" };
        t = format!("{root}{}{}", home.join("/"), &t[1..]);
    }
    loop {
        let before = t.len();
        for end in ["/*", "/.*", "/."] {
            if let Some(s) = t.strip_suffix(end) {
                t = if s.is_empty() { "/".into() } else { s.into() };
            }
        }
        if t.len() > 1 && t.ends_with('/') && !(t.len() == 3 && t.get(..2).is_some_and(is_drive)) {
            t.pop();
        }
        if t.len() == before {
            break;
        }
    }
    if matches!(t.as_str(), "*" | ".*") {
        t = ".".into();
    }
    let p = parts(&t, folder);
    let within = |a: &[String], b: &[String]| a.len() <= b.len() && a == &b[..a.len()];
    let top = if p.first().is_some_and(|d| is_drive(d)) { 2 } else { 1 };
    let in_home = p.len() == home.len() + 1 && within(home, &p) && {
        let n = p.last().map_or("", |n| n.as_str());
        !n.starts_with('.') || matches!(n, ".ssh" | ".gnupg" | ".aws" | ".config")
    };
    let what = if p.len() <= top {
        "the root or a top-level directory"
    } else if within(&p, home) {
        "the home folder"
    } else if within(&p, folder) && (with_folder || p.len() < folder.len()) {
        "the working folder"
    } else if in_home && p != folder {
        "a folder in the home folder"
    } else {
        return None;
    };
    Some(format!("{target} ({what})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(cmd: &str) -> bool {
        let home = dirs::home_dir().unwrap();
        check(cmd, &home.join("work/proj")).is_err()
    }

    #[test]
    fn destructive_commands_are_blocked() {
        let bomb = [":()", "{ :|:& };:"].concat();
        let mut cmds = vec![
            "rm -rf /".to_string(),
            "rm -rf /*".into(),
            "sudo rm -rf --no-preserve-root /".into(),
            "rm -fr ~".into(),
            "rm -rf ~/".into(),
            "rm -rf $HOME/*".into(),
            "rm -rf \"$DIR\"/*".into(),
            "rm -rf /usr".into(),
            "rm -r -f .".into(),
            "rm -rf *".into(),
            "rm -rf ..".into(),
            "rm -rf ../*".into(),
            "cd x && rm -Rf ./".into(),
            "'rm' -rf /".into(),
            "rm -rf ~/Documents".into(),
            "rm -rf ~/.ssh".into(),
            "rm -rf .git".into(),
            "rm -rf ./sub/.git/".into(),
            "bash -c 'rm -rf ~'".into(),
            "sh -c \"echo hi; rm -rf /etc\"".into(),
            "eval rm -rf /".into(),
            "timeout 5 rm -rf /".into(),
            "echo / | xargs rm -rf /".into(),
            "find / -name x -delete".into(),
            "find ~ -type f -exec rm {} +".into(),
            "find . -delete".into(),
            "find . -type f -delete".into(),
            "rsync -a --delete empty/ ~/".into(),
            "chmod -R 777 /".into(),
            "chown -R me ~".into(),
            "dd if=/dev/zero of=/dev/sda bs=1M".into(),
            "echo x > /dev/sda".into(),
            "mkfs.ext4 /dev/sdb1".into(),
            "format C: /q".into(),
            "diskpart".into(),
            "shutdown -h now".into(),
            "systemctl reboot".into(),
            bomb.clone(),
            format!("bash -c '{bomb}'"),
            "kill -9 -1".into(),
            "taskkill /F /IM oak.exe".into(),
            "taskkill //F //IM Oak.exe".into(),
            "pkill oak".into(),
            "powershell -Command \"Stop-Process -Name oak\"".into(),
            "powershell -NoProfile -Command \"Stop-Computer\"".into(),
            "git reset --hard HEAD~1".into(),
            "git -C sub clean -fdx".into(),
            "git push --force origin main".into(),
            "git push -f".into(),
            "git push --mirror".into(),
            "git checkout -- .".into(),
            "git checkout -f main".into(),
            "git switch --discard-changes main".into(),
            "git restore .".into(),
            "git stash clear".into(),
            "git reflog expire --expire=now --all".into(),
            "bash -c \"$(cat <<'__FREEBUFF_COMMAND__'\nrm -rf ~\n__FREEBUFF_COMMAND__\n)\"".into(),
            "rm -rf \"$PWD\"".into(),
            "rm -rf ${PWD}/".into(),
            "rm -rf $(pwd)".into(),
            "rm -rf \"$(pwd)\"/*".into(),
            "rm -rf `pwd`".into(),
            "sudo -u root rm -rf ~".into(),
            "sudo --user root rm -rf /".into(),
            "doas -u root rm -rf ~".into(),
            "env -u FOO rm -rf ~".into(),
            "nice -n 10 rm -rf ~".into(),
        ];
        if cfg!(windows) {
            // Windows paths: a drive's top level, home in another case and
            // in MSYS form, and Windows' own shells.
            let h = dirs::home_dir().unwrap().display().to_string().replace('\\', "/");
            let msys = format!("/{}{}", h[..1].to_lowercase(), &h[2..]);
            cmds.extend([
                "rm -rf C:/Windows".to_string(),
                "rm -rf 'C:\\Program Files'".into(),
                "rm -rf /c/Users".into(),
                format!("rm -rf {msys}"),
                format!("rm -rf {}/Desktop", h.to_uppercase()),
                "powershell -NoProfile -Command \"Remove-Item -Recurse -Force C:\\Users\"".into(),
                "pwsh -c 'ri -r -fo $env:USERPROFILE'".into(),
                "cmd //c rd //s //q C:\\\\Windows".into(),
                "cmd /c \"del /s /q %USERPROFILE%\"".into(),
            ]);
        }
        for cmd in &cmds {
            assert!(blocked(cmd), "{cmd}");
        }
        let e = check("rm -rf /", Path::new("/tmp")).unwrap_err();
        assert!(e.starts_with("Blocked by Oak's command guard: a recursive rm of / (the root"), "{e}");
        assert!(e.contains("Work around it: remove only the files or folders you mean") && !e.contains("If you are sure"), "{e}");
    }

    #[test]
    fn everyday_commands_pass() {
        let bomb = [":()", "{ :|:& };:"].concat();
        let mut cmds = vec![
            "rm -rf target node_modules dist".to_string(),
            "rm -rf ./build/*".into(),
            "rm -rf /tmp/x".into(),
            "rm -rf ~/work/proj/.cache".into(),
            "rm -rf ~/.cache".into(),
            "rm -rf .git/index.lock".into(),
            "rm -rf \"${DIR:?}\"/*".into(),
            "rm -rf \"$DIR\"".into(),
            "rm file.txt".into(),
            "rm -f *.o".into(),
            "echo 'rm -rf /'".into(),
            "grep -r 'rm -rf ~' .".into(),
            "cat <<EOF\nrm -rf /\nEOF".into(),
            format!("cat > bomb.txt <<'EOF'\n{bomb}\necho x > /dev/sda\nEOF"),
            "# rm -rf /".into(),
            "find . -name '*.pyc' -delete".into(),
            "find /tmp/x -delete".into(),
            "rsync -a --delete src/ dist/".into(),
            "chmod -R u+w .".into(),
            "chmod +x script.sh".into(),
            "dd if=/dev/zero of=/dev/null count=1".into(),
            "dd if=a of=b".into(),
            "kill -9 12345".into(),
            "kill %1".into(),
            "taskkill /PID 1234 /F".into(),
            "pkill -f 'node server.js'".into(),
            "shutdown /a".into(),
            "cmd /c rd /s /q build".into(),
            "powershell -Command \"Remove-Item -Recurse -Force build\"".into(),
            "powershell -Command \"Remove-Item -Recurse C:\\Windows -WhatIf\"".into(),
            "rmdir empty".into(),
            "git reset HEAD~1".into(),
            "git reset --soft HEAD~1".into(),
            "git clean -n".into(),
            "git clean -fdn".into(),
            "git push --force-with-lease".into(),
            "git push origin main".into(),
            "git checkout -b feature".into(),
            "git checkout main".into(),
            "git switch -c feature".into(),
            "git restore --staged .".into(),
            "git restore src/main.rs".into(),
            "git stash".into(),
            "git reflog".into(),
            "cargo build --release && cargo test".into(),
            "ls -la /".into(),
            "rm -rf \"$PWD/build\"".into(),
            "rm -rf $(pwd)/target".into(),
            "sudo -u www rm -rf /tmp/x".into(),
            "xargs -n 1 rm -rf < list.txt".into(),
        ];
        if cfg!(windows) {
            cmds.extend(["rm -rf C:/Users/x/proj/target".to_string(), "rm -rf /c/tmp/x/y".into()]);
        }
        for cmd in &cmds {
            assert!(!blocked(cmd), "{cmd}: {:?}", check(cmd, &dirs::home_dir().unwrap().join("work/proj")));
        }
    }
}
