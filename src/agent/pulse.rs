//! A session's pulse: what it is doing now (its model writing, its tools
//! running, waiting on something, summarizing; idle, paused) and since
//! when, its last progress and what holds it up, so "working" doesn't pass
//! for progress. It is factual: the phase follows the session's status line
//! (`Live::status`), progress is a reply or a tool result that went into
//! the transcript (by tool name, never a command), and a blocker is only
//! what is known: the wait under way, as its status line says it, or the
//! error its last turn ended on. Its last progress is kept with the
//! conversation (`Conversation::progress`), so it outlives a restart; none
//! of it is sent to the model. A local super agent's AGENT_LIST and its
//! subagents' rows and boxes show it, as the command center's workers'
//! (`superPulse` there).

use super::{Live, Session};
use crate::api;
use serde_json::Value;

/// The phases.
pub const MODEL: &str = "model";
pub const TOOLS: &str = "tools";
pub const WAITING: &str = "waiting";
pub const SUMMARIZING: &str = "summarizing";
pub const IDLE: &str = "idle";
pub const BLOCKED: &str = "blocked";
pub const PAUSED: &str = "paused";

/// The most of an error a blocker keeps.
const MAX_BLOCKER: usize = 300;

/// Status lines (or their starts) that say what holds a session up.
const BLOCKING: [&str; 5] = ["Retry", "Waiting for the usage limit", "Waiting for your answer", "Waiting for the context summary", "Waiting for agent messages"];

/// The phase a busy session's status line means.
pub fn phase_of(status: &str, compacting: bool) -> &'static str {
    if BLOCKING.iter().any(|b| status.starts_with(b)) {
        WAITING
    } else if status.starts_with("Running ") {
        TOOLS
    } else if compacting || status.starts_with("Summarizing") {
        SUMMARIZING
    } else {
        MODEL
    }
}

impl Live {
    /// Its phase, paused aside (`Pulse::of` has that).
    fn phase(&self) -> &'static str {
        if !self.busy {
            if self.error.is_some() { BLOCKED } else { IDLE }
        } else {
            phase_of(&self.status, self.compacting)
        }
    }

    /// Notes when its phase changed, after anything that could change it.
    pub(super) fn mark_phase(&mut self) {
        let p = self.phase();
        if self.phase_at.0 != p {
            self.phase_at = (p, api::now_ms());
        }
    }
}

/// A session's pulse, as of when it was read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pulse {
    pub phase: &'static str,
    /// What it is doing, as its status line says it.
    pub doing: String,
    /// When its phase began (ms since 1970; 0 unknown).
    pub since: i64,
    /// Its last progress: when (0: none yet) and what.
    pub progress_at: i64,
    pub progress: String,
    /// What holds it up; empty when nothing known does.
    pub blocker: String,
}

impl Pulse {
    pub fn of(s: &Session) -> Pulse {
        let paused = s.pause_settled();
        let (progress_at, progress) = {
            let c = s.conv.lock().unwrap();
            (c.progress_at, c.progress.clone())
        };
        let l = s.live.lock().unwrap();
        let mut phase = l.phase();
        let since = if l.phase_at.0 == phase { l.phase_at.1 } else { 0 };
        let blocker = match phase {
            _ if paused => String::new(),
            // (Waiting for its agents is what it chose to do.)
            WAITING if !l.status.starts_with("Waiting for agent messages") => l.status.clone(),
            BLOCKED => format!("Its last turn ended on an error: {}", clip(&one_line(l.error.as_deref().unwrap_or_default()), MAX_BLOCKER)),
            _ => String::new(),
        };
        if paused {
            phase = PAUSED;
        }
        let doing = if l.busy && !paused { l.status.clone() } else { String::new() };
        Pulse { phase, doing, since, progress_at, progress, blocker }
    }

    fn working(&self) -> bool {
        matches!(self.phase, MODEL | TOOLS | WAITING | SUMMARIZING)
    }

    /// How long it has been at it and its last progress, as of `now`:
    /// "For 3m · last progress 40s ago: READ returned"; empty when there
    /// is nothing to say.
    pub fn detail(&self, now: i64) -> String {
        let mut parts = vec![];
        if self.working() && self.since > 0 {
            parts.push(format!("For {}", span(now - self.since)));
        }
        let first = parts.is_empty();
        if self.progress_at > 0 {
            let what = if self.progress.is_empty() { String::new() } else { format!(": {}", self.progress) };
            parts.push(format!("{} {} ago{what}", if first { "Last progress" } else { "last progress" }, span(now - self.progress_at)));
        } else if self.working() {
            parts.push(if first { "No progress yet" } else { "no progress yet" }.into());
        }
        parts.join(" · ")
    }

