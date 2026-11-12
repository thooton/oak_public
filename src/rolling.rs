// Smart context: once a conversation passes a trigger size, the older part
// is summarized in the background while the agent keeps working, and requests
// from then on send that summary followed by the newest part of the transcript
// verbatim. Each later summary folds in the previous one, so the model always
// has one running timeline plus a verbatim tail.
//
// Cost: the summary request is the chat request that just finished, resent
// with the summary instruction appended to its last message, so the whole
// prefix is read from the prompt cache and only the instruction is new input.
//
// The transcript on disk is never rewritten; only what is sent changes.

use crate::tools::{Head, one_line};
use crate::api::{self, Sse};
use crate::oak::{Event, Request};
use crate::providers::Kind;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// Summarize once the context is bigger than this (tokens).
    pub trigger: u64,
    /// About this much of the newest context stays verbatim (tokens).
    pub keep: u64,
}

pub const DEFAULT: Config = Config { trigger: 120_000, keep: 40_000 };
static CONFIG: Mutex<Config> = Mutex::new(DEFAULT);

pub fn config() -> Config {
    *CONFIG.lock().unwrap()
}

pub fn set_config(c: Config) {
    let trigger = c.trigger.clamp(MIN_TRIGGER, MAX_TRIGGER);
    *CONFIG.lock().unwrap() = Config { trigger, keep: c.keep.clamp(5_000, trigger * 3 / 4) };
}

const MIN_TRIGGER: u64 = 20_000;
const MAX_TRIGGER: u64 = 2_000_000;

/// The highest trigger a model with a `window`-token context allows: 90%
/// of the window (to the 5k below), leaving room for the context to grow
/// while a summary is written. Unknown windows aren't capped.
pub fn max_trigger(window: Option<u64>) -> u64 {
    window.map_or(MAX_TRIGGER, |w| w * 9 / 10 / 5_000 * 5_000).clamp(MIN_TRIGGER, MAX_TRIGGER)
}

/// `c` within the cap: the trigger at most `max`, the keep at most three
/// quarters of the trigger.
pub fn capped(c: Config, max: u64) -> Config {
    let trigger = c.trigger.min(max);
    Config { trigger, keep: c.keep.min(trigger * 3 / 4) }
}

/// The sizes as they apply to a conversation on `model`: the settings,
/// capped by its context window.
///
/// A model on the user's own server (a short window, and slow to reread)
/// summarizes at the cap, keeping at most a quarter of it verbatim, so a
/// summary leaves room for real work.
pub fn config_for(model: &str) -> Config {
    let c = capped(config(), max_trigger(crate::api::context_window(model)));
    if crate::local::serves(model) {
        return Config { trigger: c.trigger, keep: c.keep.min(c.trigger / 4).max(2_000) };
    }
    c
}

/// Who writes a local model's summaries (a setting).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Summarizer {
    /// The model itself, thinking off: free and private; the session waits.
    Local,
    /// DeepSeek through the OpenRouter key.
    DeepSeek,
    /// The newest Haiku, through a Claude account.
    Haiku,
}

impl Summarizer {
    pub const ALL: [Summarizer; 3] = [Summarizer::Local, Summarizer::DeepSeek, Summarizer::Haiku];

    /// The settings id and the label shown for it.
    fn names(self) -> (&'static str, &'static str) {
        match self {
            Summarizer::Local => ("local", "Local model"),
            Summarizer::DeepSeek => ("deepseek", "DeepSeek"),
            Summarizer::Haiku => ("haiku", "Haiku"),
        }
    }

    pub fn id(self) -> &'static str {
        self.names().0
    }

    pub fn label(self) -> &'static str {
        self.names().1
    }

    pub fn from_id(id: &str) -> Summarizer {
        Summarizer::ALL.into_iter().find(|s| s.id() == id).unwrap_or(Summarizer::Local)
    }
}

static SUMMARIZER: Mutex<Summarizer> = Mutex::new(Summarizer::Local);

pub fn summarizer() -> Summarizer {
    *SUMMARIZER.lock().unwrap()
}

pub fn set_summarizer(s: Summarizer) {
    *SUMMARIZER.lock().unwrap() = s;
}

/// A local model's summary request as the cloud model picked in Settings
/// takes it (`summarizer`), when it is that and the model can be reached:
/// the provider and model swapped (the local model's reasoning stays out:
/// it is another provider's) and the cloud model's thinking off.
fn cloud_summary(req: &Request) -> Option<Request> {
    if req.provider != Kind::Local {
        return None;
    }
    let (provider, model) = match summarizer() {
        Summarizer::Local => return None,
        Summarizer::DeepSeek => (Kind::OpenRouter, crate::openrouter::on().then(|| crate::openrouter::MODELS[0].id.to_string())?),
        Summarizer::Haiku => (Kind::Claude, crate::agent::title::haiku()?),
    };
    let mut req = req.clone();
    req.provider = provider;
    req.model = model;
    if provider == Kind::OpenRouter {
        req.effort = Some("low".into());
    } else {
        req.effort = None;
        req.thinking = false;
    }
    Some(req)
}

/// A size as typed into the settings: thousands, "120" or "1.5".
pub fn fmt_thousands(n: u64) -> String {
    let k = n as f64 / 1000.0;
    if k.fract() == 0.0 { format!("{k:.0}") } else { format!("{}", (k * 10.0).round() / 10.0) }
}

// ---------------------------------------------------------------- choosing the sizes
//
// What the two sizes do to usage, in units of one fresh input token, at the
// conversation's model's API prices (`Rates`): a cache read costs r of one
// (0.1 on most models, 0.05 on Opus and Sonnet 5.5), a cache write w (1.25
// at the five-minute TTL, 2 at the one-hour one, `cache_ttl`), an output
// token o (5). Usage limits follow cost.
//
// Between summaries every request rereads the whole context from the cache,
// so each costs r of the context. A summary costs its own output, one read
// of the context, and writing the new view (summary plus kept part) to the
// cache again on the next request. With D the growth between two summaries
// and g what a request adds, per token the conversation grows by:
//
//   r · (leftover + D/2) / g  +  ((w + r) · (summary + keep) + r · overhead + o · summary output) / D
//
// The two terms balance at D = sqrt(2 · g · ((w + r) · (summary + keep) +
// r · overhead + o · summary output) / r), which is the best trigger for a
// keep. A smaller keep is always cheaper; too low a trigger costs more than
// as much too high. The sizes below were measured over OAK's conversations.
/// What a request adds to the context on average (tokens; the median is
/// about 1000, big tool results pull the mean up).
const GROWTH: f64 = 1_500.0;
/// A summary, as it sits in the context.
const SUMMARY: f64 = 3_600.0;
/// What writing a summary generates (thinking included).
const SUMMARY_OUT: f64 = 6_000.0;
/// The system prompt and tools.
const OVERHEAD: f64 = 5_000.0;

/// A model's prices relative to a fresh input token: a cache read, a cache
/// write (at the TTL in use) and an output token; and whether summaries clear
/// old tool output, which shrinks what they leave in the context.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub read: f64,
    pub write: f64,
    pub out: f64,
    pub clear: bool,
}

/// `model`'s rates, from its API list prices (models without a Claude price,
/// such as OpenRouter's, are priced as the newest Opus).
pub fn rates(model: &str) -> Rates {
    let p = crate::usage::price_of(model, 0);
    let write = if crate::api::cache_1h() { p.w1h } else { p.w5m };
    Rates { read: p.read / p.input, write: write / p.input, out: p.out / p.input, clear: true }
}

/// What a summary costs apart from the rereading it saves (see above).
fn rollover(keep: f64, r: Rates) -> f64 {
    (r.write + r.read) * (SUMMARY + in_context(keep, r.clear)) + r.read * OVERHEAD + r.out * SUMMARY_OUT
}

