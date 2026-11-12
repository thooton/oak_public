// Files changed on disk by someone else: OAK remembers the files the model
// read, wrote or edited, and before the next request tells it which of
// them changed since (the user saved over one, a formatter ran, ...), with
// the changed lines, so it doesn't work from a stale copy or undo them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Files bigger than this are watched by size and time only (no diff).
const MAX_TEXT: u64 = 1_000_000;
/// Lines of context around a change.
const CONTEXT: usize = 3;
/// Changed lines shown per file at most.
const MAX_LINES: usize = 45;

#[derive(Clone, Debug, PartialEq)]
struct Snap {
    path: PathBuf,
    mtime: Option<SystemTime>,
    len: u64,
    /// The content as OAK last saw it, for a diff (text files only).
    text: Option<String>,
}

impl Snap {
    fn take(path: &Path) -> Option<Snap> {
        let meta = std::fs::metadata(path).ok()?;
        let text = (meta.len() <= MAX_TEXT).then(|| std::fs::read(path).ok().and_then(|b| String::from_utf8(b).ok())).flatten();
        Some(Snap { path: path.to_path_buf(), mtime: meta.modified().ok(), len: meta.len(), text })
    }
}

/// A file that changed on disk.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// With the changed lines (numbered as in the file now), when OAK
    /// knew the old text.
    Modified(PathBuf, Option<String>),
    Deleted(PathBuf),
}

impl Change {
    pub fn path(&self) -> &Path {
        match self {
            Change::Modified(p, _) | Change::Deleted(p) => p,
        }
    }
}

/// The files the model has seen, as it last saw them.
#[derive(Default)]
pub struct Tracked {
    files: HashMap<String, Snap>,
}

fn key(path: &Path) -> String {
    crate::tools::path_key(path)
}

impl Tracked {
    /// After READ, WRITE or EDIT: the file as the model now knows it.
    pub fn record(&mut self, path: &Path) {
        match Snap::take(path) {
            Some(s) => self.files.insert(key(path), s),
            None => self.files.remove(&key(path)),
        };
    }

    /// Takes in whatever changed without reporting it: OAK's own commands
    /// did it.
    pub fn refresh(&mut self) {
        let paths: Vec<PathBuf> = self.files.values().map(|s| s.path.clone()).collect();
        for p in paths {
            self.record(&p);
        }
    }

    /// The files that changed since, which then count as seen.
    pub fn check(&mut self) -> Vec<Change> {
        let mut out = vec![];
        let mut keys: Vec<String> = self.files.keys().cloned().collect();
        keys.sort();
        for k in keys {
            let old = &self.files[&k];
            let Ok(meta) = std::fs::metadata(&old.path) else {
                out.push(Change::Deleted(old.path.clone()));
                self.files.remove(&k);
                continue;
            };
            if meta.modified().ok() == old.mtime && meta.len() == old.len {
                continue;
            }
            let Some(new) = Snap::take(&old.path) else { continue };
            let change = match (&old.text, &new.text) {
                (Some(a), Some(b)) if a == b => None, // touched, not changed
                (Some(a), Some(b)) => Some(Change::Modified(new.path.clone(), Some(diff(a, b)))),
                _ => Some(Change::Modified(new.path.clone(), None)),
            };
            out.extend(change);
            self.files.insert(k, new);
        }
        out
    }
}

/// The changed part of `new`, numbered like READ's output, with a few lines
/// around it; lines only removed are said so.
pub fn diff(old: &str, new: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suf = a.len().min(b.len()) - pre;
    let suf = a.iter().rev().zip(b.iter().rev()).take(max_suf).take_while(|(x, y)| x == y).count();
    let (gone, added) = (a.len() - pre - suf, b.len() - pre - suf);
    let from = pre.saturating_sub(CONTEXT);
    let to = (b.len() - suf + CONTEXT).min(b.len());
    let mut out = String::new();
    if added == 0 && gone > 0 {
        out += &format!("({gone} line{} removed after line {pre})\n", crate::plural(gone));
    }
    for (i, line) in b.iter().enumerate().take(to).skip(from) {
        if i - from >= MAX_LINES {
            out += &format!("[{} more changed lines; READ the file to see them]\n", to - i);
            break;
        }
        out += &format!("{:>6}\t{line}\n", i + 1);
    }
    if gone > 0 && added > 0 {
        out += &format!("({gone} old line{} replaced)\n", if gone == 1 { " was" } else { "s were" });
    }
    out
}

/// The message to the model about changed files.
pub fn message(changes: &[Change]) -> String {
    let mut s = String::from("<system-reminder>\n");
    for c in changes {
        match c {
            Change::Modified(p, Some(d)) => s += &format!(
                "Note: {} was modified, either by the user or by a linter. This change was intentional, so make sure to take it \
                 into account as you proceed (ie. don't revert it unless the user asks you to). Don't tell the user this, since \
                 they are already aware. Here are the relevant changes (shown with line numbers):\n{d}\n",
                p.display()
            ),
            Change::Modified(p, None) => s += &format!(
                "Note: {} was modified, either by the user or by a linter, since you last read or wrote it. This change was \
                 intentional, so don't revert it unless the user asks you to. READ it again before you rely on its contents or edit it.\n\n",
                p.display()
            ),
            Change::Deleted(p) => s += &format!(
                "Note: {} was deleted, either by the user or by another program, since you last read or wrote it. Don't recreate \
                 it unless the user asks you to.\n\n",
                p.display()
            ),
        }
    }
    s.trim_end().to_string() + "\n</system-reminder>"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_show_the_changed_lines() {
        let old = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let new = old.replace("line 10", "LINE TEN\nand more");
        assert_eq!(diff(&old, &new), "     7\tline 7\n     8\tline 8\n     9\tline 9\n    10\tLINE TEN\n    11\tand more\n    12\tline 11\n    13\tline 12\n    14\tline 13\n(1 old line was replaced)\n");
        let new = old.replace("line 2\n", "").replace("line 3\n", "");
        assert_eq!(diff(&old, &new), "(2 lines removed after line 1)\n     1\tline 1\n     2\tline 4\n     3\tline 5\n     4\tline 6\n");
        assert_eq!(diff("a", "a\nb"), "     1\ta\n     2\tb\n");
        assert_eq!(diff("", "x"), "     1\tx\n");
    }
}
