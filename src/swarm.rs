//! Swarms (/swarm): a coordinator conversation and the named members it
//! (and they) add, all talking on one shared, append-only board with
//! @mentions. What is kept here: each swarm's board and roster, in Oak's
//! folder under `swarms/<the coordinator's conversation id>/` (board.jsonl,
//! one post per line, and roster.json), and how a post's mentions pick
//! who it wakes. The agents' side (the SWARM tool, delivering posts,
//! Oak's own stall monitor) is `agent::swarm`; the Swarm view is
//! `swarm_view`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

/// The coordinator's name on the board.
pub const COORDINATOR: &str = "coordinator";
/// Who Oak's own stall monitor posts as.
pub const MONITOR: &str = "monitor";
/// Names no member can take: the broadcast, the coordinator, the monitor.
const RESERVED: [&str; 4] = ["all", COORDINATOR, MONITOR, "user"];
/// The longest member name.
pub const NAME_MAX: usize = 32;

/// A post on the board: its sequence number (from 1), who posted it (a
/// member's name, without the @), its text and when (Unix ms).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub seq: u64,
    pub from: String,
    pub text: String,
    pub at: i64,
}

/// A member of the swarm: the coordinator first, then those added.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    /// Its conversation.
    pub conv: String,
    /// Who added it (a member's name); empty for the coordinator.
    #[serde(default)]
    pub added_by: String,
    #[serde(default)]
    pub killed: bool,
    /// The last post it has seen (delivered to it, or read).
    #[serde(default)]
    pub seen: u64,
    /// Asleep on the board for any post (SWARM wait with
    /// `mentions_only: false`), not only those naming it.
    #[serde(default)]
    pub any: bool,
}

/// A swarm's title and members, and the monitor's last stall post.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Roster {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub members: Vec<Member>,
    /// The seq of the monitor's last stall post: it posts again only after
    /// someone else has.
    #[serde(default)]
    pub stall_at: u64,
}

/// A swarm: its roster and its board.
#[derive(Debug, Default)]
pub struct Swarm {
    pub id: String,
    pub roster: Roster,
    pub posts: Vec<Post>,
    /// Bumped on every change, for the Swarm view.
    pub version: u64,
}

/// The swarms read so far, by coordinator id.
static SWARMS: LazyLock<Mutex<HashMap<String, Arc<Mutex<Swarm>>>>> = LazyLock::new(Default::default);

/// Where swarm `id` is kept. Tests keep theirs in a folder of the test
/// run's own, as their conversations are (`store::conversations_dir`).
fn dir(id: &str) -> PathBuf {
    #[cfg(test)]
    let root = std::env::temp_dir().join(format!("oak-test-swarms-{}", std::process::id()));
    #[cfg(not(test))]
    let root = crate::store::dir().join("swarms");
    root.join(id)
}

/// Swarm `id` (the coordinator's conversation id), read from its folder
/// the first time (empty if it has none yet).
pub fn get(id: &str) -> Arc<Mutex<Swarm>> {
    SWARMS.lock().unwrap().entry(id.to_string()).or_insert_with(|| Arc::new(Mutex::new(Swarm::load(id)))).clone()
}

/// Forgets swarm `id` and deletes its folder (its coordinator's
/// conversation was deleted).
pub fn remove(id: &str) {
    SWARMS.lock().unwrap().remove(id);
    let d = dir(id);
    if d.is_dir() {
        let _ = std::fs::remove_dir_all(d);
    }
}