    /// What holds it up, as of `now`: "Blocked: Waiting for the usage limit
    /// to reset (2m)"; empty when nothing known does.
    pub fn blocked(&self, now: i64) -> String {
        match self.blocker.as_str() {
            "" => String::new(),
            b if self.working() && self.since > 0 => format!("Blocked: {b} ({})", span(now - self.since)),
            b => format!("Blocked: {b}"),
        }
    }
}

/// What a message that went into the transcript says of progress: a
/// reply (with how many tool calls) or a tool's result (its tool's name,
/// from `names`, the calls' ids and names); None for anything else (what
/// the user wrote, a notice).
pub(super) fn progress_of(role: &str, content: &[Value], names: &[(String, String)]) -> Option<String> {
    if role == "assistant" {
        let calls = content.iter().filter(|b| crate::oak::is_call(b)).count();
        return Some(match calls {
            0 => "Its model replied".into(),
            1 => "Its model replied with 1 tool call".into(),
            n => format!("Its model replied with {n} tool calls"),
        });
    }
    let r = content.iter().rev().find(|b| crate::oak::is_result(b))?;
    let name = names.iter().find(|(id, _)| Some(id.as_str()) == r["call_id"].as_str()).map_or("A tool", |(_, n)| n.as_str());
    Some(if r["is_error"] == true { format!("{name} returned an error") } else { format!("{name} returned") })
}

/// A stretch of time, short: "40s", "12m", "1h 5m", "2d 3h".
pub fn span(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn phases_follow_the_status_line() {
        for (status, compacting, phase) in [
            ("Waiting for the model", false, MODEL),
            ("Thinking", false, MODEL),
            ("Writing BASH", false, MODEL),
            ("Running BASH, READ", false, TOOLS),
            ("Retry 2/10 in 30s after: HTTP 529", false, WAITING),
            ("Retrying after: connection reset", false, WAITING),
            ("Waiting for the usage limit to reset", false, WAITING),
            ("Waiting for your answer", false, WAITING),
            ("Summarizing older context to make room", true, SUMMARIZING),
            ("", true, SUMMARIZING),
        ] {
            assert_eq!(phase_of(status, compacting), phase, "{status}");
        }
    }

    #[test]
    fn progress_names_tools_not_commands() {
        let names = vec![("c1".to_string(), "BASH".to_string())];
        let reply = vec![json!({"type": "text", "text": "ok"}), json!({"type": "tool_call", "id": "c1", "name": "BASH", "input": {"command": "rm -rf secret"}})];
        assert_eq!(progress_of("assistant", &reply, &names).as_deref(), Some("Its model replied with 1 tool call"));
        let result = vec![crate::oak::tool_result(&json!("c1"), json!("done"), false)];
        assert_eq!(progress_of("user", &result, &names).as_deref(), Some("BASH returned"));
        let failed = vec![crate::oak::tool_result(&json!("c1"), json!("no"), true)];
        assert_eq!(progress_of("user", &failed, &names).as_deref(), Some("BASH returned an error"));
        assert_eq!(progress_of("user", &[json!({"type": "text", "text": "hi"})], &names), None);
    }

    #[test]
    fn detail_and_blocker_say_what_is_known() {
        let now = 1_000_000_000;
        let p = Pulse { phase: WAITING, doing: "Waiting for the usage limit to reset".into(), since: now - 125_000, progress_at: now - 3_600_000 - 300_000, progress: "READ returned".into(), blocker: "Waiting for the usage limit to reset".into() };
        assert_eq!(p.detail(now), "For 2m · last progress 1h 5m ago: READ returned");
        assert_eq!(p.blocked(now), "Blocked: Waiting for the usage limit to reset (2m)");
        let idle = Pulse { phase: BLOCKED, blocker: "Its last turn ended on an error: HTTP 500".into(), progress_at: now - 40_000, progress: "Its model replied".into(), ..Default::default() };
        assert_eq!(idle.detail(now), "Last progress 40s ago: Its model replied");
        assert_eq!(idle.blocked(now), "Blocked: Its last turn ended on an error: HTTP 500");
        let fresh = Pulse { phase: MODEL, since: now - 5_000, ..Default::default() };
        assert_eq!((fresh.detail(now).as_str(), fresh.blocked(now).as_str()), ("For 5s · no progress yet", ""));
        assert_eq!(clip(&one_line("a\n  b c"), 3), "a b…");
    }
}
