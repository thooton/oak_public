//! The prefix invariant: what a conversation has sent the model (the
//! system prompt, the tools and the messages before the latest) is never
//! changed. Requests only add to it. The one exception is Smart context
//! acting: a summary that replaces the start of the conversation (or a
//! revert the user makes) is the only moment the prefix is rebuilt.
//!
//! Anything that changes meanwhile reaches the model at the end of the
//! messages instead:
//!
//! - the date: "Today's date is now …";
//! - the model: "Your underlying model is now …";
//! - the notes' limit (the "Notes limit" setting): "The limit on your
//!   notes is now …";
//! - a tool whose definition changed (AGENT's model list follows the
//!   providers connected), and the tools of a kit loaded meanwhile (LOAD:
//!   AGENT, BROWSER, WINDOW);
//! - Claude's effort (per-message `output_config`).
//!
//! A pin (`Pin`) holds the exact system prompt sent (as rendered, so a
//! newer Oak with other prompt text, another model, role or scratchpad
//! leaves it as it was), the tools, the date and the effort. The cache
//! expiring, another account or model, the settings, a restart: none of
//! them renews it. Only a summary does (`State::cut`): then everything is
//! brought up to date at once. NOTE calls keep their text as the model
//! wrote it: the session memory shows the notes as they are now.
//!
//! A loaded kit stays where its LOAD put it, so the history the model sees
//! never shows it loading what it had from the start: its instructions are
//! LOAD's result, and its tools join after that result (`Join`: a renewal
//! puts their message back there). Only once a summary covers the LOAD
//! (`loads`, `Setup::base`) do the kit's prompt block and tools join the
//! prefix, at the renewal that summary brings.
//!
//! How the changes go depends on the provider (`Mode`):
//! - Claude: a mid-conversation system message (`role: "system"`), with
//!   `tool_addition` blocks for tools (the inline-tools beta) and
//!   `output_config` for effort (its own beta);
//! - Codex: a developer message, and an `additional_tools` item for tools;
//! - any other (OpenRouter, NodeClub, the user's own server, or Claude where
//!   the API refuses system messages): a text block at the end of the user
//!   turn they follow. Tools can't be added there, so a loaded kit's tools
//!   are reached through LOAD (`input`) until a summary covers the LOAD.
//!
//! The messages are kept with the conversation (`SysMsg`, at the transcript
//! index they come before), so every later request sends them where they
//! first were (`inject`); a summary that covers one leaves it out.
//!
//! Freebuff is the one exception, by design: the service checks that its
//! system prompt and tools are its own CLI's with the current date, so its
//! prompt is always dated today (a new day renews its cache); the messages
//! still only grow. No other provider gets this.

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

/// The mode for a conversation on `model`, or None for Freebuff, which
/// sends its own prompt and tools. No setting turns the invariant off.
pub fn mode(provider: Provider, model: &str) -> Option<Mode> {
    Some(match provider {
        Provider::Freebuff => return None,
        Provider::Claude if supports(model) && claude_native() => Mode::Claude,
        Provider::Codex if codex_native() => Mode::Codex,
        _ => Mode::Generic,
    })
}

// Codex's developer messages and `additional_tools` items: turned off for
// the session if the backend refuses them (`codex::open`).
switch!(CODEX_NATIVE, codex_native, set_codex_native, true);

// Claude's mid-conversation system messages: turned off for the session if
// the API refuses them (`api::open_extras`), which then go as text, as
// `plain_body` sends them.
switch!(CLAUDE_NATIVE, claude_native, set_claude_native, true);

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
    /// The summary cut the pin was renewed on: a request on another cut (a
    /// summary acted) renews it, and nothing else does. None in a state
    /// from an older Oak: the cut it is on now is taken as it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut: Option<usize>,
}