/// The usage a pair of sizes costs per token of growth, apart from what
/// every pair pays alike (writing the new tokens, the replies).
pub fn cost(c: Config, r: Rates) -> f64 {
    let floor = OVERHEAD + SUMMARY + in_context(c.keep as f64, r.clear);
    let d = c.trigger as f64 - floor;
    if d <= 0.0 {
        return f64::INFINITY;
    }
    r.read * (floor + d / 2.0) / GROWTH + rollover(c.keep as f64, r) / d
}

fn best_trigger_exact(keep: f64, r: Rates) -> f64 {
    OVERHEAD + SUMMARY + in_context(keep, r.clear) + (2.0 * GROWTH * rollover(keep, r) / r.read).sqrt()
}

/// To the nearest 5k.
fn round5k(n: f64) -> u64 {
    ((n / 5_000.0).round() as u64 * 5_000).max(5_000)
}

/// The trigger that costs least with `keep` kept (on Opus 5.5, keep 40k:
/// 120k, 100k: 210k; on models whose cache reads cost 0.1 of input, 100k
/// and 180k).
pub fn best_trigger(keep: u64, r: Rates) -> u64 {
    round5k(best_trigger_exact(keep as f64, r)).clamp(MIN_TRIGGER, MAX_TRIGGER)
}

/// The keep `trigger` costs least with: the most a summary can leave
/// verbatim for that trigger to still pay off.
pub fn best_keep(trigger: u64, r: Rates) -> u64 {
    let (mut lo, mut hi) = (0.0, trigger as f64);
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if best_trigger_exact(mid, r) < trigger as f64 { lo = mid } else { hi = mid }
    }
    round5k(lo).clamp(5_000, trigger * 3 / 4)
}

/// Which size was changed last; the suggestion is for the other one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Trigger,
    Keep,
}

/// The trigger costs at most 5% more than the best one for the keep.
fn near_best(c: Config, r: Rates) -> bool {
    cost(c, r) <= 1.05 * cost(Config { trigger: best_trigger(c.keep, r), ..c }, r)
}

/// The pair that suits the side `changed`: the other side moved to cost
/// least with it (`c` itself when it already does).
pub fn paired(c: Config, changed: Side, r: Rates) -> Config {
    let keep = Config { keep: best_keep(c.trigger, r), ..c };
    // A trigger too small for any keep is raised instead.
    if changed == Side::Trigger && cost(keep, r).is_finite() && (keep != c || near_best(c, r)) {
        return keep;
    }
    Config { trigger: best_trigger(c.keep, r), ..c }
}

/// When the sizes don't suit each other (the trigger costs over 5% more
/// than the best one for the keep), the pair to suggest instead: the
/// side that wasn't `changed` moved to suit the one that was.
pub fn suggestion(c: Config, changed: Side, r: Rates) -> Option<Config> {
    if near_best(c, r) { None } else { Some(paired(c, changed, r)) }
}

/// A prompt size the API reported: the request sent messages `..n` of the
/// transcript on top of the summary that ended at `base`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub base: usize,
    pub n: usize,
    pub prompt: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    /// Transcript messages before this index are replaced by the summary.
    pub cut: usize,
    pub text: String,
    pub made_at: i64,
    /// Old tool output in messages `cut..clear` is cleared (see `view`).
    pub clear: usize,
}

/// A conversation's smart-context state, saved with it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Turned off for this conversation.
    pub off: bool,
    pub summary: Option<Summary>,
    /// Prompt sizes measured on the current summary (cleared when it changes).
    pub measured: Vec<Measure>,
    /// The context after the latest reply (its prompt plus the reply), in tokens.
    pub context: u64,
    pub compressions: u32,
    /// Tokens taken out of the context by all compressions, estimated.
    pub saved: u64,
    /// What the latest summary request cost.
    pub last_cost: Option<Cost>,
    /// Why the latest summary failed, if it did (cleared by the next one that works).
    pub last_error: Option<String>,
    /// Old tool output cleared while the cache was cold anyway, between
    /// summaries (`Clearing`).
    pub cold: Clearing,
}

/// Tool output cleared at a moment the prompt cache was cold anyway (it
/// expired, or the conversation moved to another account or model), when
/// clearing costs nothing: the next request writes the whole context
/// either way. For the view after the summary ending at `base` only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Clearing {
    pub base: usize,
    /// Old tool output in messages `base..to` is cleared, as a summary
    /// clears it.
    pub to: usize,
    /// A READ's output in messages `base..sup` is cleared when the same
    /// READ (same file and lines) came again later, before `sup`.
    pub sup: usize,
}

impl State {
    /// The summary requests use: none while smart context is off (it is
    /// kept, so turning it back on doesn't pay for it again).
    pub fn active(&self) -> Option<&Summary> {
        self.summary.as_ref().filter(|_| !self.off)
    }

    pub fn base(&self) -> usize {
        self.active().map_or(0, |s| s.cut)
    }

    /// The cold-moment clearing in force: the one made on the view in use.
    pub fn cold_clearing(&self) -> Option<&Clearing> {
        Some(&self.cold).filter(|c| !self.off && c.base == self.base() && (c.to > c.base || c.sup > c.base))
    }

    /// Messages before this index have their old tool output cleared (by the
    /// summary in use or at a cold moment).
    pub fn cleared_to(&self) -> usize {
        let by_summary = self.active().map_or(0, |s| s.clear);
        by_summary.max(self.cold_clearing().map_or(0, |c| c.to))
    }

    /// `messages` (the transcript) as requests send them (`view`).
    pub fn view(&self, messages: &[Value]) -> Vec<Value> {
        view_with(messages, self.active(), self.cold_clearing())
    }

    /// The tool calls whose output requests send as `CLEARED`.
    pub fn cleared_calls<'a>(&self, messages: &'a [Value]) -> Ids<'a> {
        cleared_set(messages, self.active(), self.cold_clearing())
    }

    /// At a moment the cache is cold anyway: clears old tool output in
    /// `messages` all but the newest `RECENT` tokens', and READ output a
    /// later identical READ superseded. Returns whether anything more is
    /// cleared than was.
    pub fn clear_cold(&mut self, messages: &[Value]) -> bool {
        let (base, n) = (self.base(), messages.len());
        if self.off || n < base + 3 {
            return false;
        }
        let now = cleared_set(messages, self.active(), self.cold_clearing()).len();
        let mut back = 0;
        let mut to = base;
        for k in (base..n).rev() {
            back += chars(&messages[k]);
            if estimate(back) >= RECENT {
                to = k;
                break;
            }
        }
        let old = self.cold_clearing().copied().unwrap_or(Clearing { base, to: base, sup: base });
        let next = Clearing { base, to: old.to.max(to), sup: old.sup.max(n) };
        if cleared_set(messages, self.active(), Some(&next)).len() <= now {
            return false;
        }
        self.cold = next;
        // The sizes measured were of the view before.
        self.measured.clear();
        true
    }

    pub fn record(&mut self, m: Measure, output: u64) {
        self.context = m.prompt + output;
        if m.base != self.base() {
            return; // measured on a summary that has since been replaced
        }
        self.measured.retain(|x| x.n != m.n);
        self.measured.push(m);
    }

    /// Installs a new summary; the measurements were for the old view.
    pub fn apply(&mut self, summary: Summary, saved: u64, cost: Cost) {
        self.summary = Some(summary);
        self.last_cost = Some(cost);
        self.last_error = None;
        self.measured.clear();
        self.compressions += 1;
        self.saved += saved;
        // Until the next request measures it.
        self.context = self.context.saturating_sub(saved);
    }
}

// ---------------------------------------------------------------- the view

const SUMMARY_OPEN: &str = "[Summary of the earlier part of this conversation]";
const SUMMARY_CLOSE: &str = "[End of summary]";
const ACK: &str = "Understood. I have the earlier context from the summary and will continue from where we left off.";