impl Swarm {
    fn load(id: &str) -> Swarm {
        let d = dir(id);
        let roster = std::fs::read(d.join("roster.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let posts = std::fs::read_to_string(d.join("board.jsonl"))
            .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default();
        Swarm { id: id.to_string(), roster, posts, version: 1 }
    }

    /// Writes the roster.
    pub fn save(&mut self) {
        self.version += 1;
        if self.id.is_empty() {
            return;
        }
        let d = dir(&self.id);
        let _ = std::fs::create_dir_all(&d);
        if let Ok(b) = serde_json::to_vec_pretty(&self.roster) {
            let _ = crate::store::write_atomic(&d.join("roster.json"), &b);
        }
    }

    /// The board's last seq (0 while it is empty).
    pub fn head(&self) -> u64 {
        self.posts.last().map_or(0, |p| p.seq)
    }

    /// Adds a post by `from` at `at` (appended to the board's file) and
    /// returns it.
    pub fn post(&mut self, from: &str, text: &str, at: i64) -> Post {
        let p = Post { seq: self.head() + 1, from: from.to_string(), text: text.trim().to_string(), at };
        if !self.id.is_empty() {
            let d = dir(&self.id);
            let _ = std::fs::create_dir_all(&d);
            if let (Ok(line), Ok(mut f)) = (serde_json::to_string(&p), std::fs::OpenOptions::new().create(true).append(true).open(d.join("board.jsonl"))) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.posts.push(p.clone());
        self.version += 1;
        p
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.roster.members.iter().find(|m| m.name == name)
    }

    pub fn member_mut(&mut self, name: &str) -> Option<&mut Member> {
        self.roster.members.iter_mut().find(|m| m.name == name)
    }

    /// The members a name on the board means: the member, or a numbered
    /// group's (`fix` for fix-1, fix-2...), killed ones too.
    pub fn named(&self, name: &str) -> Vec<&Member> {
        self.roster.members.iter().filter(|m| names(&m.name, name)).collect()
    }

    /// Who post `p` wakes: the members it names (@name, @group, @all) and
    /// those asleep for any post, but not its author or the killed.
    pub fn recipients(&self, p: &Post) -> Vec<Member> {
        let mentioned = mentions(&p.text);
        self.roster
            .members
            .iter()
            .filter(|m| !m.killed && m.name != p.from)
            .filter(|m| m.any || mentioned.iter().any(|x| x == "all" || names(&m.name, x)))
            .cloned()
            .collect()
    }

    /// Whether post `p` names `member` (or all).
    pub fn names_member(p: &Post, member: &str) -> bool {
        mentions(&p.text).iter().any(|x| x == "all" || names(member, x))
    }

    /// The posts after `since`, at most `max`.
    pub fn after(&self, since: u64, max: usize) -> &[Post] {
        let from = self.posts.partition_point(|p| p.seq <= since);
        &self.posts[from..(from + max).min(self.posts.len())]
    }

    /// The last `n` posts.
    pub fn recent(&self, n: usize) -> &[Post] {
        &self.posts[self.posts.len().saturating_sub(n)..]
    }

    /// The coordinator said the job is done: its last post starts with
    /// [done] (the monitor then stays quiet).
    pub fn done(&self) -> bool {
        self.posts.iter().rev().find(|p| p.from == COORDINATOR).is_some_and(|p| p.text.trim_start().to_lowercase().starts_with("[done]"))
    }

    /// How often each member named another: (from, to, posts).
    pub fn edges(&self) -> Vec<(String, String, u32)> {
        let mut out: Vec<(String, String, u32)> = vec![];
        for p in &self.posts {
            let to: Vec<&Member> = self.roster.members.iter().filter(|m| m.name != p.from && Self::names_member(p, &m.name)).collect();
            for m in to {
                match out.iter_mut().find(|(f, t, _)| *f == p.from && *t == m.name) {
                    Some(e) => e.2 += 1,
                    None => out.push((p.from.clone(), m.name.clone(), 1)),
                }
            }
        }
        out
    }
}

/// The names a text mentions (`@parser`, `@fix`, `@all`), lowercased,
/// without the @. An @ inside a word (an email address) isn't one.
pub fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '@' || (i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) {
            continue;
        }
        let name: String = chars[i + 1..].iter().take_while(|c| c.is_alphanumeric() || **c == '-' || **c == '_' || **c == '.').collect();
        // (A sentence's full stop or a trailing dash isn't part of it.)
        let name = name.trim_end_matches(['.', '-']).to_lowercase();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Whether member `member` answers to `mention`: its name, or its group
/// (`fix` for `fix-3`).
pub fn names(member: &str, mention: &str) -> bool {
    member == mention || member.strip_prefix(mention).and_then(|r| r.strip_prefix('-')).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A member's name as given, checked: lowercase letters, digits, - and _,
/// starting with a letter, at most NAME_MAX long, not reserved. A leading
/// @ is dropped.
pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim().trim_start_matches('@').to_lowercase();
    if n.is_empty() {
        return Err("The name is empty.".into());
    }
    if n.len() > NAME_MAX || !n.starts_with(|c: char| c.is_ascii_lowercase()) || !n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err(format!("`{n}` isn't a member name: short, lowercase letters, digits, - and _, starting with a letter (at most {NAME_MAX})."));
    }
    if RESERVED.contains(&n.as_str()) {
        return Err(format!("`{n}` is reserved."));
    }
    Ok(n)
}

/// A post as the model reads it: `#12 @parser: text`.
pub fn post_line(p: &Post) -> String {
    format!("#{} @{}: {}", p.seq, p.from, p.text)
}

/// `text` in runs, each marked when it is a mention (`@name`, as
/// `mentions` reads them), for the Swarm view to color.
pub fn split_mentions(text: &str) -> Vec<(&str, bool)> {
    let mut out = vec![];
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let at_word = prev.is_none_or(|p| !(p.is_alphanumeric() || p == '_'));
        prev = Some(c);
        if c != '@' || !at_word {
            continue;
        }
        let mut end = i + 1;
        while let Some(&(j, d)) = iter.peek() {
            if d.is_alphanumeric() || d == '-' || d == '_' || d == '.' {
                end = j + d.len_utf8();
                prev = Some(d);
                iter.next();
            } else {
                break;
            }
        }
        // (Not a full stop or dash after it.)
        let name_end = i + 1 + text[i + 1..end].trim_end_matches(['.', '-']).len();
        if name_end > i + 1 {
            if i > start {
                out.push((&text[start..i], false));
            }
            out.push((&text[i..name_end], true));
            start = name_end;
        }
    }
    if start < text.len() {
        out.push((&text[start..], false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_pick_names_groups_and_all() {
        assert_eq!(mentions("@parser see #4, @fix and @ALL. mail a@b.c"), ["parser", "fix", "all"]);
        assert_eq!(mentions("@coordinator [done] ok."), ["coordinator"]);
        assert!(names("fix-2", "fix") && names("fix", "fix") && !names("fixer", "fix") && !names("fix-a", "fix"));
    }

    #[test]
    fn posts_wake_who_they_name() {
        let mut s = Swarm::default();
        for (name, any) in [(COORDINATOR, false), ("parser", false), ("fix-1", false), ("fix-2", true), ("old", false)] {
            s.roster.members.push(Member { name: name.into(), any, killed: name == "old", ..Default::default() });
        }
        let p = s.post(COORDINATOR, "@fix take #2, @old too", 1);
        let woken: Vec<String> = s.recipients(&p).into_iter().map(|m| m.name).collect();
        assert_eq!(woken, ["fix-1", "fix-2"]);
        let p = s.post("parser", "@all the API changed", 2);
        let woken: Vec<String> = s.recipients(&p).into_iter().map(|m| m.name).collect();
        assert_eq!(woken, [COORDINATOR, "fix-1", "fix-2"]);
        let p = s.post("parser", "notes for later", 3);
        assert_eq!(s.recipients(&p).len(), 1, "only the one asleep for any post");
        assert_eq!((p.seq, s.head(), s.after(1, 10).len(), s.recent(1)[0].seq), (3, 3, 2, 3));
    }

    #[test]
    fn done_is_the_coordinators_last_word() {
        let mut s = Swarm::default();
        s.post(COORDINATOR, "[done] parser rewritten", 1);
        assert!(s.done());
        s.post("parser", "one more thing", 2);
        assert!(s.done());
        s.post(COORDINATOR, "@parser go on", 3);
        assert!(!s.done());
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(check_name("@Parser").unwrap(), "parser");
        assert!(check_name("all").is_err() && check_name("coordinator").is_err() && check_name("2x").is_err() && check_name("a b").is_err());
    }

    #[test]
    fn mentions_are_split_out_for_coloring() {
        assert_eq!(split_mentions("@fix-1 see #2. a@b @all."), [("@fix-1", true), (" see #2. a@b ", false), ("@all", true), (".", false)]);
        assert_eq!(split_mentions("plain"), [("plain", false)]);
    }

    #[test]
    fn the_board_and_roster_outlive_the_session() {
        let id = "kept".to_string();
        {
            let sw = get(&id);
            let mut s = sw.lock().unwrap();
            s.roster.title = "the parser".into();
            s.roster.members.push(Member { name: COORDINATOR.into(), conv: id.clone(), ..Default::default() });
            s.save();
            s.post(COORDINATOR, "@parser go", 1);
            s.post("parser", "@coordinator [done] ok", 2);
        }
        SWARMS.lock().unwrap().clear();
        let sw = get(&id);
        let s = sw.lock().unwrap();
        assert_eq!((s.roster.title.as_str(), s.roster.members.len(), s.head(), s.posts[1].from.as_str()), ("the parser", 1, 2, "parser"));
        drop(s);
        assert!(dir(&id).join("board.jsonl").is_file());
        remove(&id);
        assert!(!dir(&id).exists());
    }
}
