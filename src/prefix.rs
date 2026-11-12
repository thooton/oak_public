//! Stable prefix: what goes before a conversation (the system prompt and
//! the tools) stays byte for byte as its cache was written, and what changes
//! meanwhile reaches the model at the end of the messages instead, where it
//! adds to the cache rather than rewriting all of it:
//!
//! - the date in the system prompt: "Today's date is now …";
//! - a tool whose definition changed (AGENT's model list follows the
//!   providers connected), and the tools of a kit loaded meanwhile (LOAD:
//!   AGENT, BROWSER, WINDOW, and a local model's GOAL);
//! - Claude's effort (per-message `output_config`).
//!
//! All are held as they were (`Pin`) while the cache lives, and brought up
//! to date at a moment it is cold anyway (expired, a summary, another
//! account or model), where rewriting the prefix costs nothing. Then what
//! the conversation says twice is folded too (`fold`): the result of each
//! LOAD (the kit is in the prompt and tools now) and the text of each NOTE
//! call (the session memory has the notes).
//!
//! How the changes go depends on the provider (`Mode`):
//! - Claude: a mid-conversation system message (`role: "system"`), with
//!   `tool_addition` blocks for tools (the inline-tools beta) and
//!   `output_config` for effort (its own beta);
//! - Codex: a developer message, and an `additional_tools` item for tools;
//! - any other (OpenRouter, NodeClub, the user's own server): a text block
//!   at the end of the user turn they follow. Tools can't be added there, so
//!   a loaded kit's tools are reached through LOAD (`input`) until the pin is
//!   renewed.
//!
//! The messages are kept with the conversation (`SysMsg`, at the transcript
//! index they come before), so every later request sends them where they
//! first were (`inject`); a summary that covers one leaves it out.

use crate::api::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The beta for `tool_addition` blocks.
pub const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";

/// The beta for a system message's `output_config` (per-message effort).
pub const EFFORT_BETA: &str = "mid-conversation-output-config-2026-07-01";

/// How changes reach the model (see the module's comment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Claude,
    Codex,
    Generic,
}

impl Mode {
    /// Whether tools can be added (and redefined) mid-conversation.
    pub fn inline_tools(self) -> bool {
        self != Mode::Generic
    }
}

/// The mode for a conversation on `model`, or None to send everything as it
/// is now (the setting off, or Freebuff, which sends its own prompt and
/// tools).
pub fn mode(provider: Provider, model: &str) -> Option<Mode> {
    if !crate::api::stable_prefix() {
        return None;
    }
    Some(match provider {
        Provider::Freebuff => return None,
        Provider::Claude if supports(model) => Mode::Claude,
        Provider::Codex if codex_native() => Mode::Codex,
        _ => Mode::Generic,
    })
}

// Codex's developer messages and `additional_tools` items: turned off for
// the session if the backend refuses them (`codex::open`).
switch!(CODEX_NATIVE, codex_native, set_codex_native, true);

/// `model` is one of `ids`, or a dated version of one.
fn one_of(model: &str, ids: &[&str]) -> bool {
    let m = model.split('[').next().unwrap_or(model);
    let dated = |r: &str| r.len() == 8 && r.chars().all(|c| c.is_ascii_digit());
    ids.iter().any(|p| m == *p || m.strip_prefix(p).and_then(|r| r.strip_prefix('-')).is_some_and(dated))
}

/// Whether `model` takes mid-conversation system messages (Sonnet 5 and
/// older models don't).
pub fn supports(model: &str) -> bool {
    one_of(model, &["claude-opus-5-5", "claude-opus-5", "claude-opus-4-8", "claude-sonnet-5-5", "claude-haiku-5-5"])
        || model.starts_with("claude-fable-")
        || model.starts_with("claude-mythos-")
}

/// Whether `model` takes a per-message effort (Claude Fable 5 and Opus 4.8
/// don't).
pub fn supports_effort(model: &str) -> bool {
    one_of(model, &["claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2", "claude-mythos-5-2", "claude-fable-5-1", "claude-mythos-5-1"])
}