fn summary_message(text: &str) -> Value {
    let text = format!(
        "{SUMMARY_OPEN}\n{text}\n{SUMMARY_CLOSE}\n\nThe earlier messages of this conversation were condensed into the summary above to save context; \
         everything after it is verbatim. Continue from where we left off."
    );
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

// ---------------------------------------------------------------- clearing old tool output
//
// Tool output is most of a conversation (about 65% of OAK's, mostly files
// read and command output) and the least needed once it is old: the model's
// reasoning and its calls stay, so it knows what it saw and can look again.
// So when a summary is made, tool results in the verbatim part are cleared
// too, all but those in its newest `RECENT` tokens; and so they are when the
// cache is cold anyway (it expired, or the conversation moved to another
// account or model: `State::clear_cold`), along with READ output a later
// identical READ superseded. Only then: clearing at any other time would
// make the next request write the whole context to the cache again.
//
// RECENT was checked by replaying OAK's conversations (Nov 2026, 200k/125k,
// clearing at cold moments): 10k saves 1.7% but leaves 2 points fewer of the
// addresses the model went back to still in view; 40k costs 4.4% more for
// no gain in that measure; 20k is the knee.

/// Tool output in about this many of the newest tokens is never cleared.
pub const RECENT: u64 = 20_000;
/// The share of a stretch of conversation left once its tool output is
/// cleared (measured over OAK's own conversations).
const LEFT: f64 = 0.35;
/// Results this short stay (characters): they cost little and say a lot
/// ("exit code 1").
const SMALL: usize = 600;
pub const CLEARED: &str = "[Old tool output, cleared to save context. Run the tool again if you need it.]";
/// Tools whose output running them again wouldn't bring back: the user's
/// answers, other agents' messages, a task's output already read.
const KEPT_TOOLS: &[&str] = &["ASK", "AGENT_NEW", "AGENT_SEND", "AGENT_WAIT", "TASK_WAIT", "TASK_GET_NEW_OUTPUT"];


/// Which tool each call id in `messages` went to.
fn tool_names(messages: &[Value]) -> Names<'_> {
    let mut names = Names::default();
    for b in messages.iter().filter_map(|m| m["content"].as_array()).flatten() {
        if b["type"] == "tool_call"
            && let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str())
        {
            // A family's call (`TASK` with an action) as its action's tool.
            names.insert(id, crate::tools::canonical_name(name, &b["input"]));
        }
    }
    names
}

/// Whether tool result `b` would be cleared.
fn clearable(b: &Value, names: &Names) -> bool {
    b["type"] == "tool_result"
        && block_chars(b) > SMALL
        && !b["call_id"].as_str().and_then(|id| names.get(id)).is_some_and(|n| KEPT_TOOLS.contains(n))
}

/// The characters clearing takes out of message `m`.
fn clearable_chars(m: &Value, names: &Names) -> usize {
    m["content"].as_array().into_iter().flatten().filter(|b| clearable(b, names)).map(|b| block_chars(b) - CLEARED.len()).sum()
}

/// The summary `view` sends, if it covers any of `messages`.
fn in_use<'a>(messages: &[Value], summary: Option<&'a Summary>) -> Option<&'a Summary> {
    summary.filter(|s| s.cut > 0 && s.cut < messages.len())
}

/// The tool calls whose output the model sees as `CLEARED` (see `view`).
#[cfg(test)]
pub fn cleared_calls<'a>(messages: &'a [Value], summary: Option<&Summary>) -> Ids<'a> {
    cleared_set(messages, summary, None)
}

/// Tool call ids, borrowed from the transcript (hashed quickly: the
/// transcript works out which are cleared every frame).
pub type Ids<'a> = std::collections::HashSet<&'a str, crate::QuickState>;

/// Each tool call's tool, by call id (`tool_names`).
type Names<'a> = HashMap<&'a str, &'a str, crate::QuickState>;

/// What a READ call reads (its file, lines and all), the same for calls
/// that read the same thing.
fn read_key(input: &Value) -> String {
    let path = input["path"].as_str().unwrap_or("").replace('\\', "/").to_lowercase();
    format!("{path}|{}|{}", input["offset"], input["limit"])
}

/// The tool calls whose output the view clears: old output in the part a
/// summary or a cold moment cleared (`Summary::clear`, `Clearing::to`), and
/// READs a later identical READ superseded (`Clearing::sup`).
/// Borrowed from `messages`, in one pass over them: the transcript takes it
/// every frame.
fn cleared_set<'a>(messages: &'a [Value], summary: Option<&Summary>, cold: Option<&Clearing>) -> Ids<'a> {
    let n = messages.len();
    let s = in_use(messages, summary);
    let cut = s.map_or(0, |s| s.cut);
    let cold = cold.filter(|c| c.base == cut);
    let to = s.map_or(cut, |s| s.clear).max(cold.map_or(0, |c| c.to)).min(n).max(cut);
    let sup = cold.map_or(cut, |c| c.sup.min(n).max(cut));
    // Nothing cleared: no calls to name.
    if to == cut && sup == cut {
        return Default::default();
    }
    // Every call's tool from the cut on (`tool_names`), the READs up to
    // `sup` with what each reads, and the results up to `to` or `sup` big
    // enough to clear (with the message each is in).
    let mut names = Names::default();
    let mut reads: Vec<(&str, String)> = vec![];
    let mut big: Vec<(usize, &str)> = vec![];
    for (k, m) in messages.iter().enumerate().skip(cut) {
        for b in m["content"].as_array().into_iter().flatten() {
            match b["type"].as_str() {
                Some("tool_call") => {
                    let id = b["id"].as_str();
                    if let (Some(id), Some(name)) = (id, b["name"].as_str()) {
                        names.insert(id, crate::tools::canonical_name(name, &b["input"]));
                    }
                    if k < sup && b["name"] == "READ"
                        && let Some(id) = id
                    {
                        reads.push((id, read_key(&b["input"])));
                    }
                }
                Some("tool_result") if k < to.max(sup) && block_chars(b) > SMALL => big.extend(b["call_id"].as_str().map(|id| (k, id))),
                _ => {}
            }
        }
    }
    let kept = |id: &str| names.get(id).is_some_and(|n| KEPT_TOOLS.contains(n));
    let mut set: Ids = big.iter().filter(|(k, id)| *k < to && !kept(id)).map(|(_, id)| *id).collect();
    // The READs a later one reading the same thing superseded.
    let mut seen = std::collections::HashSet::new();
    let old: Ids = reads.into_iter().rev().filter_map(|(id, key)| (!seen.insert(key)).then_some(id)).collect();
    if !old.is_empty() {
        set.extend(big.iter().filter(|(k, id)| *k < sup && !kept(id) && old.contains(id)).map(|(_, id)| *id));
    }
    set
}

/// What `keep` tokens of conversation take in the context once the summary
/// that keeps them has cleared their old tool output.
fn in_context(keep: f64, clear: bool) -> f64 {
    let recent = RECENT as f64;
    if clear && keep > recent { recent + (keep - recent) * LEFT } else { keep }
}

/// The messages to send: the summary, then the transcript from the cut on
/// (old tool output cleared). A cut at a user message gets an
/// acknowledgement in between so the roles still alternate; a cut at an
/// assistant message follows the summary directly (the tool results before
/// it answered calls that were summarized).
#[cfg(test)]
pub fn view(messages: &[Value], summary: Option<&Summary>) -> Vec<Value> {
    view_with(messages, summary, None)
}