/// What a request goes with besides its messages: the date, kits and model
/// its system prompt is built with, its effort and its tools. As built for
/// it (`Setup::now`) or as held since the last summary (`Pin`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pin {
    pub day: String,
    pub kits: Vec<String>,
    pub effort: Option<String>,
    pub tools: Vec<Value>,
    /// The working folder the system prompt names. A CD's result tells the
    /// model of the new one, so no message is needed (empty in a pin from
    /// an older Oak: the folder then is the folder now, as it never changed).
    pub folder: String,
    /// The model the system prompt names (its id).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// The system prompt as it was sent (without the session memory, which
    /// changes only with a summary as well). Empty until the request that
    /// renewed the pin rendered it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// The notes' limit the system prompt names, in tokens (0: it names
    /// none: a session without notes, or a pin from an older Oak).
    #[serde(skip_serializing_if = "is_zero")]
    pub notes_limit: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A change the model is told of, before transcript message `at`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SysMsg {
    pub at: usize,
    /// "date", "model", "effort", "notes_limit" or "tool:NAME".
    pub kind: String,
    /// What it brings, to tell whether a newer one is needed: the date, the
    /// effort, the notes' limit, a hash of the tool's definition.
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

/// What a request is built with, and how.
pub struct Setup {
    pub mode: Mode,
    /// The transcript's length, and the summary cut the request is built on
    /// (another one than the pin's renews it).
    pub n: usize,
    pub cut: usize,
    /// The transcript ends with a user turn (where a message may go).
    pub add: bool,
    /// The effort can change per message (else it isn't held).
    pub effort_per_message: bool,
    /// What the request would go with without the pin: every kit loaded.
    pub now: Pin,
    /// What a renewal holds: `now` with only the kits whose LOAD the view
    /// doesn't show (a summary covers it, or there is none), and their
    /// tools.
    pub base: Pin,
    /// Where the tools of the kits loaded in view join (`loads`).
    pub joins: Vec<Join>,
    /// About how many tokens the notes take now (`agent::notes_tokens`), for
    /// what a new limit says.
    pub notes: u64,
}

/// A kit's tool that joins the conversation after the kit's LOAD.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub tool: String,
    pub kit: String,
    /// The transcript index after the LOAD's result.
    pub at: usize,
    /// The result said to call the tool through LOAD (where tools can't be
    /// added).
    pub through: bool,
}

/// A kit's first LOAD in a transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct Load {
    pub kit: String,
    /// The message with the call.
    pub call: usize,
    /// The message after the one with its result.
    pub join: usize,
    /// Its result said to call the kit's tool through LOAD.
    pub through: bool,
}

/// What a LOAD's result says where the kit's tool can't join the list.
pub const THROUGH_LOAD: &str = "call it through LOAD";

/// The kit a LOAD call loads (not one calling a kit's tool through it):
/// Oak's kits in upper case, an MCP server's as `mcp::kit_id` names it.
fn loads_kit(b: &Value) -> Option<String> {
    (b["type"] == "tool_call" && b["name"] == "LOAD" && b["input"].get("input").is_none_or(Value::is_null))
        .then(|| crate::mcp::kit_named(b["input"]["name"].as_str().unwrap_or("")))
        .filter(|k| !k.is_empty())
}

/// Each kit's first LOAD in `messages` that worked, in order.
#[cfg(test)]
pub fn loads(messages: &[Value]) -> Vec<Load> {
    let mut f = Loads::default();
    f.add(0, messages);
    f.out
}

/// `loads` of a conversation's messages: those on disk only are read once
/// (in parts) and what they hold kept, until what is held starts
/// elsewhere (`Messages::loads_memo`).
pub fn loads_of(messages: &crate::store::Messages) -> Vec<Load> {
    let base = messages.base();
    let mut f = {
        let mut memo = messages.loads_memo.lock().unwrap_or_else(|e| e.into_inner());
        match memo.as_ref().filter(|(at, _)| *at == base) {
            Some((_, f)) => f.clone(),
            None => {
                let mut f = Loads::default();
                messages.each_on_disk(base, |k, part| f.add(k, part));
                *memo = Some((base, f.clone()));
                f
            }
        }
    };
    f.add(base, &messages[base..]);
    f.out
}

/// `loads`, worked out a part at a time.
#[derive(Clone, Default)]
pub struct Loads {
    out: Vec<Load>,
    /// The LOAD calls without a result yet: id, kit, message.
    open: Vec<(Value, String, usize)>,
}

impl Loads {
    /// Goes on with `messages`, the first of which is message `from`.
    fn add(&mut self, from: usize, messages: &[Value]) {
        for (k, m) in messages.iter().enumerate() {
            let k = from + k;
            for b in m["content"].as_array().into_iter().flatten() {
                if let Some(kit) = loads_kit(b) {
                    self.open.push((b["id"].clone(), kit, k));
                } else if b["type"] == "tool_result"
                    && let Some(i) = self.open.iter().position(|(id, ..)| *id == b["call_id"])
                {
                    let (_, kit, call) = self.open.swap_remove(i);
                    if b["is_error"] != true && !self.out.iter().any(|l| l.kit == kit) {
                        let through = b["content"].as_str().is_some_and(|t| t.contains(THROUGH_LOAD));
                        self.out.push(Load { kit, call, join: k + 1, through });
                    }
                }
            }
        }
    }
}