/// A conversation's stable prefix.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub pin: Option<Pin>,
    pub sys: Vec<SysMsg>,
    /// The kits loaded (LOAD), in the order they were.
    pub kits: Vec<String>,
    /// The transcript's length when the pin was last renewed: what came
    /// before is folded (`fold`).
    pub folded: usize,
}

/// What a request goes with besides its messages: the date and kits its
/// system prompt is built with, its effort and its tools. As built for it
/// (`Setup::now`) or as held for the cache (`Pin`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pin {
    pub day: String,
    pub kits: Vec<String>,
    pub effort: Option<String>,
    pub tools: Vec<Value>,
}

/// A change the model is told of, before transcript message `at`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SysMsg {
    pub at: usize,
    /// "date", "effort" or "tool:NAME".
    pub kind: String,
    /// What it brings, to tell whether a newer one is needed: the date, the
    /// effort, a hash of the tool's definition.
    pub key: String,
    /// What it says (may be empty).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Tool definitions it adds or replaces.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    /// The effort from here on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

fn hash(v: &Value) -> String {
    let d = Sha256::digest(v.to_string().as_bytes());
    d[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a system message at `at` is in the view after summary cut `cut`.
fn in_view(at: usize, cut: usize) -> bool {
    cut == 0 || at > cut
}

/// What a folded NOTE call's text becomes.
pub const NOTE_FOLDED: &str = "(Saved. Your session memory has the notes as they are now.)";

/// What a request is built with, and how.
pub struct Setup {
    pub mode: Mode,
    /// Nothing cached is left: the pin is renewed.
    pub cold: bool,
    /// The transcript's length, and the summary cut the request is built on.
    pub n: usize,
    pub cut: usize,
    /// The transcript ends with a user turn (where a message may go).
    pub add: bool,
    /// The effort can change per message (else it isn't held).
    pub effort_per_message: bool,
    /// What the request would go with without the pin.
    pub now: Pin,
}

impl State {
    /// Holds what requests go with from now on (at a cold moment, or the
    /// first request), which says all the system messages said: they go.
    pub fn renew(&mut self, n: usize, now: &Pin) {
        self.pin = Some(now.clone());
        self.sys.clear();
        self.folded = n;
    }

    /// What the model was last told of `kind` in the view after `cut`.
    fn said(&self, kind: &str, cut: usize) -> Option<&str> {
        self.sys.iter().rev().find(|s| s.kind == kind && in_view(s.at, cut)).map(|s| s.key.as_str())
    }

    /// For a request: what it goes with (the pin, renewed first when cold),
    /// with system messages added for what changed since (before transcript
    /// message `n`, after the user turn that ends it).
    pub fn prepare(&mut self, s: Setup) -> Pin {
        // (A pin without tools is from an older Oak, which held less.)
        let old = self.pin.as_ref().is_some_and(|p| p.tools.is_empty() && !s.now.tools.is_empty());
        if s.cold || self.pin.is_none() || old {
            self.renew(s.n, &s.now);
        }
        let pin = self.pin.clone().unwrap_or_default();
        let (now, cut) = (&s.now, s.cut);
        let mut new = vec![];
        let msg = |kind: String, key: String| SysMsg { at: s.n, kind, key, ..Default::default() };
        if self.said("date", cut).unwrap_or(&pin.day) != now.day {
            new.push(SysMsg { text: format!("Today's date is now {}.", now.day), ..msg("date".into(), now.day.clone()) });
        }
        for t in &now.tools {
            let name = t["name"].as_str().unwrap_or("");
            let (kind, key) = (format!("tool:{name}"), hash(t));
            let held = pin.tools.iter().find(|p| p["name"] == name);
            let said = self.said(&kind, cut).map(str::to_string).or_else(|| held.map(hash));
            if said.as_deref() == Some(key.as_str()) {
                continue;
            }
            let changed = match name {
                "AGENT" => "The AGENT tool changed: the models it can start agents on are now those in its new definition.".to_string(),
                _ => format!("The {name} tool changed: its new definition replaces the one before."),
            };
            match (s.mode.inline_tools(), held) {
                (true, None) => new.push(SysMsg { tools: vec![t.clone()], ..msg(kind, key) }),
                (true, Some(_)) => new.push(SysMsg { tools: vec![t.clone()], text: changed, ..msg(kind, key) }),
                (false, Some(_)) => new.push(SysMsg { text: format!("{changed} It is now:\n{t}"), ..msg(kind, key) }),
                // Reached through LOAD until the pin is renewed.
                (false, None) => {}
            }
        }
        let held_effort = if s.effort_per_message { pin.effort.clone() } else { now.effort.clone() };
        if s.mode == Mode::Claude && s.effort_per_message
            && let Some(e) = &now.effort
            && self.said("effort", cut).or(held_effort.as_deref()) != Some(e.as_str())
        {
            new.push(SysMsg { effort: Some(e.clone()), ..msg("effort".into(), e.clone()) });
        }
        if s.add {
            self.sys.extend(new);
        }
        Pin { effort: held_effort, ..pin }
    }

    /// `view` (the messages a request sends: the transcript's `n` messages
    /// from summary cut `cut` on, after the summary and its acknowledgement
    /// when there is one) with the system messages in their places, as
    /// `mode` sends them.
    pub fn inject(&self, mode: Mode, view: Vec<Value>, cut: usize, n: usize) -> Vec<Value> {
        let mut at: Vec<usize> = self.sys.iter().filter(|s| in_view(s.at, cut) && s.at <= n).map(|s| s.at).collect();
        at.dedup();
        if at.is_empty() {
            return view;
        }
        // Where transcript message `k` is in the view.
        let offset = view.len().saturating_sub(n.saturating_sub(cut));
        let pos = |k: usize| offset + k.saturating_sub(cut);
        let mut out = view;
        // From the last, so the earlier places stay where they are.
        for &k in at.iter().rev() {
            let group: Vec<&SysMsg> = self.sys.iter().filter(|s| s.at == k && in_view(s.at, cut)).filter(|s| !s.text.is_empty() || !s.tools.is_empty() || s.effort.is_some()).collect();
            if group.is_empty() {
                continue;
            }
            let text: Vec<&str> = group.iter().map(|s| s.text.as_str()).filter(|t| !t.is_empty()).collect();
            let mut i = pos(k).min(out.len());
            if mode == Mode::Generic {
                // A text block at the end of the user turn before.
                if text.is_empty() {
                    continue;
                }
                let block = json!({"type": "text", "text": format!("<system-reminder>\n{}\n</system-reminder>", text.join("\n"))});
                match out.get_mut(i.wrapping_sub(1)).filter(|m| m["role"] == "user") {
                    Some(m) => m["content"].as_array_mut().into_iter().for_each(|c| c.push(block.clone())),
                    None => out.insert(i, json!({"role": "user", "content": [block]})),
                }
                continue;
            }
            let mut content: Vec<Value> = group.iter().flat_map(|s| &s.tools).map(|t| json!({"type": "tool_definition", "definition": t})).collect();
            content.extend(text.iter().map(|t| json!({"type": "text", "text": t})));
            let mut m = json!({"role": "system", "content": content});
            if let Some(e) = group.iter().rev().find_map(|s| s.effort.as_ref()) {
                m["effort"] = json!(e);
            }
            // Claude: a message with content must come right before a reply
            // (or end the request).
            if mode == Mode::Claude && !m["content"].as_array().is_some_and(Vec::is_empty) {
                while i < out.len() && out[i]["role"] != "assistant" {
                    i += 1;
                }
            }
            out.insert(i, m);
        }
        out
    }

    /// Folds, in `view` (as for `inject`), what the transcript before the
    /// last renewal says twice: a LOAD's result (the kit's instructions are
    /// in the system prompt and tools now) and a NOTE call's text (the
    /// session memory has the notes).
    pub fn fold(&self, view: &mut [Value], cut: usize, n: usize) {
        if self.folded <= cut {
            return;
        }
        let offset = view.len().saturating_sub(n.saturating_sub(cut));
        let end = (offset + self.folded - cut).min(view.len());
        let mut loads: Vec<(Value, String)> = vec![];
        for m in &mut view[offset..end] {
            let Some(content) = m["content"].as_array_mut() else { continue };
            for b in content {
                match b["type"].as_str() {
                    Some("tool_call") => {
                        let action = b["input"]["action"].as_str().unwrap_or("");
                        let note = matches!(b["name"].as_str(), Some("NOTE_NEW" | "NOTE_UPDATE")) || (b["name"] == "NOTE" && matches!(action, "new" | "update"));
                        if note && b["input"]["text"].is_string() {
                            b["input"]["text"] = json!(NOTE_FOLDED);
                        }
                        if b["name"] == "LOAD" && b["input"].get("input").is_none() {
                            loads.push((b["id"].clone(), b["input"]["name"].as_str().unwrap_or("").to_uppercase()));
                        }
                    }
                    Some("tool_result") if b["is_error"] != true => {
                        if let Some((_, kit)) = loads.iter().find(|(id, _)| *id == b["call_id"]) {
                            b["content"] = json!(format!("Loaded {kit}: it is in your tools and system prompt now."));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The betas a Claude body needs for its system messages.
pub fn betas(body: &Value) -> Vec<&'static str> {
    let sys: Vec<&Value> = body["messages"].as_array().into_iter().flatten().filter(|m| m["role"] == "system").collect();
    let mut out = vec![];
    if sys.iter().any(|m| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] != "text"))) {
        out.push(INLINE_TOOLS_BETA);
    }
    if sys.iter().any(|m| m.get("output_config").is_some()) {
        out.push(EFFORT_BETA);
    }
    out
}

/// `body` (Claude's) without its system messages, for an API that refuses
/// them.
pub fn plain_body(mut body: Value) -> Value {
    if let Some(m) = body["messages"].as_array_mut() {
        m.retain(|m| m["role"] != "system");
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(models: &[&str]) -> Vec<Value> {
        vec![json!({"name": "READ"}), json!({"name": "AGENT", "input_schema": {"properties": {"model": {"enum": models}}}})]
    }

    fn now(day: &str, effort: &str, tools: Vec<Value>) -> Pin {
        Pin { day: day.into(), kits: vec![], effort: Some(effort.into()), tools }
    }

    fn setup(mode: Mode, n: usize, now: Pin) -> Setup {
        Setup { mode, cold: false, n, cut: 0, add: true, effort_per_message: true, now }
    }

    #[test]
    fn models_that_take_system_messages() {
        for m in ["claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-4-8", "claude-opus-5-5[1m]", "claude-haiku-5-5-20261001"] {
            assert!(supports(m), "{m}");
        }
        for m in ["claude-sonnet-5", "claude-opus-4-7", "claude-sonnet-4-6", "gpt-6-astra", "claude-opus-5-1"] {
            assert!(!supports(m), "{m}");
        }
        assert!(supports_effort("claude-opus-5-5") && supports_effort("claude-fable-5-1") && supports_effort("claude-fable-5-2"));
        assert!(!supports_effort("claude-opus-4-8") && !supports_effort("claude-fable-5"));
    }

    /// Nothing changed: no messages, the tools as built.
    #[test]
    fn unchanged_prefix_adds_nothing() {
        let mut s = State::default();
        let p = s.prepare(setup(Mode::Claude, 1, now("2026-11-06", "high", tools(&["a"]))));
        assert_eq!((p.day.as_str(), p.tools.clone()), ("2026-11-06", tools(&["a"])));
        s.prepare(setup(Mode::Claude, 3, now("2026-11-06", "high", tools(&["a"]))));
        assert!(s.sys.is_empty());
    }

    /// A new day, another model list, another effort and a loaded kit: the
    /// prompt, tools and effort stay as the cache has them, and one message
    /// tells each change, once.
    #[test]
    fn changes_become_system_messages() {
        let mut s = State::default();
        s.prepare(setup(Mode::Claude, 1, now("2026-11-06", "high", tools(&["a"]))));
        let mut t = tools(&["a", "b"]);
        t.push(json!({"name": "BROWSER"}));
        let p = s.prepare(setup(Mode::Claude, 7, now("2026-11-07", "low", t.clone())));
        assert_eq!((p.day.as_str(), p.effort.as_deref(), p.tools.clone()), ("2026-11-06", Some("high"), tools(&["a"])));
        let kinds: Vec<&str> = s.sys.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, ["date", "tool:AGENT", "tool:BROWSER", "effort"]);
        assert!(s.sys.iter().all(|m| m.at == 7));
        assert_eq!(s.sys[1].tools, vec![t[1].clone()]);
        assert!(s.sys[2].text.is_empty(), "a loaded kit's result says what it is");
        s.prepare(setup(Mode::Claude, 9, now("2026-11-07", "low", t.clone())));
        assert_eq!(s.sys.len(), 4, "told once");
        // Back to the first list and effort: told again.
        s.prepare(setup(Mode::Claude, 11, now("2026-11-07", "high", tools(&["a"]))));
        assert_eq!(s.sys.len(), 6);
        // Renewed at a cold moment: everything as built, the messages gone.
        let p = s.prepare(Setup { cold: true, ..setup(Mode::Claude, 13, now("2026-11-08", "high", t.clone())) });
        assert_eq!((p.day.as_str(), p.tools.clone(), s.sys.len(), s.folded), ("2026-11-08", t, 0, 13));
    }

    /// A conversation from an older Oak (a pin without tools, messages of
    /// another shape) is renewed, and empty messages never go.
    #[test]
    fn older_state_is_renewed() {
        let mut s: State = serde_json::from_value(json!({"pin": {"day": "2026-11-05", "agent": {"name": "AGENT"}}, "sys": [{"at": 1, "kind": "date", "key": "x", "content": []}]})).unwrap();
        let out = s.inject(Mode::Claude, vec![m("user", "q0"), m("assistant", "a1")], 0, 2);
        assert_eq!(out.len(), 2);
        let p = s.prepare(setup(Mode::Claude, 3, now("2026-11-06", "high", tools(&["a"]))));
        assert_eq!((p.tools, s.sys.len(), s.folded), (tools(&["a"]), 0, 3));
    }

    /// Without a user turn to follow, nothing is added (it waits).
    #[test]
    fn messages_wait_for_a_user_turn() {
        let mut s = State::default();
        s.prepare(setup(Mode::Claude, 1, now("2026-11-06", "high", vec![])));
        s.prepare(Setup { add: false, ..setup(Mode::Claude, 2, now("2026-11-07", "high", vec![])) });
        assert!(s.sys.is_empty());
    }

    /// Without inline tools a new tool isn't told (LOAD reaches it), a
    /// changed one is told in text; effort isn't held.
    #[test]
    fn generic_mode_tells_in_text() {
        let mut s = State::default();
        s.prepare(setup(Mode::Generic, 1, now("2026-11-06", "high", tools(&["a"]))));
        let mut t = tools(&["a", "b"]);
        t.push(json!({"name": "BROWSER"}));
        let p = s.prepare(Setup { effort_per_message: false, ..setup(Mode::Generic, 3, now("2026-11-06", "low", t)) });
        assert_eq!(p.effort.as_deref(), Some("low"));
        assert_eq!(s.sys.len(), 1);
        assert!(s.sys[0].tools.is_empty() && s.sys[0].text.contains("\"b\""));
    }

    fn m(r: &str, t: &str) -> Value {
        json!({"role": r, "content": [{"type": "text", "text": t}]})
    }

    fn sys(at: usize, t: &str) -> SysMsg {
        SysMsg { at, kind: "date".into(), key: t.into(), text: t.into(), ..Default::default() }
    }

    fn texts(v: &[Value]) -> Vec<String> {
        v.iter().map(|x| x["content"].as_array().unwrap().iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("+")).collect()
    }

    /// Each message goes before the transcript message it came before; a
    /// summary leaves out those it covers.
    #[test]
    fn inject_places_messages() {
        let s = State { sys: vec![sys(1, "d1"), sys(3, "d3")], ..Default::default() };
        let out = s.inject(Mode::Claude, vec![m("user", "q0"), m("assistant", "a1"), m("user", "q2")], 0, 3);
        assert_eq!(texts(&out), ["q0", "d1", "a1", "q2", "d3"]);
        assert_eq!(out[1]["role"], "system");
        // Summary at cut 4 of a 6-message transcript: [summary, ack, t4, t5].
        let s = State { sys: vec![sys(3, "d3"), sys(5, "x5")], ..Default::default() };
        let out = s.inject(Mode::Codex, vec![m("user", "S"), m("assistant", "ack"), m("user", "t4"), m("assistant", "t5")], 4, 6);
        assert_eq!(texts(&out), ["S", "ack", "t4", "x5", "t5"]);
    }

    /// One message per place, tools first, the effort with it; Claude's
    /// goes right before a reply.
    #[test]
    fn inject_merges_and_waits_for_a_reply() {
        let tool = SysMsg { at: 1, kind: "tool:X".into(), key: "k".into(), tools: vec![json!({"name": "X"})], ..Default::default() };
        let effort = SysMsg { at: 1, kind: "effort".into(), key: "low".into(), effort: Some("low".into()), ..Default::default() };
        let s = State { sys: vec![sys(1, "d1"), tool, effort], ..Default::default() };
        let out = s.inject(Mode::Claude, vec![m("user", "q0"), m("user", "q1"), m("assistant", "a2")], 0, 3);
        assert_eq!(out[2]["role"], "system");
        assert_eq!(out[2]["content"][0]["type"], "tool_definition");
        assert_eq!(out[2]["content"][1]["text"], "d1");
        assert_eq!(out[2]["effort"], "low");
        assert_eq!(out[3]["role"], "assistant");
    }

    /// Without system messages, the text goes at the end of the user turn.
    #[test]
    fn generic_inject_appends_text() {
        let s = State { sys: vec![sys(1, "d1"), sys(1, "e1")], ..Default::default() };
        let out = s.inject(Mode::Generic, vec![m("user", "q0"), m("assistant", "a1")], 0, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"][1]["text"], "<system-reminder>\nd1\ne1\n</system-reminder>");
    }

    /// Before the renewal, LOAD results and NOTE texts are folded.
    #[test]
    fn fold_before_the_renewal() {
        let call = |id: &str, name: &str, input: Value| json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": name, "input": input}]});
        let result = |id: &str, t: &str| json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": t}]});
        let mut view = vec![
            m("user", "go"),
            call("a", "LOAD", json!({"name": "browser"})),
            result("a", "long instructions"),
            call("b", "NOTE", json!({"action": "new", "text": "fact"})),
            result("b", "Saved note 1."),
            call("c", "LOAD", json!({"name": "AGENT"})),
            result("c", "more instructions"),
        ];
        let s = State { folded: 5, ..Default::default() };
        s.fold(&mut view, 0, 7);
        assert_eq!(view[2]["content"][0]["content"], "Loaded BROWSER: it is in your tools and system prompt now.");
        assert_eq!(view[3]["content"][0]["input"]["text"], NOTE_FOLDED);
        assert_eq!(view[6]["content"][0]["content"], "more instructions", "after the renewal");
    }

    #[test]
    fn betas_follow_what_the_body_has() {
        let body = json!({"messages": [
            {"role": "user", "content": []},
            {"role": "system", "content": [{"type": "tool_addition", "tool": {"type": "tool_definition", "definition": {}}}]},
            {"role": "system", "content": [], "output_config": {"effort": "low"}}]});
        assert_eq!(betas(&body), [INLINE_TOOLS_BETA, EFFORT_BETA]);
        let plain = plain_body(body);
        assert!(betas(&plain).is_empty());
        assert_eq!(plain["messages"].as_array().unwrap().len(), 1);
    }
}