/// `view`, with what a cold moment cleared (`Clearing`) cleared too.
pub fn view_with(messages: &[Value], summary: Option<&Summary>, cold: Option<&Clearing>) -> Vec<Value> {
    let cleared = cleared_set(messages, summary, cold);
    // A copy of each message, with its cleared tool output as `CLEARED`
    // (never copied first: in a long conversation that is most of it).
    let copy = |m: &Value| -> Value {
        let Some(blocks) = m["content"].as_array().filter(|_| !cleared.is_empty()) else { return m.clone() };
        let is_cleared = |b: &Value| b["type"] == "tool_result" && b["call_id"].as_str().is_some_and(|id| cleared.contains(id));
        if !blocks.iter().any(is_cleared) {
            return m.clone();
        }
        // `o` without its content, which `content` then is.
        let with = |o: &Value, content: Value| {
            let mut o: serde_json::Map<String, Value> = o.as_object().into_iter().flatten().filter(|(k, _)| *k != "content").map(|(k, v)| (k.clone(), v.clone())).collect();
            o.insert("content".into(), content);
            Value::Object(o)
        };
        with(m, blocks.iter().map(|b| if is_cleared(b) { with(b, json!(CLEARED)) } else { b.clone() }).collect())
    };
    let v = match in_use(messages, summary) {
        None => messages.iter().map(copy).collect(),
        Some(s) => {
            let mut v = vec![summary_message(&s.text)];
            if messages[s.cut]["role"] == "user" {
                v.push(json!({"role": "assistant", "content": [{"type": "text", "text": ACK}]}));
            }
            v.extend(messages[s.cut..].iter().map(copy));
            v
        }
    };
    strip_oak(v)
}

/// Takes OAK's own keys out of `v` (`oak::clean`: say, how long a thinking
/// block took). BASH calls from before the tool was renamed go as EXECUTE,
/// the tool there is now.
fn strip_oak(v: Vec<Value>) -> Vec<Value> {
    let mut v = crate::oak::clean(v);
    for o in v.iter_mut().flat_map(|m| m["content"].as_array_mut().into_iter().flatten()).filter_map(Value::as_object_mut) {
        if o.get("type").is_some_and(|t| t == "tool_call") && o.get("name").is_some_and(|n| n == "BASH") {
            o.insert("name".into(), json!("EXECUTE"));
        }
    }
    v
}

// ---------------------------------------------------------------- the cut

fn has_tool_result(m: &Value) -> bool {
    m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"))
}

/// What an image costs, in the characters `chars` counts: images of the
/// size OAK sends come to about 1600 tokens.
const IMAGE_CHARS: usize = 5600;

fn block_chars(b: &Value) -> usize {
    20 + match b["type"].as_str().unwrap_or("") {
        "text" => b["text"].as_str().map_or(0, str::len),
        "reasoning" => b["text"].as_str().map_or(0, str::len),
        "tool_call" => b["input"].to_string().len(),
        "image" => IMAGE_CHARS,
        // A text document is its text; a PDF, very roughly, a third of its
        // base64 (pages are read as text and as pictures).
        "document" => match b["media_type"].as_str() {
            Some("text/plain") => b["data"].as_str().map_or(0, str::len),
            _ => b["data"].as_str().map_or(0, |d| d.len() / 3),
        },
        "tool_result" => match &b["content"] {
            Value::String(s) => s.len(),
            Value::Array(blocks) => blocks.iter().map(block_chars).sum(),
            other => other.to_string().len(),
        },
        _ => 0,
    }
}

/// Characters of a message that become tokens (signatures don't, and an
/// image's base64 data isn't counted as text).
pub fn chars(m: &Value) -> usize {
    m["content"].as_array().into_iter().flatten().map(block_chars).sum()
}

pub fn estimate(chars: usize) -> u64 {
    (chars as f64 / 3.5) as u64
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Where the verbatim part will start.
    pub cut: usize,
    /// Tokens of the request before the cut (system, tools and old summary included).
    pub before: u64,
    /// Old tool output in messages `cut..clear` is cleared.
    pub clear: usize,
    /// Tokens that clearing takes out of the part kept, beyond what was cleared already.
    pub cleared: u64,
}

/// Picks where to cut `messages` (the transcript a request of `total` prompt
/// tokens sent, on top of the summary ending at `base`) so that about `keep`
/// tokens stay verbatim. Sizes come from the API's own counts where a
/// request ended at the boundary, and from character counts otherwise.
///
/// A boundary is a user message that isn't tool results (a new turn) or an
/// assistant message (whose tool calls and their results then stay together
/// in the kept part); the latter is the only kind inside a long tool loop.
///
/// `keep` counts the conversation as it was, before any clearing: messages
/// `base..cleared` had their old tool output cleared by the summary at `base`.
/// With `clear`, the new one clears tool output older than the newest
/// `RECENT` tokens.
pub fn plan(messages: &[Value], base: usize, cleared: usize, total: u64, measured: &[Measure], keep: u64, clear: bool) -> Option<Plan> {
    let n = messages.len();
    if n < base + 3 {
        return None;
    }
    let at = |k: usize| measured.iter().find(|m| m.base == base && m.n == k).map(|m| m.prompt);
    // Each message as `total` counted it (with what the last summary cleared
    // taken out), and what clearing would take out of it now.
    let names = tool_names(&messages[base..]);
    let removed: Vec<usize> = (0..n).map(|k| clearable_chars(&messages[k], &names)).collect();
    let sent: Vec<usize> = (0..n).map(|k| chars(&messages[k]) - if k < cleared { removed[k] } else { 0 }).collect();
    let all: usize = sent[base..].iter().sum::<usize>().max(1);
    // The same tails as the conversation was, before clearing, and what
    // clearing would take out of each now.
    let (mut tail_chars, mut back, mut more) = (0, 0, 0);
    let (mut tails, mut full, mut gone) = (vec![0u64; n], vec![0u64; n], vec![0u64; n]);
    for k in (base + 1..n).rev() {
        tail_chars += sent[k];
        if k < cleared { back += removed[k] } else { more += removed[k] }
        let measured_before = match messages[k]["role"].as_str() {
            Some("assistant") => at(k),
            _ => at(k + 1).map(|p| p.saturating_sub(estimate(sent[k]))),
        };
        tails[k] = match measured_before {
            Some(before) if before <= total => total - before,
            _ => (total as f64 * tail_chars as f64 / all as f64) as u64,
        };
        full[k] = tails[k] + estimate(back);
        gone[k] = estimate(more);
    }
    let candidates: Vec<usize> = (base + 1..n)
        .rev()
        .filter(|&k| full[k] >= keep && (messages[k]["role"] == "assistant" || !has_tool_result(&messages[k])))
        .collect();
    // The start of a turn reads best, if one keeps no more than half again as much.
    let cut = candidates.iter().copied().find(|&k| messages[k]["role"] == "user" && full[k] <= keep * 3 / 2).or(candidates.first().copied())?;
    let before = total - tails[cut];
    // Tool output in the newest RECENT tokens stays.
    let clear = if clear { (cut..n).rev().find(|&k| full[k] >= RECENT).unwrap_or(cut) } else { cut };
    let cleared = if clear > cut { gone[cut].saturating_sub(gone[clear]) } else { 0 };
    // Not worth a summary for less than this.
    (before >= 10_000).then_some(Plan { cut, before, clear, cleared })
}

// ---------------------------------------------------------------- the summary request

const RULES: &str = "\
RULES:
- Structure it as a TIMELINE: numbered steps showing what happened, in order
- Keep ALL file paths and function, type and variable names exactly as written
- Keep ALL technical decisions and why they were made
- Keep ALL code changes: which file, what changed, what the new code does
- Keep ALL errors hit and how they were resolved
- Keep ALL user requests and instructions: what they asked for, their constraints, what to do or NOT do
- Keep the user's preferences and recurring patterns (\"always use X\", \"never do Y\")
- Include short key code snippets where they are central
- No commentary; be as dense as possible, every sentence should carry information

FORMAT:
## Active Goal
- What the user is currently asking for, and the constraints they gave (a goal set with the goal tool: just name it, it is kept as written)
## Previous Goals
- Earlier goals, finished or set aside (brief)
## Timeline
1. ...
## Current State
- What is done, what is in progress, what is next
## Key Details
- Paths, settings and decisions that must not be forgotten";

/// The summary instruction. Phrased as the user's own housekeeping request
/// (as the plugin found, a "you are a context compressor" framing gets
/// refused by some models). `verbatim_from` describes where the kept part
/// starts; None means everything above is summarized.
#[cfg(test)]
pub fn prompt(verbatim_from: Option<&str>) -> String {
    prompt_for("", verbatim_from)
}