impl State {
    /// Holds what requests go with from now on (at the first request, or a
    /// summary on cut `cut`), which says all the system messages said: they
    /// go. The system prompt is rendered for it by the request (`system`).
    pub fn renew(&mut self, n: usize, cut: usize, now: &Pin) {
        self.pin = Some(Pin { system: String::new(), ..now.clone() });
        self.sys.clear();
        self.folded = n;
        self.cut = Some(cut);
    }

    /// The system prompt the pin holds, rendered by `render` from what it
    /// holds the first time (after a renewal, or for a pin an older Oak
    /// kept, rendered as that Oak did), then kept as it is.
    pub fn system(&mut self, render: impl FnOnce(&Pin) -> String) -> String {
        let Some(p) = self.pin.as_mut() else { return render(&Pin::default()) };
        if p.system.is_empty() {
            p.system = render(p);
        }
        p.system.clone()
    }

    /// After a renewal: the tools of the kits loaded in view, back after
    /// their LOADs, where they joined (`Join`). Where tools can't be added,
    /// a tool whose LOAD said it was in the list (the conversation moved
    /// from a model that takes them) is told in text, to be called through
    /// LOAD.
    fn rejoin(&mut self, s: &Setup) {
        for j in &s.joins {
            let Some(t) = s.now.tools.iter().find(|t| t["name"] == j.tool.as_str()) else { continue };
            let msg = SysMsg { at: j.at, kind: format!("tool:{}", j.tool), key: hash(t), ..Default::default() };
            if s.mode.inline_tools() {
                self.sys.push(SysMsg { tools: vec![t.clone()], ..msg });
            } else if !j.through {
                let text = format!("The {} tool isn't in your tool list here: {THROUGH_LOAD}, with name={} and the tool's input in `input`. It is:\n{t}", j.tool, j.kit);
                self.sys.push(SysMsg { text, ..msg });
            }
        }
        self.sys.sort_by_key(|m| m.at);
    }

    /// The transcript was cut back to `messages` (a revert): kits whose
    /// LOAD went are unloaded, and what was told after the cut goes; what
    /// stays is sent as it was. (A pin holds a kit whose LOAD went only if
    /// the revert went back past the summary that covered the LOAD: that
    /// summary goes, and its cut with it, which renews the pin.)
    pub fn reverted(&mut self, messages: &crate::store::Messages) {
        let n = messages.len();
        let loads = loads_of(messages);
        self.kits.retain(|k| loads.iter().any(|l| &l.kit == k));
        self.sys.retain(|m| m.at <= n);
        self.folded = self.folded.min(n);
    }

    /// What the model was last told of `kind` in the view after `cut`.
    fn said(&self, kind: &str, cut: usize) -> Option<&str> {
        self.sys.iter().rev().find(|s| s.kind == kind && in_view(s.at, cut)).map(|s| s.key.as_str())
    }