/// `prompt` for a conversation on `model`: a model on the user's own
/// server has no notes or heartbeats, and a short context, so its summary
/// is kept short.
/// Whether `text` is this file's summary instruction (the block appended to
/// a request to ask for a rolling summary). A summary is a step of the turn
/// it belongs to, not a user prompt.
pub fn is_housekeeping(text: &str) -> bool {
    text.starts_with("This is a housekeeping request from me, not part of the work:")
}

pub fn prompt_for(model: &str, verbatim_from: Option<&str>) -> String {
    let local = crate::local::serves(model);
    let scope = match verbatim_from {
        Some(at) => format!(
            "Everything from {at} onwards will stay in the conversation verbatim, right after your summary. \
             So summarize in detail what comes BEFORE that point; the part after it needs no retelling, \
             though the Active Goal and Current State should still be right for where we are now."
        ),
        None => "Summarize the whole conversation above.".to_string(),
    };
    format!(
        "This is a housekeeping request from me, not part of the work: I'm trimming the older part of our conversation \
         to keep the context small, and the summary you write now will replace it.\n\n{scope}\n\n\
         {memory}\
         Write it so you can carry on exactly where we left off with nothing but the summary and the verbatim part. \
         Don't use any tools and don't continue the task, just write the summary. Don't mention this request in it, \
         and treat the work in progress as still in progress.\n\n{RULES}\n\n\
         If the conversation starts with an earlier summary, merge it in: keep its details and extend its timeline.\n\n\
         {length}Write only the summary.",
        memory = if local {
            ""
        } else {
            "The goal, your notes and your heartbeats in the Session memory part of your instructions stay in your context \
             after this, as they are: don't restate them; where one matters, refer to it by its number (\"note 12\"). \
             Put a fact in the summary only if it must outlive its note.\n\n"
        },
        length = if local { "The context is small, so keep the whole summary under 600 words: drop what is done and no longer matters.\n\n" } else { "" },
    )
}

/// How the prompt points at the first kept message.
pub fn describe(m: &Value) -> String {
    let blocks: Vec<&Value> = m["content"].as_array().into_iter().flatten().collect();
    let quote = |s: &str| {
        let s = one_line(s);
        let short: String = s.head(240);
        if short.len() < s.len() { format!("\"{short}…\"") } else { format!("\"{short}\"") }
    };
    if m["role"] == "user" {
        let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(" ");
        let images = blocks.iter().filter(|b| b["type"] == "image").count();
        let s = crate::plural(images);
        return match (images, text.trim().is_empty()) {
            (0, _) => format!("my message {}", quote(&text)),
            (n, true) => format!("my message with {n} image{s}"),
            (n, false) => format!("my message with {n} image{s} and the text {}", quote(&text)),
        };
    }
    // A tool call's id is unique, where its command or a short reply may
    // repeat many times in a long session, so the id is what pins the place.
    let first_call = blocks.iter().find(|b| b["type"] == "tool_call");
    let id = |call: &Value| call["id"].as_str().filter(|i| !i.is_empty()).map(|i| format!(" (id {i})")).unwrap_or_default();
    if let Some(t) = blocks.iter().find_map(|b| b["text"].as_str().filter(|t| !t.trim().is_empty())) {
        return match first_call {
            Some(call) => format!("your reply {} with the {} call{}", quote(t), call["name"].as_str().unwrap_or("tool"), id(call)),
            None => format!("your reply {}", quote(t)),
        };
    }
    if let Some(call) = first_call {
        let name = call["name"].as_str().unwrap_or("tool");
        let arg = ["command", "path", "objective"].iter().find_map(|k| call["input"][k].as_str()).unwrap_or("");
        return format!("your {name} call{} {}", id(call), quote(arg));
    }
    "the most recent part of the conversation".into()
}

/// The summary request: `req` (a chat request) with the instruction after
/// its last message. The last message of a chat request is a user turn, so
/// the instruction joins it as a text block and the prefix stays identical;
/// the cache breakpoint stays where the request this one copies had it, so
/// the summary reads exactly the entry that request wrote (looked up from
/// the instruction instead, a long wave of tool results can put that entry
/// out of the lookup's reach). See `wire_anthropic::body`.
pub fn summary_request(mut req: Request, prompt: &str) -> Request {
    req.instruction = Some(prompt.to_string());
    req.purpose = crate::oak::Purpose::Summary;
    req
}

/// What a summary request cost, as the API reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub fresh: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// Dollars, through OpenRouter (which says what each request cost).
    #[serde(default)]
    pub usd: f64,
}

impl Cost {
    fn add(&mut self, reply: &crate::oak::Collected) {
        let u = reply.usage;
        self.usd += reply.usd;
        self.fresh += u.input;
        self.cache_read += u.cache_read;
        self.cache_write += u.write;
        self.output += u.output;
    }
}

/// Errors worth sending the summary request again for: a busy model or a
/// dropped stream. (HTTP 429 and 5xx answers are already retried by `api`.)
fn transient(e: &str) -> bool {
    e.contains("verloaded") || e.starts_with("Stream") || e.starts_with("Network error")
}

/// One streamed request, as its events.
pub fn events(req: &Request) -> Result<Vec<Event>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    api::stream_messages(req.clone(), tx);
    let mut events = vec![];
    for msg in rx.try_iter() {
        match msg {
            Sse::Event(e) => events.push(e),
            Sse::Done => break,
            Sse::Err(e) => return Err(e),
            Sse::Limit(_) => return Err("The usage limit was reached".into()),
            Sse::Rotated(_) => {}
        }
    }
    Ok(events)
}

/// Added to the instruction when the model called a tool instead of
/// writing the summary.
const NO_TOOLS: &str = "\n\nTools can't be used in this request: reply with the summary as text, nothing else.";

/// Sends a summary request and returns the summary and its cost. A busy
/// model or a dropped stream is tried twice more. A refusal gets the quick
/// retries a failed request gets (`api::retry_wait`); after those, the agent
/// tries again a minute and a half later, with a fresh request. If the model
/// calls a tool instead of writing, it is asked again with a reminder added
/// to the instruction (still read from the cache), and if it calls one
/// again, with tools switched off (that one can't reuse the cache, as
/// tool_choice is part of the cache key).
pub fn summarize(req: Request) -> Result<(String, Cost), String> {
    // A local model's, to the cloud model picked for it, or itself when
    // that fails.
    if let Some(cloud) = cloud_summary(&req)
        && let Ok(r) = summarize_with(cloud)
    {
        return Ok(r);
    }
    summarize_with(req)
}

fn summarize_with(mut req: Request) -> Result<(String, Cost), String> {
    req.purpose = crate::oak::Purpose::Summary; // for the usage log
    let mut cost = Cost::default();
    let (mut failures, mut refusals) = (0, 0);
    loop {
        let events = match events(&req) {
            Ok(ev) => ev,
            Err(e) if transient(&e) && failures < 2 => {
                failures += 1;
                std::thread::sleep(std::time::Duration::from_secs(10 * failures));
                continue;
            }
            Err(e) => return Err(e),
        };
        let reply = crate::oak::collect(&events);
        cost.add(&reply);
        let stop = reply.stop.as_str();
        // A tool call instead of the summary: asked again with a reminder
        // after the instruction, where the cache still reads all before it;
        // only then with tools off, which `tool_choice` makes a cache miss.
        if stop == crate::oak::stop::TOOL_CALLS && !req.tools_off {
            match req.instruction.as_mut() {
                Some(i) if !i.ends_with(NO_TOOLS) => i.push_str(NO_TOOLS),
                _ => req.tools_off = true,
            }
            continue;
        }
        if stop == crate::oak::stop::REFUSAL && refusals < api::RETRIES {
            refusals += 1;
            std::thread::sleep(std::time::Duration::from_secs(api::retry_wait(refusals)));
            continue;
        }
        if reply.text.is_empty() || stop == crate::oak::stop::REFUSAL {
            let why = reply.detail.as_deref().map(|d| format!("; {d}")).unwrap_or_default();
            return Err(format!("The summary came back empty (stop reason: {}{why}).", if stop.is_empty() { "none" } else { stop }));
        }
        return Ok((reply.text, cost));
    }
}

/// "84.2k" style token counts.
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..100_000 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{}k", n / 1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cleared_set` as it was before it took one pass: names, then the
    /// cleared part, then the superseded READs.
    fn cleared_set_reference(messages: &[Value], summary: Option<&Summary>, cold: Option<&Clearing>) -> std::collections::HashSet<String> {
        let superseded = |from: usize, to: usize| {
            let mut seen = std::collections::HashSet::new();
            let mut out = std::collections::HashSet::new();
            for m in messages[from.min(to)..to].iter().rev() {
                for b in m["content"].as_array().into_iter().flatten().rev() {
                    if b["type"] == "tool_call" && b["name"] == "READ"
                        && let Some(id) = b["id"].as_str()
                        && !seen.insert(read_key(&b["input"]))
                    {
                        out.insert(id.to_string());
                    }
                }
            }
            out
        };
        let n = messages.len();
        let s = in_use(messages, summary);
        let cut = s.map_or(0, |s| s.cut);
        let cold = cold.filter(|c| c.base == cut);
        let to = s.map_or(cut, |s| s.clear).max(cold.map_or(0, |c| c.to)).min(n).max(cut);
        let names = tool_names(&messages[cut..n.max(cut)]);
        let call = |b: &Value| b["call_id"].as_str().map(str::to_string);
        let mut set: std::collections::HashSet<String> =
            messages[cut..to].iter().filter_map(|m| m["content"].as_array()).flatten().filter(|b| clearable(b, &names)).filter_map(call).collect();
        if let Some(c) = cold {
            let sup = c.sup.min(n);
            let old = superseded(cut, sup);
            if !old.is_empty() {
                let blocks = messages[cut..sup].iter().filter_map(|m| m["content"].as_array()).flatten();
                set.extend(blocks.filter(|b| clearable(b, &names)).filter_map(call).filter(|id| old.contains(id)));
            }
        }
        set
    }

    /// The one pass clears what the three passes did (and the view sends
    /// the same), over transcripts with repeated READs, kept tools, reused
    /// call ids, small and big outputs, Oak's own keys, and every summary
    /// and cold clearing.
    #[test]
    fn cleared_set_matches_reference() {
        let mut seed = 7u64;
        let mut rand = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for round in 0..2000 {
            let mut messages = vec![];
            for i in 0..2 + rand(24) as usize {
                let id = format!("c{}", if rand(6) == 0 { rand(4) as usize } else { i });
                let (name, input) = match rand(5) {
                    0 | 1 => ("READ", json!({"path": format!("src/{}.rs", rand(3)), "offset": rand(2)})),
                    2 => ("TASK", json!({"action": "wait"})),
                    3 => ("ASK", json!({})),
                    _ => ("EXECUTE", json!({"command": "ls"})),
                };
                messages.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": name, "input": input}]}));
                let out = "x".repeat(if rand(3) == 0 { 10 } else { 2000 });
                let mut result = if rand(8) == 0 { json!({"type": "tool_result", "content": out}) } else { json!({"type": "tool_result", "call_id": id, "content": out}) };
                if rand(3) == 0 {
                    result["_oak_display"] = json!([]);
                }
                messages.push(json!({"role": "user", "_oak_at": i, "content": [result, {"type": "text", "text": "go on", "_oak_at": i}]}));
            }
            let n = messages.len() as u64;
            let summary = (round % 3 != 0).then(|| Summary { cut: rand(n) as usize, text: "s".into(), made_at: 0, clear: rand(n + 4) as usize });
            let base = summary.as_ref().filter(|s| s.cut > 0).map_or(0, |s| s.cut);
            let cold = (round % 2 == 0).then(|| Clearing { base: if rand(5) == 0 { base + 1 } else { base }, to: rand(n + 4) as usize, sup: rand(n + 4) as usize });
            let want = cleared_set_reference(&messages, summary.as_ref(), cold.as_ref());
            let got: std::collections::HashSet<String> = cleared_set(&messages, summary.as_ref(), cold.as_ref()).into_iter().map(str::to_string).collect();
            assert_eq!(got, want, "round {round}");
            // And the view sends what it did when it copied every message
            // and cleared the output after.
            let mut copied = match in_use(&messages, summary.as_ref()) {
                None => messages.to_vec(),
                Some(s) => {
                    let mut v = vec![summary_message(&s.text)];
                    if messages[s.cut]["role"] == "user" {
                        v.push(json!({"role": "assistant", "content": [{"type": "text", "text": ACK}]}));
                    }
                    v.extend_from_slice(&messages[s.cut..]);
                    v
                }
            };
            for b in copied.iter_mut().flat_map(|m| m["content"].as_array_mut().into_iter().flatten()) {
                if b["type"] == "tool_result" && b["call_id"].as_str().is_some_and(|id| want.contains(id)) {
                    b["content"] = json!(CLEARED);
                }
            }
            assert_eq!(view_with(&messages, summary.as_ref(), cold.as_ref()), strip_oak(copied), "round {round}");
        }
    }

    fn user(t: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": t}]})
    }
    fn reply(t: &str) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": t}]})
    }

    /// A local model summarizes near its short window, keeping at most a
    /// quarter of it, and asks for a short summary without notes.
    #[test]
    fn local_models_summarize_early_and_short() {
        assert_eq!(config_for("local/qwen"), Config { trigger: 25_000, keep: 6_250 });
        let p = prompt_for("local/qwen", None);
        assert!(p.contains("under 600 words") && !p.contains("note 12"), "{p}");
        assert!(!prompt(None).contains("600 words") && prompt(None).contains("note 12"));
        assert_eq!(Summarizer::from_id("haiku"), Summarizer::Haiku);
        assert_eq!(Summarizer::from_id("?"), Summarizer::Local);
    }

    /// Roles alternate and every tool_result follows its tool_use.
    fn valid(v: &[Value]) -> bool {
        v[0]["role"] == "user"
            && v.windows(2).all(|w| w[0]["role"] != w[1]["role"])
            && v.iter().enumerate().all(|(i, m)| {
                m["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_result").all(|b| {
                    i > 0 && v[i - 1]["content"].as_array().unwrap().iter().any(|c| c["id"] == b["call_id"])
                })
            })
    }

    /// Twenty turns, each a question, a tool call, its result and an answer.
    fn transcript() -> Vec<Value> {
        let big = "x".repeat(3500); // about 1000 tokens
        (0..20)
            .flat_map(|i| {
                let id = format!("t{i}");
                vec![
                    user(&format!("question {i}")),
                    json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": "EXECUTE", "input": {"command": "ls"}}]}),
                    json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": big}]}),
                    reply("answer"),
                ]
            })
            .collect()
    }

    /// Rates of a model whose cache reads cost a tenth of input (Opus 4.8,
    /// Haiku 4.5), at the five-minute cache.
    const TENTH: Rates = Rates { read: 0.1, write: 1.25, out: 5.0, clear: false };

    /// The trigger is capped at 90% of the model's context window (the keep
    /// following it down); unknown windows aren't capped.
    #[test]
    fn sizes_capped_by_the_model() {
        assert_eq!(max_trigger(Some(1_000_000)), 900_000);
        assert_eq!(max_trigger(Some(1_048_576)), 940_000);
        assert_eq!(max_trigger(Some(200_000)), 180_000);
        assert_eq!(max_trigger(Some(10_000)), 20_000);
        assert_eq!(max_trigger(None), 2_000_000);
        let c = Config { trigger: 1_500_000, keep: 1_000_000 };
        assert_eq!(capped(c, 180_000), Config { trigger: 180_000, keep: 135_000 });
        assert_eq!(capped(DEFAULT, 900_000), DEFAULT);
    }

    /// The kept part is a valid transcript at any boundary, and a cut at a
    /// user message gets the acknowledgement in between.
    #[test]
    fn view_keeps_roles_and_tool_pairs() {
        let m = transcript();
        for cut in 1..m.len() {
            if m[cut]["role"] == "assistant" || m[cut]["content"][0]["type"] != "tool_result" {
                let s = Summary { cut, text: "earlier".into(), made_at: 0, ..Default::default() };
                assert!(valid(&view(&m, Some(&s))), "cut {cut}");
            }
        }
        assert_eq!(view(&m, None), m);
    }

    /// The refusal's category and explanation are kept, for the error.
    #[test]
    fn refusal_category_is_kept() {
        let ev = json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber", "explanation": "Declined."}}});
        let c = crate::oak::collect(&crate::wire_anthropic::events(&ev, "claude"));
        assert_eq!((c.stop.as_str(), c.detail.as_deref()), ("refusal", Some("category cyber: Declined.")));
        let ev = json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_details": null}});
        assert_eq!(crate::oak::collect(&crate::wire_anthropic::events(&ev, "claude")).detail, None);
    }

    /// OAK's own block keys never reach the API, with or without a summary in
    /// force; the saved messages keep them. A pre-rename BASH call goes as
    /// EXECUTE.
    #[test]
    fn view_drops_oak_keys() {
        let think = json!({"role": "assistant", "content": [{"type": "reasoning", "text": "t", "sig": "s", "by": "claude", "_oak_ms": 1200}, {"type": "text", "text": "a"}]});
        let m = vec![user("q"), think.clone(), user("r"), think];
        for s in [None, Some(Summary { cut: 2, text: "s".into(), made_at: 0, ..Default::default() })] {
            let sent = serde_json::to_string(&view(&m, s.as_ref())).unwrap();
            assert!(!sent.contains("_oak_"), "{sent}");
            assert!(sent.contains("\"sig\":\"s\""), "{sent}");
        }
        assert_eq!(m[1]["content"][0]["_oak_ms"], 1200);
        let old = json!({"role": "assistant", "content": [{"type": "tool_call", "id": "t", "name": "BASH", "input": {"command": "ls"}}]});
        assert_eq!(view(&[old], None)[0]["content"][0]["name"], "EXECUTE");
    }

    /// The cut lands where about `keep` tokens stay, preferring the start of
    /// a turn, and a plan with nothing worth summarizing is none.
    #[test]
    fn plan_keeps_about_the_target() {
        let m = transcript();
        let total = 20_000 + 3_000; // ~1000 tokens a turn, plus system and tools
        let p = plan(&m, 0, 0, total, &[], 6_000, false).unwrap();
        let s = Summary { cut: p.cut, text: "s".into(), made_at: 0, ..Default::default() };
        assert!(valid(&view(&m, Some(&s))));
        let kept: usize = m[p.cut..].iter().map(chars).sum();
        let kept = total as f64 * kept as f64 / m.iter().map(chars).sum::<usize>() as f64;
        assert!((6_000.0..9_000.0).contains(&kept), "kept {kept}");
        assert_eq!(m[p.cut]["content"][0]["text"].as_str().map(|t| t.starts_with("question")), Some(true));
        // Not worth a summary for less than 10k before the cut.
        assert_eq!(plan(&m, 0, 0, 12_000, &[], 6_000, false), None);
    }

    /// The API's own counts are used where a request ended at a boundary, so
    /// a long tool loop is cut inside it, at a call.
    #[test]
    fn plan_uses_measured_sizes_and_cuts_inside_tool_loops() {
        let mut m = vec![user("do it all")];
        for i in 0..30 {
            m.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": format!("t{i}"), "name": "EXECUTE", "input": {}}]}));
            m.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": format!("t{i}"), "content": "ok"}]}));
        }
        // The API says each round added 2000 tokens.
        let measured: Vec<Measure> = (0..30).map(|i| Measure { base: 0, n: 1 + 2 * i, prompt: 5_000 + 2_000 * i as u64 }).collect();
        let total = 5_000 + 2_000 * 30;
        let p = plan(&m, 0, 0, total, &measured, 20_000, false).unwrap();
        assert_eq!(m[p.cut]["role"], "assistant");
        assert_eq!(total - p.before, 20_000, "the measured sizes are used exactly");
        assert!(valid(&view(&m, Some(&Summary { cut: p.cut, text: "s".into(), made_at: 0, ..Default::default() }))));
    }

    /// A summary that clears: tool output stays in the newest RECENT tokens
    /// and in short results and kept tools; the rest goes, and the next plan
    /// counts what was cleared as still there.
    #[test]
    fn summaries_clear_old_tool_output() {
        // 40 rounds of a 2000-token result (7000 characters) in one tool loop,
        // one of them an ASK and one short.
        let big = "y".repeat(7_000);
        let mut m = vec![user("go")];
        for i in 0..40 {
            let name = if i == 5 { "ASK" } else { "EXECUTE" };
            m.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": format!("t{i}"), "name": name, "input": {}}]}));
            let out = if i == 6 { "exit code 1" } else { big.as_str() };
            m.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": format!("t{i}"), "content": out}]}));
        }
        let total = 5_000 + 40 * 2_000;
        let p = plan(&m, 0, 0, total, &[], 60_000, true).unwrap();
        let tail = |from: usize| m[from..].iter().map(chars).sum::<usize>() as f64 / m.iter().map(chars).sum::<usize>() as f64 * total as f64;
        assert!(tail(p.cut) >= 60_000.0 && tail(p.clear) >= RECENT as f64 && tail(p.clear + 1) < RECENT as f64 + 2_100.0, "{p:?}");
        assert!(p.cleared > 25_000, "{p:?}");
        // The cut is at a call, so no acknowledgement: v[i] is m[i].
        let v = view(&m, Some(&Summary { cut: 1, text: "s".into(), made_at: 0, clear: p.clear }));
        assert!(valid(&v));
        let out = |i: usize| v[i]["content"][0]["content"].as_str().unwrap().to_string();
        let first_kept = (p.clear..).find(|&i| i % 2 == 0).unwrap();
        assert_eq!(out(2), CLEARED, "old output goes");
        assert_eq!(out(12), big, "ASK's answer stays");
        assert_eq!(out(14), "exit code 1", "short results stay");
        assert_eq!(out(first_kept), big, "the newest stay");
        assert_eq!(out(first_kept - 2), CLEARED);
        // The next plan, after 40 more rounds, on the view that summary left,
        // still counts the conversation as it was: about 60k of it stays.
        let s = Summary { cut: 1, text: "s".into(), made_at: 0, clear: 61 };
        for i in 40..80 {
            m.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": format!("t{i}"), "name": "EXECUTE", "input": {}}]}));
            m.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": format!("t{i}"), "content": big}]}));
        }
        let shown = 5_000 + estimate(view(&m, Some(&s))[1..].iter().map(chars).sum());
        let again = plan(&m, 1, 61, shown, &[], 60_000, true).unwrap();
        let kept = estimate(m[again.cut..].iter().map(chars).sum());
        assert!((54_000..70_000).contains(&kept), "{kept} {again:?}");
        assert!(again.clear > 61 && again.cleared > 25_000, "{again:?}");
        // Off: nothing is cleared.
        let off = plan(&m, 0, 0, total, &[], 60_000, false).unwrap();
        assert_eq!((off.clear, off.cleared), (off.cut, 0));
    }

    /// Clearing shrinks what a summary leaves, so it pays to summarize sooner.
    #[test]
    fn clearing_moves_the_best_trigger() {
        let opus = Rates { read: 0.05, write: 1.25, out: 5.0, clear: false };
        let clear = Rates { clear: true, ..opus };
        assert_eq!(in_context(10_000.0, true), 10_000.0);
        assert_eq!(in_context(120_000.0, true), 55_000.0);
        assert_eq!(best_trigger(115_000, opus), 230_000);
        assert_eq!(best_trigger(115_000, clear), 140_000);
    }

    /// The summary request is the chat request with the instruction added to
    /// its last turn, keeping the cache breakpoint where it was.
    #[test]
    fn summary_request_appends_to_the_last_turn() {
        let req = Request {
            model: "m".into(),
            messages: vec![user("a"), json!({"role": "user", "content": [{"type": "tool_result", "call_id": "t1", "content": "ok"}]})],
            ..Default::default()
        };
        let r = summary_request(req.clone(), "SUMMARIZE");
        let body = crate::api::claude_body(&r);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "the prefix stays as sent");
        assert_eq!(msgs[1]["content"][0]["content"], "ok");
        assert_eq!(msgs[1]["content"][1]["text"], "SUMMARIZE");
        // The breakpoint stays on the copied request's last block, none on
        // the instruction.
        assert!(msgs[1]["content"][0].get("cache_control").is_some());
        assert_eq!(msgs[1]["content"][1].get("cache_control"), None);
        // A view truncated after an answer gets its own user turn.
        let r = summary_request(Request { messages: vec![user("a"), reply("b")], ..Default::default() }, "S");
        assert_eq!(crate::api::claude_body(&r)["messages"][2]["role"], "user");
    }

    /// The text of a streamed reply, without its thinking.
    #[test]
    fn reply_text_joins_the_text() {
        let events = vec![
            Event::Block { index: 0, block: json!({"type": "reasoning", "text": ""}) },
            Event::Block { index: 1, block: crate::oak::text("") },
            Event::Text { index: 1, text: "## Active ".into() },
            Event::Text { index: 1, text: "Goal".into() },
            Event::Stop { reason: "end".into(), detail: None },
        ];
        let c = crate::oak::collect(&events);
        assert_eq!((c.text.as_str(), c.stop.as_str()), ("## Active Goal", "end"));
    }

    /// The sizes pair up: the best trigger for a keep, the best keep for a
    /// trigger, and the suggestion only when the pair is off by over 5%.
    #[test]
    fn sizes_suit_each_other() {
        let best_keep = |t| best_keep(t, TENTH);
        assert_eq!(best_trigger(40_000, TENTH), 100_000);
        assert_eq!(best_trigger(100_000, TENTH), 180_000);
        // Opus 5.5's cache reads cost half as much: rereading is cheaper, so
        // later triggers.
        let opus = Rates { read: 0.05, write: 1.25, out: 5.0, clear: false };
        assert_eq!(round5k(best_trigger_exact(40_000.0, opus)), 120_000);
        assert_eq!(round5k(best_trigger_exact(115_000.0, opus)), 230_000);
        // The two directions agree.
        for keep in [5_000, 40_000, 100_000, 150_000] {
            let t = best_trigger(keep, TENTH);
            assert!(best_keep(t).abs_diff(keep) <= 5_000, "keep {keep} -> trigger {t} -> keep {}", best_keep(t));
            assert_eq!(suggestion(Config { trigger: t, keep }, Side::Keep, TENTH), None);
        }
        // The default is close enough to leave alone.
        assert_eq!(suggestion(DEFAULT, Side::Keep, TENTH), None);
        // Too high a trigger for the keep: the other side is suggested.
        let off = Config { trigger: 250_000, keep: 100_000 };
        assert_eq!(suggestion(off, Side::Keep, TENTH), Some(Config { trigger: 180_000, keep: 100_000 }));
    }

    #[test]
    fn config_is_clamped() {
        set_config(Config { trigger: 5_000, keep: 1_000 });
        assert_eq!(config(), Config { trigger: 20_000, keep: 5_000 });
        set_config(Config { trigger: 100_000, keep: 90_000 });
        assert_eq!(config(), Config { trigger: 100_000, keep: 75_000 }, "keep stays below the trigger");
        set_config(DEFAULT);
    }

    /// An image counts as about 1600 tokens, never as its base64 data.
    #[test]
    fn images_count_as_tokens_not_base64() {
        let img = json!({"type": "image", "media_type": "image/png", "data": "A".repeat(2_000_000)});
        let m = json!({"role": "user", "content": [img.clone(), {"type": "text", "text": "look"}]});
        assert!(chars(&m) < 6_000, "{}", chars(&m));
        assert_eq!(describe(&m), "my message with 1 image and the text \"look\"");
    }

    /// `describe` names the first kept message, with the call id that pins it.
    #[test]
    fn describe_points_at_the_cut() {
        assert_eq!(describe(&user("fix  the\nbug")), "my message \"fix the bug\"");
        assert_eq!(describe(&reply("Done.")), "your reply \"Done.\"");
        let both = json!({"role": "assistant", "content": [{"type": "text", "text": "Running it."}, {"type": "tool_call", "id": "toolu_2", "name": "READ", "input": {"path": "a"}}]});
        assert_eq!(describe(&both), "your reply \"Running it.\" with the READ call (id toolu_2)");
    }

    /// The calls `cleared_calls` names are the ones `view` clears.
    #[test]
    fn cleared_calls_match_the_view() {
        let long = "x".repeat(SMALL + 1);
        let use_ = |id: &str, name: &str| json!({"role": "assistant", "content": [{"type": "tool_call", "id": id, "name": name, "input": {}}]});
        let result = |id: &str, out: &str| json!({"role": "user", "content": [{"type": "tool_result", "call_id": id, "content": out}]});
        let messages = vec![
            json!({"role": "user", "content": "go"}),
            use_("a", "READ"), result("a", &long), // before the cut: summarized
            use_("b", "READ"), result("b", &long), // cleared
            use_("c", "READ"), result("c", "short"), // too short to clear
            use_("d", "ASK"), result("d", &long), // kept: can't be run again
            use_("e", "READ"), result("e", &long), // past `clear`: recent
        ];
        let s = Summary { cut: 3, text: "s".into(), made_at: 0, clear: 9 };
        assert_eq!(cleared_calls(&messages, Some(&s)), ["b"].into_iter().collect());
        let seen = view(&messages, Some(&s));
        let shown: Vec<&Value> = seen.iter().filter_map(|m| m["content"].as_array()).flatten().filter(|b| b["content"] == CLEARED).collect();
        assert_eq!((shown.len(), shown[0]["call_id"].as_str()), (1, Some("b")));
        assert!(cleared_calls(&messages, None).is_empty());
    }

    /// At a cold moment old tool output goes as a summary would clear it,
    /// and a READ a later identical READ superseded goes even when recent.
    #[test]
    fn cold_moments_clear_old_output_and_superseded_reads() {
        let big = "y".repeat(7_000);
        let mut m = vec![user("go")];
        for i in 0..30 {
            let (name, input) = match i {
                22 | 27 => ("READ", json!({"path": "src/a.rs"})),
                28 => ("READ", json!({"path": "src/b.rs"})),
                _ => ("EXECUTE", json!({})),
            };
            m.push(json!({"role": "assistant", "content": [{"type": "tool_call", "id": format!("t{i}"), "name": name, "input": input}]}));
            m.push(json!({"role": "user", "content": [{"type": "tool_result", "call_id": format!("t{i}"), "content": big}]}));
        }
        let mut s = State::default();
        assert!(s.clear_cold(&m));
        let v = s.view(&m);
        assert!(valid(&v));
        let out = |i: usize| v[2 + 2 * i]["content"][0]["content"].as_str().unwrap().to_string();
        assert_eq!(out(0), CLEARED, "old output goes");
        assert_eq!(out(29), big, "the newest stay");
        assert_eq!(out(22), CLEARED, "superseded by the READ of round 27");
        assert_eq!((out(27), out(28)), (big.clone(), big.clone()));
        let to = s.cleared_to();
        assert!(to > 1 && to < 2 + 2 * 22, "{to}");
        assert_eq!(s.cleared_calls(&m).len(), (0..30).filter(|&i| out(i) == CLEARED).count());
        // Nothing new to clear: no change (the next request can read the cache).
        assert!(!s.clear_cold(&m));
        // Another summary's view: that clearing is not in it.
        s.summary = Some(Summary { cut: 3, text: "s".into(), made_at: 0, clear: 3 });
        assert_eq!(s.cleared_to(), 3);
    }
}