    /// For a request: what it goes with (the pin, renewed first when a
    /// summary acted since: nothing else renews it), with system messages
    /// added for what changed since (before transcript message `n`, after
    /// the user turn that ends it).
    pub fn prepare(&mut self, s: Setup) -> Pin {
        // A state from an older Oak: the cut it is on is its own.
        if self.pin.is_some() && self.cut.is_none() {
            self.cut = Some(s.cut);
        }
        // A summary acted (its cut moved, or covers a kit's LOAD).
        let summary = self.cut != Some(s.cut) || self.pin.as_ref().is_some_and(|p| s.base.kits.iter().any(|k| !p.kits.contains(k)));
        if self.pin.is_none() || summary {
            self.renew(s.n, s.cut, &s.base);
            self.rejoin(&s);
        }
        // What a pin from an older Oak didn't hold: what it held without
        // knowing (the folder, the tools and model as they are now, as an
        // older Oak sent them).
        if let Some(p) = self.pin.as_mut() {
            if p.folder.is_empty() {
                p.folder = s.now.folder.clone();
            }
            if p.tools.is_empty() {
                p.tools = s.base.tools.clone();
            }
            if p.model.is_empty() {
                p.model = s.now.model.clone();
            }
            // (A system prompt not rendered yet names the limit it is
            // rendered with; one an older Oak rendered names none, so the
            // model is told it below.)
            if p.notes_limit == 0 && p.system.is_empty() {
                p.notes_limit = s.now.notes_limit;
            }
        }
        let pin = self.pin.clone().unwrap_or_default();
        let (now, cut) = (&s.now, s.cut);
        let mut new = vec![];
        let msg = |kind: String, key: String| SysMsg { at: s.n, kind, key, ..Default::default() };
        if self.said("date", cut).unwrap_or(&pin.day) != now.day {
            new.push(SysMsg { text: format!("Today's date is now {}.", now.day), ..msg("date".into(), now.day.clone()) });
        }
        if !now.model.is_empty() && self.said("model", cut).unwrap_or(&pin.model) != now.model {
            new.push(SysMsg { text: format!("Your underlying model is now {}.", crate::api::model_name(&now.model)), ..msg("model".into(), now.model.clone()) });
        }
        // The "Notes limit" setting changed (none for a session without notes).
        let limit = now.notes_limit.to_string();
        let told = self.said("notes_limit", cut).map(str::to_string).unwrap_or_else(|| pin.notes_limit.to_string());
        if now.notes_limit != 0 && told != limit {
            let n = crate::agent::with_commas(now.notes_limit);
            let mut text = match told.parse::<u64>().unwrap_or(0) {
                0 => format!(
                    "Your notes are now limited to about {n} tokens in all: Oak refuses a new note, or an update that makes a note longer, \
                     that would take them past it."
                ),
                was => format!("The limit on your notes is now about {n} tokens in all (it was {}).", crate::agent::with_commas(was)),
            };
            if s.notes > now.notes_limit && crate::agent::notes_alert_on() {
                text += &format!(
                    " They take about {} now, over it: shorten or delete notes (NOTE action=update with less text, or action=delete) \
                     before adding one or making one longer.",
                    s.notes
                );
            }
            new.push(SysMsg { text, ..msg("notes_limit".into(), limit) });
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
                // A kit's tool is reached through LOAD (its result says so);
                // another (a model's own server serving fewer tools first)
                // joins the list with the next summary.
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
                // A text block at the end of the user turn before; the tools
                // a message told where they could be added, in text.
                let listed = |name: &str| self.pin.as_ref().is_some_and(|p| p.tools.iter().any(|t| t["name"] == name));
                let mut lines: Vec<String> = group.iter().flat_map(|s| &s.tools).map(|t| tool_text(t, listed(t["name"].as_str().unwrap_or("")))).collect();
                lines.extend(text.iter().map(|t| t.to_string()));
                remind(&mut out, i, &lines);
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
    /// last renewal says twice: a NOTE call's text (the session memory has
    /// the notes), and the result of a LOAD whose kit the pin holds (only a
    /// pin an older Oak renewed holds a kit whose LOAD is in view).
    pub fn fold(&self, view: &mut [Value], cut: usize, n: usize) {
        if self.folded <= cut {
            return;
        }
        let offset = view.len().saturating_sub(n.saturating_sub(cut));
        let end = (offset + self.folded - cut).min(view.len());
        let mut loads: Vec<(Value, String)> = vec![];
        let held = self.pin.as_ref().map(|p| p.kits.as_slice()).unwrap_or_default();
        for m in &mut view[offset..end] {
            let Some(content) = m["content"].as_array_mut() else { continue };
            for b in content {
                match b["type"].as_str() {
                    Some("tool_call") => {
                        if let Some(kit) = loads_kit(b).filter(|k| held.contains(k)) {
                            loads.push((b["id"].clone(), kit));
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

/// A tool definition a system message brought, told in text where system
/// messages can't go: `listed` if a tool of its name is in the tool list.
fn tool_text(def: &Value, listed: bool) -> String {
    let name = def["name"].as_str().unwrap_or("");
    if listed {
        format!("The {name} tool's definition is now:\n{def}")
    } else if crate::tools::kits_for(false).contains(&name) {
        format!("The {name} tool isn't in your tool list here: {THROUGH_LOAD}, with name={name} and the tool's input in `input`. It is:\n{def}")
    } else {
        format!("The {name} tool isn't in your tool list here, so it can't be called until earlier context is summarized. It is:\n{def}")
    }
}

/// `lines` as a system reminder: a text block at the end of the user turn
/// before message `i` of `out`, or a user turn of its own there.
fn remind(out: &mut Vec<Value>, i: usize, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let block = json!({"type": "text", "text": format!("<system-reminder>\n{}\n</system-reminder>", lines.join("\n"))});
    match out.get_mut(i.wrapping_sub(1)).filter(|m| m["role"] == "user") {
        Some(m) => m["content"].as_array_mut().into_iter().for_each(|c| c.push(block.clone())),
        None => out.insert(i, json!({"role": "user", "content": [block]})),
    }
}

/// `body` (Claude's) for an API that refuses its system messages: each
/// one's text and tools go as `inject` sends them in `Mode::Generic` (which
/// the session's later requests use), so what the model is told is still
/// there, where it was, and nothing sent changes after.
pub fn plain_body(mut body: Value) -> Value {
    let tools = body["tools"].as_array().cloned().unwrap_or_default();
    if let Some(messages) = body["messages"].as_array_mut() {
        *messages = plain_messages(std::mem::take(messages), &tools);
    }
    body
}

/// `messages` with their system messages (Oak's, as `inject` makes them in
/// `Mode::Claude` or `Mode::Codex`, or Claude's wire form) as `Mode::Generic`
/// sends them: for a provider that refuses them (`plain_body`, Codex).
pub fn plain_messages(messages: Vec<Value>, tools: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        if m["role"] != "system" {
            out.push(m);
            continue;
        }
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        let def = |b: &Value| match b["type"].as_str() {
            Some("tool_definition") => Some(b["definition"].clone()),
            Some("tool_addition") => Some(b["tool"]["definition"].clone()),
            _ => None,
        };
        let mut lines: Vec<String> = blocks.iter().filter_map(def).map(|d| tool_text(&d, tools.iter().any(|t| t["name"] == d["name"]))).collect();
        lines.extend(blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).filter(|t| !t.is_empty()).map(str::to_string));
        let i = out.len();
        remind(&mut out, i, &lines);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(models: &[&str]) -> Vec<Value> {
        vec![json!({"name": "READ"}), json!({"name": "AGENT", "input_schema": {"properties": {"model": {"enum": models}}}})]
    }

    fn now(day: &str, effort: &str, tools: Vec<Value>) -> Pin {
        Pin { day: day.into(), kits: vec![], effort: Some(effort.into()), tools, folder: "F".into(), model: "M".into(), system: String::new(), notes_limit: 0 }
    }

    fn setup(mode: Mode, n: usize, now: Pin) -> Setup {
        Setup { mode, n, cut: 0, add: true, effort_per_message: true, base: now.clone(), now, joins: vec![], notes: 0 }
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
        // Another model: told once, the prompt naming the first.
        let other = Pin { model: "N".into(), ..now("2026-11-07", "high", tools(&["a"])) };
        let p = s.prepare(setup(Mode::Claude, 13, other.clone()));
        assert_eq!((p.model.as_str(), s.sys.len()), ("M", 7));
        assert_eq!(s.sys[6].text, "Your underlying model is now N.");
        s.prepare(setup(Mode::Claude, 15, other));
        assert_eq!(s.sys.len(), 7);
        // Renewed only by a summary: everything as built, the messages gone.
        let p = s.prepare(Setup { cut: 10, ..setup(Mode::Claude, 17, now("2026-11-08", "high", t.clone())) });
        assert_eq!((p.day.as_str(), p.tools.clone(), s.sys.len(), s.folded, s.cut), ("2026-11-08", t, 0, 17, Some(10)));
    }

    /// The system prompt is rendered once per renewal, and kept as it was
    /// whatever the prompt would say now.
    #[test]
    fn the_system_prompt_is_kept_as_sent() {
        let mut s = State::default();
        s.prepare(setup(Mode::Claude, 1, now("2026-11-06", "high", tools(&["a"]))));
        assert_eq!(s.system(|p| format!("prompt on {} for {}", p.day, p.model)), "prompt on 2026-11-06 for M");
        s.prepare(setup(Mode::Claude, 3, Pin { model: "N".into(), ..now("2026-11-09", "low", tools(&["b"])) }));
        assert_eq!(s.system(|_| "another prompt".into()), "prompt on 2026-11-06 for M");
        // A summary renews it.
        s.prepare(Setup { cut: 2, ..setup(Mode::Claude, 5, now("2026-11-09", "low", tools(&["b"]))) });
        assert_eq!(s.system(|_| "another prompt".into()), "another prompt");
        // Saved and loaded: the same.
        let mut back: State = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.system(|_| "newer Oak".into()), "another prompt");
    }

    /// A conversation from an older Oak (a pin without tools, model, cut or
    /// system prompt; messages of another shape) keeps its pin: what it
    /// lacks is taken as it is now, which is what that Oak sent. Empty
    /// messages never go.
    #[test]
    fn older_state_keeps_its_pin() {
        let mut s: State = serde_json::from_value(json!({"pin": {"day": "2026-11-05", "agent": {"name": "AGENT"}}, "sys": [{"at": 1, "kind": "date", "key": "x", "content": []}], "folded": 1})).unwrap();
        let out = s.inject(Mode::Claude, vec![m("user", "q0"), m("assistant", "a1")], 0, 2);
        assert_eq!(out.len(), 2);
        let p = s.prepare(Setup { cut: 0, ..setup(Mode::Claude, 3, now("2026-11-05", "high", tools(&["a"]))) });
        // (What it told is kept; the date it told comes again.)
        assert_eq!((p.tools, p.model.as_str(), s.sys[0].key.as_str(), s.folded, s.cut), (tools(&["a"]), "M", "x", 1, Some(0)));
        // A state saved on a summary's cut by an older Oak: that cut is its own.
        let mut s: State = serde_json::from_value(json!({"pin": {"day": "2026-11-05", "tools": [{"name": "READ"}]}, "folded": 9})).unwrap();
        s.prepare(Setup { cut: 6, ..setup(Mode::Claude, 12, now("2026-11-05", "high", tools(&["a"]))) });
        assert_eq!((s.folded, s.cut), (9, Some(6)));
    }

    /// Without a user turn to follow, nothing is added (it waits).
    /// The "Notes limit" setting changing: a message (with what the notes
    /// take when they are over it), once per change; the system prompt
    /// stays as it was until a summary.
    #[test]
    fn a_new_notes_limit_is_a_message() {
        let limit = |n| Pin { notes_limit: n, ..now("2026-11-06", "high", vec![]) };
        let mut s = State::default();
        let p = s.prepare(setup(Mode::Claude, 1, limit(2_000)));
        assert_eq!(p.notes_limit, 2_000);
        assert_eq!(s.system(|p| format!("notes up to {}", p.notes_limit)), "notes up to 2000");
        s.prepare(setup(Mode::Claude, 3, limit(2_000)));
        assert!(s.sys.is_empty());
        let p = s.prepare(Setup { notes: 1_500, ..setup(Mode::Claude, 5, limit(1_000)) });
        assert_eq!(p.notes_limit, 2_000, "the pin keeps what the system prompt says");
        assert_eq!(s.system(|_| "another prompt".into()), "notes up to 2000");
        assert_eq!((s.sys.len(), s.sys[0].at, s.sys[0].kind.as_str(), s.sys[0].key.as_str()), (1, 5, "notes_limit", "1000"));
        assert_eq!(
            s.sys[0].text,
            "The limit on your notes is now about 1,000 tokens in all (it was 2,000). They take about 1500 now, over it: shorten or \
             delete notes (NOTE action=update with less text, or action=delete) before adding one or making one longer."
        );
        // Told once; told again only of another change, back included.
        s.prepare(Setup { notes: 1_500, ..setup(Mode::Claude, 7, limit(1_000)) });
        assert_eq!(s.sys.len(), 1);
        s.prepare(Setup { notes: 900, ..setup(Mode::Claude, 9, limit(2_000)) });
        assert_eq!(s.sys[1].text, "The limit on your notes is now about 2,000 tokens in all (it was 1,000).");
        // Saved and loaded: the same.
        let mut s: State = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        s.prepare(setup(Mode::Claude, 11, limit(2_000)));
        assert_eq!(s.sys.len(), 2);
        // A session moved to a model without notes is told nothing.
        s.prepare(setup(Mode::Claude, 13, limit(0)));
        assert_eq!(s.sys.len(), 2);
        // A summary renders the system prompt with the limit now.
        let p = s.prepare(Setup { cut: 12, ..setup(Mode::Claude, 15, limit(4_000)) });
        assert_eq!((p.notes_limit, s.sys.len()), (4_000, 0));
        assert_eq!(s.system(|p| format!("notes up to {}", p.notes_limit)), "notes up to 4000");
    }

    /// A system prompt an older Oak sent named no limit: the model is told
    /// it in a message. One not rendered yet names it.
    #[test]
    fn an_older_pin_is_told_the_notes_limit() {
        let older = |system: &str| State { pin: Some(Pin { system: system.into(), ..now("2026-11-06", "high", vec![]) }), ..Default::default() };
        let mut s = older("an older prompt");
        let p = s.prepare(setup(Mode::Claude, 3, Pin { notes_limit: 2_000, ..now("2026-11-06", "high", vec![]) }));
        assert_eq!(p.notes_limit, 0);
        assert_eq!(s.sys.len(), 1);
        assert!(s.sys[0].text.starts_with("Your notes are now limited to about 2,000 tokens in all"), "{}", s.sys[0].text);
        let mut s = older("");
        let p = s.prepare(setup(Mode::Claude, 3, Pin { notes_limit: 2_000, ..now("2026-11-06", "high", vec![]) }));
        assert_eq!((p.notes_limit, s.sys.len()), (2_000, 0));
    }

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

    /// Before the renewal, the LOAD results of kits the pin holds are
    /// folded (a pin an older Oak renewed): a kit the pin doesn't hold
    /// keeps its LOAD's result, its instructions. NOTE calls keep their
    /// text.
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
        let before = view.clone();
        let s = State { folded: 5, ..Default::default() };
        s.fold(&mut view, 0, 7);
        assert_eq!(view[2]["content"][0]["content"], "long instructions", "not in the prefix");
        assert_eq!(view[3]["content"][0]["input"]["text"], "fact");
        let mut view = before;
        let s = State { folded: 5, pin: Some(Pin { kits: vec!["BROWSER".into(), "AGENT".into()], ..Default::default() }), ..Default::default() };
        s.fold(&mut view, 0, 7);
        assert_eq!(view[2]["content"][0]["content"], "Loaded BROWSER: it is in your tools and system prompt now.");
        assert_eq!(view[6]["content"][0]["content"], "more instructions", "after the renewal");
    }

    /// Each kit's first LOAD that worked, and where its tool joins: after
    /// its result. Calls through LOAD and failed LOADs don't count.
    #[test]
    fn loads_are_found_in_the_transcript() {
        let call = |id: &str, input: Value| json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": "LOAD", "input": input}]});
        let result = |id: &str, t: &str, error: bool| json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": t, "is_error": error}]});
        let messages = vec![
            m("user", "go"),
            call("x", json!({"name": "nope"})),
            result("x", "No kit nope.", true),
            call("y", json!({"name": "BROWSER", "input": {"action": "open"}})),
            result("y", "opened", false),
            call("a", json!({"name": " agent "})),
            result("a", "Loaded AGENT. Its tool isn't in your tool list: call it through LOAD, with name=AGENT", false),
            call("b", json!({"name": "AGENT"})),
            result("b", "AGENT is loaded already.", false),
            call("c", json!({"name": "BROWSER", "input": null})),
            result("c", "Loaded BROWSER. The BROWSER tool is in your tool list now.", false),
        ];
        assert_eq!(
            loads(&messages),
            [Load { kit: "AGENT".into(), call: 5, join: 7, through: true }, Load { kit: "BROWSER".into(), call: 9, join: 11, through: false }]
        );
    }

    /// A renewal holds `base` (the kits a summary covers) and puts the
    /// other kits' tools back where they joined, in order with the rest.
    #[test]
    fn a_renewal_puts_kit_tools_back_after_their_loads() {
        let t = |n: &str| json!({"name": n});
        let now = Pin { day: "d".into(), kits: vec!["AGENT".into(), "BROWSER".into()], tools: vec![t("READ"), t("AGENT"), t("BROWSER")], ..Default::default() };
        let base = Pin { kits: vec!["AGENT".into()], tools: vec![t("READ"), t("AGENT")], ..now.clone() };
        let joins = vec![Join { tool: "BROWSER".into(), kit: "BROWSER".into(), at: 4, through: false }];
        let mut s = State::default();
        let pin = s.prepare(Setup { base: base.clone(), joins: joins.clone(), ..setup(Mode::Claude, 9, now.clone()) });
        assert_eq!(pin, base);
        assert_eq!(s.sys.iter().map(|m| (m.at, m.kind.as_str())).collect::<Vec<_>>(), [(4, "tool:BROWSER")]);
        assert_eq!(s.sys[0].tools, [t("BROWSER")]);
        // Warm, nothing changed: nothing more.
        s.prepare(Setup { base: base.clone(), joins: joins.clone(), ..setup(Mode::Claude, 11, now.clone()) });
        assert_eq!(s.sys.len(), 1);
        // A summary that doesn't cover the LOAD: the same.
        s.prepare(Setup { cut: 2, base: base.clone(), joins: joins.clone(), ..setup(Mode::Claude, 13, now.clone()) });
        assert_eq!((s.sys.len(), s.sys[0].at), (1, 4));
        // A summary now covers BROWSER's LOAD too: both kits held.
        let s2 = s.prepare(Setup { cut: 6, base: now.clone(), ..setup(Mode::Claude, 15, now.clone()) });
        assert_eq!(s2, now);
        assert!(s.sys.is_empty());
        // Where tools can't be added, a LOAD that said the tool was in the
        // list gets a text telling to call it through LOAD.
        let mut g = State::default();
        g.prepare(Setup { base: base.clone(), joins: joins.clone(), ..setup(Mode::Generic, 9, now.clone()) });
        assert!(g.sys[0].text.starts_with("The BROWSER tool isn't in your tool list here: call it through LOAD, with name=BROWSER"));
        let mut g = State::default();
        let through = vec![Join { through: true, ..joins[0].clone() }];
        g.prepare(Setup { base, joins: through, ..setup(Mode::Generic, 9, now) });
        assert!(g.sys.is_empty());
    }

    /// A revert unloads the kits whose LOADs it took and drops what was
    /// told after its point; the pin stays (only a summary renews it).
    #[test]
    fn a_revert_unloads_kits_and_drops_later_messages() {
        let call = json!({"role": "assistant", "content": [{"type": "tool_call", "id": "a", "name": "LOAD", "input": {"name": "AGENT"}}]});
        let result = json!({"role": "user", "content": [{"type": "tool_result", "call_id": "a", "content": "Loaded AGENT."}]});
        let mut s = State {
            kits: vec!["AGENT".into(), "BROWSER".into()],
            sys: vec![SysMsg { at: 3, ..Default::default() }, SysMsg { at: 7, ..Default::default() }],
            folded: 7,
            pin: Some(Pin { kits: vec!["BROWSER".into()], ..Default::default() }),
            cut: Some(0),
        };
        s.reverted(&vec![m("user", "go"), call, result].into());
        assert_eq!(s.kits, ["AGENT"]);
        assert_eq!(s.sys.len(), 1);
        assert_eq!(s.folded, 3);
        assert!(s.pin.is_some());
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

    /// Where the API refuses system messages, what they said goes as the
    /// text `Mode::Generic` sends, byte for byte, so the session's later
    /// requests (in that mode) send the same.
    #[test]
    fn a_plain_body_says_what_its_system_messages_said() {
        let browser = json!({"name": "BROWSER", "input_schema": {}});
        let state = State {
            pin: Some(Pin { tools: vec![json!({"name": "READ"})], ..Default::default() }),
            sys: vec![sys(1, "Today's date is now d."), SysMsg { at: 3, kind: "tool:BROWSER".into(), key: "k".into(), tools: vec![browser.clone()], ..Default::default() }],
            ..Default::default()
        };
        let view = vec![m("user", "q0"), m("assistant", "a1"), m("user", "q2"), m("assistant", "a3")];
        let generic = state.inject(Mode::Generic, view.clone(), 0, 4);
        let claude = state.inject(Mode::Claude, view, 0, 4);
        let wire = |v: &[Value]| json!({"tools": [{"name": "READ"}], "messages": crate::wire_anthropic::messages(v, "anthropic", true)});
        assert_eq!(plain_body(wire(&claude)), wire(&generic));
        let told = generic[2]["content"][1]["text"].as_str().unwrap();
        assert!(told.starts_with("<system-reminder>\nThe BROWSER tool isn't in your tool list here: call it through LOAD"), "{told}");
    }
}
