// One worker thread per conversation runs the agent loop: send the
// transcript, stream the reply, run the tools it asks for, repeat.
//
// The UI only pushes onto `Live::queued` and sets `interrupt` or `paused`.
// Queued messages, task notifications and other sessions' messages go with
// the next request, even mid-turn (after the tool results). An interrupt
// abandons what is in flight (a running EXECUTE becomes a task) and sends
// them at once. A pause holds the next request; a streaming reply is dropped
// and its request sent again on resume. A request that hits the plan's usage
// limit goes again through the account with the most usage left, and waits
// for the first reset when all are used up. The model is told about neither.

/// Waiting past a usage limit's reset by this much, so it really is over.
const LIMIT_SLACK_MS: i64 = 15_000;

/// Whether to stop waiting for a usage limit: its reset (plus slack) is
/// `left` ms away, or since the wait began an account was signed in or
/// came free early (its limit was reset: `api::accounts_freed`), so the
/// request may go through. Still limited, it waits again.
fn limit_over(left: i64, freed_then: u64, freed_now: u64) -> bool {
    left <= 0 || freed_now != freed_then
}

use crate::tools::Head;
use crate::api;
use crate::oak;
use crate::rolling::{self, Measure, Plan, Summary};
use crate::changes;
use crate::instructions;
use crate::store::{self, Conversation, Heartbeat, Mark, MarkKind, Memory, Note};
use crate::tasks::{self, Proc, Tasks, fmt_duration};
use crate::tools::{self, MAX_OUTPUT, TASK_WAIT_MAX};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

mod alerts;
mod memory;
pub(crate) mod prompt;
mod reply;
mod rewrite;
mod subagents;
pub(crate) mod title;
mod tool_calls;
use prompt::scratchpad;
pub(crate) use prompt::SCRATCH_ROOT;
pub use memory::{notes_alert_on, notes_tokens, set_notes_alert, NOTES_ALERT_TOKENS};
use memory::memory_tool;
use title::title_for;
use tool_calls::{Batch, end_ask_request};

pub const NOTE_TAG: &str = "<task-notification>";
/// A heartbeat the model started, arriving.
pub const BEAT_TAG: &str = "<heartbeat>";
/// A usage alert from Oak (`alerts`), shown like a task notification.
pub const ALERT_TAG: &str = "<usage-alert>";
/// Starts the message that gives the model a /goal.
pub const GOAL_TAG: &str = "<goal>";
/// Starts a message from another session: a subagent, or for a subagent
/// the main session or another agent. `<agent-message from="...">`, with
/// `type="final reply"` for what a subagent said as it ended its turn.
pub const AGENT_TAG: &str = "<agent-message";
/// Starts the message telling the model whether the user accepted its
/// REQUEST_ASK.
pub const ASK_TAG: &str = "<ask-request>";

/// The message telling the model the user accepted (or declined) its
/// REQUEST_ASK about `about`.
fn ask_reply_message(about: &str, accepted: bool) -> String {
    let about = about.trim();
    if accepted {
        format!("{ASK_TAG}The user accepted your REQUEST_ASK (about: {about}). You may now call ASK once to ask them.</ask-request>")
    } else {
        format!(
            "{ASK_TAG}The user declined your REQUEST_ASK (about: {about}). Carry on without asking them: decide for yourself, or leave it for your final report.</ask-request>"
        )
    }
}

/// How subagents and their messages name the session that started them.
pub const MAIN_LABEL: &str = "main session";

/// How messages name subagent `id`: `agent 2 (Explore the parser)`.
pub fn agent_label(id: u64, name: &str) -> String {
    format!("agent {id} ({})", name.replace('"', "'"))
}

/// A message from `from` for another session's model.
pub(crate) fn agent_message(from: &str, final_reply: bool, body: &str) -> String {
    let kind = if final_reply { " type=\"final reply\"" } else { "" };
    format!("{AGENT_TAG} from=\"{from}\"{kind}>\n{}\n</agent-message>", body.trim())
}

/// An agent message's sender, whether it is a final reply, and its text.
pub fn parse_agent_message(text: &str) -> Option<(&str, bool, &str)> {
    let rest = text.strip_prefix(AGENT_TAG)?.strip_prefix(" from=\"")?;
    let (from, rest) = rest.split_once('"')?;
    let (attrs, body) = rest.split_once('>')?;
    let body = body.trim().trim_end_matches("</agent-message>").trim();
    Some((from, attrs.contains("final reply"), body))
}

/// Every session OAK runs, by conversation id, so that sessions can send
/// each other messages.
static SESSIONS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Session>>> = std::sync::LazyLock::new(Default::default);
/// Oak is closing at once (`gpu::recover`): every session at work is
/// ultrapaused, its background tasks left running. Their ids.
pub(crate) fn ultrapause_all_keeping_tasks() -> Vec<String> {
    let sessions: Vec<(String, Session)> = SESSIONS.lock().unwrap().iter().map(|(id, s)| (id.clone(), s.clone())).collect();
    let mut ids = vec![];
    for (id, s) in sessions {
        if s.active() {
            if !s.pause_settled() {
                s.ultrapause_keeping_tasks();
            }
            ids.push(id);
        }
    }
    ids
}

/// Whether the sessions `ids` have all settled into their pause.
pub(crate) fn all_settled(ids: &[String]) -> bool {
    let sessions = SESSIONS.lock().unwrap();
    ids.iter().all(|id| sessions.get(id).is_none_or(|s| s.pause_settled() || !s.pause_on()))
}

/// Subagents started since the app last took them (`take_spawned`).
static SPAWNED: std::sync::LazyLock<Mutex<Vec<(String, Session)>>> = std::sync::LazyLock::new(Default::default);
/// Conversations AGENT_DELETE removed since the app last took them
/// (`take_deleted`).
static DELETED: std::sync::LazyLock<Mutex<Vec<String>>> = std::sync::LazyLock::new(Default::default);

/// Every conversation's scratchpad, for `crate::scratch`.
pub fn scratch_owners() -> Vec<crate::scratch::Owner> {
    let sessions: Vec<Session> = SESSIONS.lock().unwrap().values().cloned().collect();
    sessions
        .iter()
        .filter_map(|s| {
            let busy = s.live.lock().unwrap().busy || s.tasks.running() > 0;
            let c = s.conv.lock().unwrap();
            (!c.deleted).then(|| crate::scratch::Owner { id: c.id.clone(), folder: c.folder.clone(), scratch: c.scratch.clone(), last_ms: c.last_message_at, in_use: busy })
        })
        .collect()
}

/// The session of conversation `id`, if it runs.
fn find_session(id: &str) -> Option<Session> {
    SESSIONS.lock().unwrap().get(id).cloned()
}

/// The subagents AGENT_NEW started since the last call, with their
/// conversation ids, for the app to list.
pub fn take_spawned() -> Vec<(String, Session)> {
    std::mem::take(&mut *SPAWNED.lock().unwrap())
}

/// The conversations AGENT_DELETE removed since the last call, for the app
/// to drop (their tabs close).
pub fn take_deleted() -> Vec<String> {
    std::mem::take(&mut *DELETED.lock().unwrap())
}

/// The message that gives the model a /goal.
fn goal_prompt(goal: &str) -> String {
    format!(
        "{GOAL_TAG}\n{goal}\n</goal>\n\n\
         Continue working until you have completed the user's goal. When you have completed the user's goal, \
         please call GOAL action=justify with a complete justification and explanation of how you have completed \
         the user's goal. When you feel that your justification reflects accurately how you have \
         fulfilled the user's request in its entirety, call GOAL action=finish, which will hand control back to the user \
         after you end your turn."
    )
}

/// Sent when the model ends its turn with the goal not finished.
fn goal_continue(goal: &str) -> String {
    format!(
        "Automatic message from Oak, not from the user: you ended your turn, but the goal isn't finished: \"{goal}\" \
         Continue with what remains. Once all of it is done, call GOAL action=justify with a complete explanation \
         of how you fulfilled it, then GOAL action=finish; control goes back to the user when you end that turn."
    )
}

/// A message about the goal from OAK. Its "_oak_goal" key (dropped before
/// sending) keeps it out of the transcript, where its mark stands instead.
fn goal_block(text: &str) -> Value {
    json!({"type": "text", "text": text, "_oak_goal": true})
}

/// Context from OAK for the model (a subdirectory's instructions, files
/// changed on disk), kept out of the transcript the same way.
fn context_block(text: &str) -> Value {
    json!({"type": "text", "text": text, "_oak_context": true})
}

/// Whether a user-turn block is a message from OAK that the transcript
/// leaves out (goal messages, also the `<oak-goal>` notes of earlier
/// versions, and context). The transcript reads these fields along with
/// the rest of the block (`transcript::UserBlock::hidden`).
#[cfg(test)]
pub fn is_hidden_block(b: &Value) -> bool {
    b.get("_oak_goal").is_some() || b.get("_oak_context").is_some() || b["text"].as_str().is_some_and(|t| t.starts_with(OLD_GOAL_TAG))
}

/// What goal notes began with before they were marked `_oak_goal`.
pub const OLD_GOAL_TAG: &str = "<oak-goal>";

/// A change the user made in the session box. It reaches the model with the
/// next request, without interrupting what it is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// The session goal; empty clears it.
    Goal(String),
    Effort(String),
    /// The provider and model the conversation goes on with (its id).
    Model(api::Provider, String),
}

/// Every effort level a model may have, lowest first. Ultra is the Codex
/// models' only (some of them).
pub const EFFORTS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];

/// The levels most models have: all of them but ultra.
pub const STANDARD_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort levels `model` tells apart, lowest first: a Claude model's as
/// the Models API lists them (`api::claude_efforts`; all five standard ones
/// until it has answered, none for one without effort), a Codex model's
/// and an OpenRouter model's from their tables (DeepSeek: low, high, max).
pub fn efforts(model: &str) -> Vec<&'static str> {
    if let Some(m) = crate::openrouter::model(model) {
        return m.efforts.to_vec();
    }
    if let Some(m) = crate::codex::model(model) {
        return m.efforts.to_vec();
    }
    if let Some(m) = crate::freebuff::model(model) {
        return m.efforts.to_vec();
    }
    if let Some(m) = crate::nodeclub::model(model) {
        return m.efforts.to_vec();
    }
    if crate::local::serves(model) {
        return crate::local::EFFORTS.to_vec();
    }
    match api::claude_efforts(model) {
        Some(levels) => EFFORTS.iter().copied().filter(|e| levels.iter().any(|l| l == e)).collect(),
        None => STANDARD_EFFORTS.to_vec(),
    }
}

/// `effort` as `model` takes it: itself when the model has that level,
/// else the next one up it has (DeepSeek: medium is high, xhigh is max),
/// else its highest (ultra on Claude is max). Empty for a model without
/// effort levels: requests for it then go without one.
pub fn effort_for(model: &str, effort: &str) -> String {
    let levels = efforts(model);
    if levels.contains(&effort) {
        return effort.to_string();
    }
    let rank = |e: &str| EFFORTS.iter().position(|x| *x == e);
    let want = rank(effort).unwrap_or(2);
    let up = levels.iter().find(|l| rank(l).is_some_and(|r| r >= want));
    up.or(levels.last()).map_or(String::new(), |l| l.to_string())
}

/// Where a revert goes back to: everything from there on is removed.
#[derive(Clone, Debug, PartialEq)]
pub enum RevertTo {
    /// Before block `block` of message `msg`, which must still be that
    /// block (as the user saw it). A user message keeps its earlier blocks
    /// (tool results); a reply keeps its earlier blocks, and the results of
    /// the calls among them.
    Block(usize, usize, Value),
    /// Back to the first `n` messages: before the reply streaming in.
    End(usize),
}

#[derive(Default)]
pub struct Live {
    pub busy: bool,
    /// The pass (`cumulative_pass_nr`) the transcript was last drawn in: a
    /// delta streaming in while it isn't shown asks for no frame.
    pub drawn: u64,
    /// The viewport that drew it (a side chat has its own pass counter).
    pub drawn_viewport: Option<eframe::egui::ViewportId>,
    pub status: String,
    /// The assistant reply streaming in right now.
    pub blocks: Vec<Value>,
    /// The results of its tool calls that started while it streamed in and
    /// are done (`tool_calls::Batch`): held back from the transcript till
    /// the reply is in it (None when not holding).
    pub early: Option<Vec<Value>>,
    /// User messages waiting for the agent to finish what it is doing.
    pub queued: Vec<Queued>,
    next_queued: u64,
    /// Task notifications waiting to be sent, with the task they are about.
    pub notes: Vec<(u64, String)>,
    /// Tasks whose exit the model has already seen (via TASK_GET_NEW_OUTPUT
    /// or TASK_DELETE), so a notification about it would only be stale.
    pub reported: BTreeSet<u64>,
    pub error: Option<String>,
    /// A smart-context summary is being written in the background.
    pub compacting: bool,
    /// When and at what context size the last summary failed, and how long
    /// to wait before trying again (unless the context grows a tenth meanwhile).
    rolling_failed: Option<(Instant, u64, Duration)>,
    /// /goal and /effort commands waiting for the next request.
    pub commands: Vec<Command>,
    /// Questions from ASK, waiting for the user's answers.
    pub ask: Option<Ask>,
    /// A revert the user asked for (see `RevertTo`). The worker applies it
    /// once it has stopped.
    pub revert: Option<RevertTo>,
    /// How many reverts were applied: a summary begun before one is dropped.
    pub reverts: u64,
    /// The goal was resumed while idle: the worker sends the "carry on"
    /// the pause held back, if the model's turn ended with the goal open.
    goal_resume: bool,
    /// The main session stopped this subagent (AGENT_STOP): the turn's end
    /// isn't reported back to it.
    quiet_stop: bool,
    /// A request to the model is under way (sent, its reply not all in).
    pub requesting: bool,
    /// The user paused during a request: the pause starts when the request
    /// ends, so the reply paid for isn't thrown away (`request_pause`).
    pub pause_queued: bool,
    /// Ultrapause: everything stops at once. Running commands are killed,
    /// waits end, and the session is paused until resumed.
    pub ultra: bool,
    /// Paused, the worker is holding its next request (`hold_while_paused`,
    /// or a usage-limit wait while paused): nothing runs.
    held: bool,
    /// Tool calls running, and how many of them only wait (ASK, TASK_WAIT,
    /// AGENT_WAIT): a paused session sitting in nothing but waits counts as
    /// paused, as it does no work until resumed.
    tools: usize,
    waiting: usize,
}

/// Counts a tool call in `Live::tools` while it runs (a wait also in
/// `Live::waiting`, around its waiting).
struct Running<'a> {
    live: &'a Mutex<Live>,
    wait: bool,
}

impl<'a> Running<'a> {
    fn new(live: &'a Mutex<Live>, wait: bool) -> Running<'a> {
        let mut l = live.lock().unwrap();
        if wait {
            l.waiting += 1;
        } else {
            l.tools += 1;
        }
        Running { live, wait }
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut l = self.live.lock().unwrap();
        if self.wait {
            l.waiting = l.waiting.saturating_sub(1);
        } else {
            l.tools = l.tools.saturating_sub(1);
        }
    }
}

impl Live {
    /// Whether a pause has taken hold: paused, no pause waiting for a
    /// request, and the worker idle, holding, or only waiting on something.
    pub fn pause_settled(&self, paused: bool) -> bool {
        paused && !self.pause_queued && (!self.busy || self.held || (self.tools > 0 && self.tools <= self.waiting))
    }

    /// Whether a pause is under way but hasn't taken hold yet.
    pub fn pausing(&self, paused: bool) -> bool {
        (paused || self.pause_queued) && !self.pause_settled(paused)
    }

    /// Takes the queued messages that are ready to send, leaving held ones.
    fn take_queued(&mut self) -> Vec<Queued> {
        let (ready, held): (Vec<Queued>, Vec<Queued>) = std::mem::take(&mut self.queued).into_iter().partition(|q| !q.held);
        self.queued = held;
        ready
    }

    /// Queued messages that will be sent when the agent is free.
    pub fn ready_count(&self) -> usize {
        self.queued.iter().filter(|q| !q.held).count()
    }

    /// The id for a new queued message.
    fn next_queued_id(&mut self) -> u64 {
        self.next_queued += 1;
        self.next_queued
    }
}

/// The questions of an ASK call and the user's answers so far. The UI fills
/// in `picked` and `other` and sets `sent` (or `skipped`); the worker reads
/// them then.
pub struct Ask {
    pub questions: Vec<Question>,
    /// Per question, per option: picked or not.
    pub picked: Vec<Vec<bool>>,
    /// Per question: the user's own answer, and whether it is chosen.
    pub other: Vec<(String, bool)>,
    pub sent: bool,
    /// The user skipped the questions without answering.
    pub skipped: bool,
}

pub struct Question {
    pub text: String,
    pub options: Vec<String>,
    pub multiple: bool,
}

impl Ask {
    /// The questions of an ASK input, or why they can't be asked.
    pub fn new(input: &Value) -> Result<Ask, String> {
        let qs = input["questions"].as_array().map(Vec::as_slice).unwrap_or_default();
        if qs.is_empty() || qs.len() > 6 {
            return Err("Ask 1 to 6 questions.".into());
        }
        let mut questions = vec![];
        for q in qs {
            let text = q["question"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err("Each question needs a `question` text.".into());
            }
            let options: Vec<String> =
                q["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str()).map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect();
            if options.len() > 10 {
                return Err("At most 10 options per question.".into());
            }
            questions.push(Question { text, options, multiple: q["multiple"].as_bool().unwrap_or(false) });
        }
        Ok(Ask {
            picked: questions.iter().map(|q| vec![false; q.options.len()]).collect(),
            other: questions.iter().map(|_| (String::new(), false)).collect(),
            questions,
            sent: false,
            skipped: false,
        })
    }

    /// Whether question `i` has an answer.
    pub fn answered(&self, i: usize) -> bool {
        self.picked[i].iter().any(|&p| p) || (self.other[i].1 && !self.other[i].0.trim().is_empty())
    }

    pub fn ready(&self) -> bool {
        (0..self.questions.len()).all(|i| self.answered(i))
    }

    /// Picks option `o` of question `i` (or the Other answer, for `None`).
    /// A single-choice question keeps only that one.
    pub fn pick(&mut self, i: usize, o: Option<usize>) {
        let multiple = self.questions[i].multiple;
        let now = match o {
            Some(o) => !self.picked[i][o],
            None => !self.other[i].1,
        };
        if !multiple && now {
            self.picked[i].iter_mut().for_each(|p| *p = false);
            self.other[i].1 = false;
        }
        match o {
            Some(o) => self.picked[i][o] = now,
            None => self.other[i].1 = now,
        }
    }

    /// The answers, for the model.
    pub fn answers(&self) -> String {
        let mut out = String::new();
        for (i, q) in self.questions.iter().enumerate() {
            let mut picks: Vec<String> = q.options.iter().zip(&self.picked[i]).filter(|(_, p)| **p).map(|(o, _)| o.clone()).collect();
            let (other, on) = &self.other[i];
            if *on && !other.trim().is_empty() {
                picks.push(format!("Other: {}", other.trim()));
            }
            out += &format!("{}. {}\n   Answer: {}\n", i + 1, q.text, picks.join("; "));
        }
        out
    }
}

#[derive(Clone)]
pub struct Session {
    pub conv: Arc<store::Shared>,
    pub live: Arc<Mutex<Live>>,
    interrupt: Arc<AtomicBool>,
    /// Mirrors `Conversation::paused`, for the worker to check cheaply.
    paused: Arc<AtomicBool>,
    /// Deleted (AGENT_DELETE): everything the worker does stops, and it ends.
    deleted: Arc<AtomicBool>,
    wake: Sender<()>,
    /// The session's background tasks, shared with its worker (the UI
    /// lists them).
    pub tasks: Arc<Tasks>,
}

/// A queued user message. A held one is being edited in the UI, so it
/// stays in the queue (in its place) and is not sent until released.
#[derive(Clone)]
pub struct Queued {
    pub id: u64,
    pub text: String,
    /// Attached images, as content blocks.
    pub images: Vec<Value>,
    pub held: bool,
}

impl Queued {
    /// The message's content blocks: images first, as the API suggests.
    fn content(self) -> Vec<Value> {
        let mut c = self.images;
        if !self.text.trim().is_empty() {
            c.push(oak::text(&self.text));
        }
        c
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }
}


impl Session {
    pub fn start(mut conv: Conversation, ctx: eframe::egui::Context) -> Session {
        // Closed while a tool ran: its call has no result, which the API
        // refuses, so it gets one saying so.
        let changed = answer_lost_calls(&mut conv);
        // Paused when OAK closed, with the transcript ending in a user turn
        // (tool results or a message the model hasn't answered): the turn
        // carries on once the session is resumed, as it would have without
        // the restart.
        let resume = conv.paused && last_is(&conv, "user");
        if changed {
            conv.save();
        }
        // (A copy of a running session's conversation gets watchers of its own.)
        conv.meters.iter_mut().for_each(|m| m.watched = false);
        let (s, w) = Session::with_worker(conv, ctx, resume);
        // Rewrites left unfinished when OAK closed carry on; progress meters
        // run again, each when it is next due.
        crate::unslop::start(&s.conv, &w.ctx);
        crate::progress::watch(&s.conv, &w.ctx);
        // Tasks left running when OAK closed report their end again.
        w.watch_revived();
        let id = s.conv.lock().unwrap().id.clone();
        std::thread::spawn(move || w.run());
        SESSIONS.lock().unwrap().insert(id, s.clone());
        s
    }

    /// A session for `conv` and the worker sharing its state, not running yet.
    fn with_worker(conv: Conversation, ctx: eframe::egui::Context, resume: bool) -> (Session, Worker) {
        let (wake, wake_rx) = channel();
        let tasks = Tasks::open(&conv.id);
        let s = Session {
            paused: Arc::new(AtomicBool::new(conv.paused)),
            conv: Arc::new(store::Shared::new(conv)),
            live: Arc::default(),
            interrupt: Arc::default(),
            deleted: Arc::default(),
            wake: wake.clone(),
            tasks,
        };
        let w = Worker {
            conv: s.conv.clone(),
            live: s.live.clone(),
            interrupt: s.interrupt.clone(),
            paused: s.paused.clone(),
            deleted: s.deleted.clone(),
            wake_rx: Mutex::new(wake_rx),
            wake,
            tasks: s.tasks.clone(),
            ctx,
            last_request: Mutex::new(None),
            thread: Mutex::new(None),
            warm: Mutex::new(None),
            marks: Mutex::default(),
            files: Mutex::default(),
            resume: AtomicBool::new(resume),
        };
        (s, w)
    }

    /// Wakes the worker, to look at what changed.
    fn wake(&self) {
        let _ = self.wake.send(());
    }

    /// A message from another session (`agent_message`): it goes with this
    /// session's next request, even in the middle of a turn, and wakes it
    /// if it is idle.
    fn deliver(&self, text: String) {
        let mut c = self.conv.lock().unwrap();
        c.inbox.push(text);
        c.save();
        drop(c);
        self.live.lock().unwrap().error = None;
        self.wake();
    }

    /// The user accepted or declined the model's REQUEST_ASK. Accepted, the
    /// model may call ASK once; either way it is told with its next request
    /// (even in the middle of a turn), and an idle session wakes for it.
    pub fn answer_ask_request(&self, accept: bool) {
        let mut c = self.conv.lock().unwrap();
        let about = std::mem::take(&mut c.ask_request);
        if about.is_empty() {
            return;
        }
        c.ask_granted = accept;
        drop(c);
        self.deliver(ask_reply_message(&about, accept));
    }

    /// Deletes the session (AGENT_DELETE, or Delete conversation on Home):
    /// whatever it is doing stops without a report back, its background
    /// tasks are killed, its worker ends, and its conversation file is
    /// removed. The app drops it (`take_deleted`).
    pub(crate) fn delete(&self) {
        let id = {
            let mut c = self.conv.lock().unwrap();
            c.deleted = true;
            c.id.clone()
        };
        self.live.lock().unwrap().quiet_stop = true;
        self.deleted.store(true, Ordering::SeqCst);
        self.tasks.kill_all();
        self.tasks.forget();
        self.wake();
        SESSIONS.lock().unwrap().remove(&id);
        crate::store::remove_conversation(&id);
        DELETED.lock().unwrap().push(id);
    }

    /// What the session is doing, for AGENT_LIST: working, idle or paused.
    fn state(&self) -> String {
        if self.paused.load(Ordering::SeqCst) {
            return "paused".into();
        }
        let waiting = !self.conv.lock().unwrap().inbox.is_empty();
        let l = self.live.lock().unwrap();
        if l.busy {
            if l.status.is_empty() { "working".into() } else { format!("working ({})", l.status.to_lowercase()) }
        } else if waiting || l.ready_count() > 0 {
            "starting on new messages".into()
        } else if l.error.is_some() {
            "idle (its last turn ended with an error)".into()
        } else {
            "idle".into()
        }
    }

    /// Reverts the conversation to before block `block` of message `msg`
    /// (`was` is that block, as the user saw it): it and everything after it
    /// are removed. Only the conversation goes back; files stay as they are.
    /// Whatever OAK is doing stops first.
    pub fn revert(&self, msg: usize, block: usize, was: Value) {
        self.rewind(RevertTo::Block(msg, block, was));
    }

    /// Reverts the conversation to `to`, mid-turn too: what OAK is doing
    /// stops first, then everything from that point on is removed. Only
    /// the conversation goes back; files stay as they are.
    pub fn rewind(&self, to: RevertTo) {
        self.live.lock().unwrap().revert = Some(to);
        self.interrupt.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn send(&self, text: String, images: Vec<Value>) {
        let mut l = self.live.lock().unwrap();
        let id = l.next_queued_id();
        l.queued.push(Queued { id, text, images, held: false });
        l.error = None;
        drop(l);
        self.wake();
    }

    /// Takes a message out of the queue unsent.
    pub fn unqueue(&self, id: u64) {
        self.live.lock().unwrap().queued.retain(|q| q.id != id);
    }

    /// Holds a queued message for editing. False if it was already sent.
    pub fn hold(&self, id: u64) -> bool {
        let mut l = self.live.lock().unwrap();
        l.queued.iter_mut().find(|q| q.id == id).map(|q| q.held = true).is_some()
    }

    /// Releases a held message, with new text if it was edited (empty text
    /// removes it).
    pub fn release(&self, id: u64, text: Option<String>) {
        let mut l = self.live.lock().unwrap();
        if let Some(q) = l.queued.iter_mut().find(|q| q.id == id) {
            q.held = false;
            if let Some(t) = text {
                q.text = t;
            }
        }
        l.queued.retain(|q| !q.is_empty());
        drop(l);
        self.wake();
    }

    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
    }

    /// Pauses or resumes the session. Paused, nothing is sent: a reply
    /// streaming in is dropped (its request goes again on resume, so the
    /// model sees no trace of the pause), a running tool finishes, and
    /// messages, notifications and heartbeats wait. Either way a queued
    /// pause (`request_pause`) is done with.
    pub fn set_paused(&self, on: bool) {
        {
            let mut l = self.live.lock().unwrap();
            l.pause_queued = false;
            if !on {
                l.ultra = false;
            }
        }
        self.paused.store(on, Ordering::SeqCst);
        let mut c = self.conv.lock().unwrap();
        c.paused = on;
        c.save();
        drop(c);
        self.wake();
    }

    /// The pause button: with a request under way, the pause waits for it
    /// to end (pausing mid-reply throws away tokens already paid for), so
    /// the session pauses before its next request; otherwise it pauses
    /// now. True when the pause was queued.
    pub fn request_pause(&self) -> bool {
        let mut l = self.live.lock().unwrap();
        if l.requesting {
            l.pause_queued = true;
            return true;
        }
        drop(l);
        self.set_paused(true);
        false
    }

    /// The ultrapause button: everything stops now. The reply streaming in
    /// is dropped (its request goes again on resume), running commands
    /// and the session's background tasks are killed, and tools waiting
    /// on something (a search, a fetch, a question, a task) give up. The
    /// model learns what was killed from those tools' results once the
    /// session is resumed.
    pub fn ultrapause(&self) {
        self.live.lock().unwrap().ultra = true;
        self.tasks.kill_all();
        self.set_paused(true);
    }

    /// Ultrapause for closing OAK with the background tasks left running:
    /// all else stops at once.
    pub fn ultrapause_keeping_tasks(&self) {
        self.live.lock().unwrap().ultra = true;
        self.set_paused(true);
    }

    /// A pause waits for the request under way to end.
    pub fn pause_queued(&self) -> bool {
        self.live.lock().unwrap().pause_queued
    }

    /// Paused, or a pause queued: what the pause button shows as on.
    pub fn pause_on(&self) -> bool {
        self.paused.load(Ordering::SeqCst) || self.pause_queued()
    }

    /// The pause has taken hold (`Live::pause_settled`).
    pub fn pause_settled(&self) -> bool {
        let paused = self.paused.load(Ordering::SeqCst);
        self.live.lock().unwrap().pause_settled(paused)
    }

    /// A pause is under way but hasn't taken hold yet.
    pub fn pausing(&self) -> bool {
        let paused = self.paused.load(Ordering::SeqCst);
        self.live.lock().unwrap().pausing(paused)
    }

    /// Doing something the user would lose by closing OAK: working (or
    /// pausing), or messages waiting to go. Settled pauses and idle
    /// sessions aren't.
    pub fn active(&self) -> bool {
        let paused = self.paused.load(Ordering::SeqCst);
        let waiting = !paused && !self.conv.lock().unwrap().inbox.is_empty();
        let l = self.live.lock().unwrap();
        if paused || l.pause_queued {
            return !l.pause_settled(paused);
        }
        l.busy || waiting || l.ready_count() > 0
    }

    /// The conversation ids of the subagents this session started.
    pub fn agent_convs(&self) -> Vec<String> {
        self.conv.lock().unwrap().agents.iter().map(|a| a.conv.clone()).collect()
    }

    /// Whether this session is a subagent.
    pub fn is_agent(&self) -> bool {
        self.conv.lock().unwrap().parent.is_some()
    }

    /// Pauses or resumes the goal's auto-continue. The goal stays in the
    /// model's context either way, and the model isn't told. Resumed while
    /// OAK is idle after a reply, the "carry on" the pause held back is
    /// sent now.
    pub fn set_goal_paused(&self, on: bool) {
        let mut c = self.conv.lock().unwrap();
        if c.goal_paused == on {
            return;
        }
        c.goal_paused = on;
        c.save();
        drop(c);
        let mut l = self.live.lock().unwrap();
        l.goal_resume = !on && !l.busy;
        drop(l);
        self.wake();
    }

    /// Queues a /goal, /effort or model change for the next request.
    pub fn command(&self, cmd: Command) {
        let mut l = self.live.lock().unwrap();
        l.commands.push(cmd);
        l.error = None;
        drop(l);
        self.wake();
    }

    /// The user stopped one of the model's heartbeats.
    pub fn stop_heartbeat(&self, id: u64) {
        self.forget(|c| {
            let before = c.heartbeats.len();
            c.heartbeats.retain(|h| h.id != id);
            c.heartbeats.len() != before
        }, format!("heartbeat {id} stopped"));
    }

    /// The user removed one of the model's progress meters.
    pub fn remove_meter(&self, id: u64) {
        self.forget(|c| {
            let before = c.meters.len();
            c.meters.retain(|m| m.id != id);
            c.meters.len() != before
        }, format!("progress meter {id} removed"));
    }

    /// Drops something the model set up (`remove` says whether it was
    /// there), and puts a line in the transcript if it was.
    fn forget(&self, remove: impl FnOnce(&mut Conversation) -> bool, text: String) {
        let mut c = self.conv.lock().unwrap();
        if !remove(&mut c) {
            return;
        }
        add_mark(&mut c, MarkKind::Heartbeat, text, String::new(), api::now_ms());
        c.save();
    }
}

struct Worker {
    conv: Arc<store::Shared>,
    live: Arc<Mutex<Live>>,
    interrupt: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    deleted: Arc<AtomicBool>,
    /// (In a Mutex so tools can run on several threads at once.)
    wake_rx: Mutex<Receiver<()>>,
    wake: Sender<()>,
    tasks: Arc<Tasks>,
    ctx: eframe::egui::Context,
    /// The last chat request sent: its body, how many transcript messages it
    /// carried, and the summary cut it was built on.
    last_request: Mutex<Option<(oak::Request, usize, usize, i64)>>,
    /// Where the server's copy of the conversation stands, with server-side
    /// threads on (`api::threads`): set after each complete reply.
    thread: Mutex<Option<api::Thread>>,
    /// The last main request, while its five-minute cache may be kept warm
    /// (`keep_warm`); and whether keep-alive gave up on it (a ping missed
    /// the cache or failed).
    warm: Mutex<Option<(oak::Request, bool)>>,
    /// Command lines for the transcript, added once a reply shows the
    /// request carrying them got through.
    marks: Mutex<Vec<(MarkKind, String, String)>>,
    /// The files the model read, wrote or edited, as it last saw them.
    files: Mutex<changes::Tracked>,
    /// The session started paused with an unanswered user turn (OAK
    /// closed while paused): it carries on once the session is resumed.
    resume: AtomicBool,
}

enum Reply {
    Done(Vec<Value>, String, Usage),
    Interrupted,
    /// Dropped for a pause: send the same request again on resume.
    Paused,
    /// The usage limit was reached; it resets then (ms since 1970).
    Limited(i64),
    Failed(String),
}

/// Token counts the API reported for one request.
#[derive(Clone, Copy, Default)]
struct Usage {
    /// The whole prompt: fresh, cache-written and cache-read input.
    prompt: u64,
    output: u64,
    input: u64,
    cache_read: u64,
    write: u64,
}

impl Usage {
    fn read(&mut self, u: &oak::Usage) {
        self.prompt = self.prompt.max(u.prompt());
        self.output = self.output.max(u.output);
        self.input = self.input.max(u.input);
        self.cache_read = self.cache_read.max(u.cache_read);
        self.write = self.write.max(u.write);
    }
}

/// Adds `s` to block `b`'s string at `key`.
fn append(b: &mut Value, key: &str, s: &str) {
    match b.get_mut(key) {
        Some(Value::String(old)) => old.push_str(s),
        _ => b[key] = json!(s),
    }
}

/// Whether the transcript ends in a message of `role`.
fn last_is(c: &Conversation, role: &str) -> bool {
    c.messages.last().is_some_and(|m| m["role"] == role)
}

/// The text of the model's last reply, if the transcript ends in one.
fn reply_text(c: &Conversation) -> Option<String> {
    let m = c.messages.last().filter(|m| m["role"] == "assistant")?;
    let texts: Vec<&str> = m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    Some(texts.join("\n\n"))
}

/// A benchmark timeline event for conversation `conv` (`OAK_BENCH_LOG`).
fn bench(conv: &store::Shared, ev: &str, extra: Value) {
    if crate::bench::on() {
        let id = conv.lock().unwrap().id.clone();
        crate::bench::log(&id, ev, extra);
    }
}

/// Milliseconds since `t`, for the benchmark timeline.
fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Puts a line at the transcript's end: `text` for the user, `sent` what
/// the model was sent with it (empty for none).
fn add_mark(c: &mut Conversation, kind: MarkKind, text: String, sent: String, time: i64) -> &mut Mark {
    let at = c.messages.len();
    c.marks.push(Mark { sent, ..Mark::new(at, kind, text, time) });
    c.marks.last_mut().unwrap()
}

impl Worker {
    fn run(self) {
        loop {
            // Deleted: the turn under way has stopped. Commands it left
            // running as tasks are killed too.
            if self.deleted.load(Ordering::SeqCst) {
                self.tasks.kill_all();
                self.idle();
                return;
            }
            // A revert the user asked for: whatever was running has stopped.
            let revert = self.live.lock().unwrap().revert.take();
            if let Some(to) = revert {
                self.apply_revert(&to);
            }
            if self.retry_limited() {
                continue;
            }
            if self.paused.load(Ordering::SeqCst) {
                self.idle();
                if self.wake_rx.lock().unwrap().recv().is_err() {
                    return;
                }
                continue;
            }
            let (users, notes, goal_resume) = {
                let mut l = self.live.lock().unwrap();
                (l.take_queued(), std::mem::take(&mut l.notes), std::mem::take(&mut l.goal_resume))
            };
            // (Saved: the push below saves them in the transcript instead.)
            let inbox = std::mem::take(&mut self.conv.lock().unwrap().inbox);
            let beats = due_beats(&mut self.conv.lock().unwrap(), api::now_ms());
            let goal = self.take_commands();
            // The goal resumed after the model's turn ended while it was
            // paused: the carry-on it would have got then. (Not when the
            // user writes: their message leads.)
            let carry_on = (goal_resume && users.is_empty() && goal.is_empty()).then(|| self.goal_carry_on()).flatten();
            // A turn cut off by closing OAK while paused, now resumed.
            let resume = self.resume.swap(false, Ordering::SeqCst) && last_is(&self.conv.lock().unwrap(), "user");
            if users.is_empty() && notes.is_empty() && inbox.is_empty() && beats.is_empty() && goal.is_empty() && !resume && carry_on.is_none() {
                // An /effort (or a model) while idle needs no request: it
                // is set now. A goal's line waits for a reply that shows
                // the model got it.
                self.add_marks(false);
                if !self.sleep_idle() {
                    return;
                }
                continue;
            }
            self.set(|l| l.busy = true);
            if let Some(first) = users.first() {
                self.first_title(first);
            }
            self.name_session(users.first().map(|u| u.text.as_str()));
            let texts = notes.into_iter().map(|(_, t)| t).chain(beats).chain(inbox);
            let mut content: Vec<Value> = texts.map(|t| oak::text(&t)).collect();
            content.extend(self.take_held());
            content.extend(goal);
            content.extend(carry_on);
            content.extend(users.into_iter().flat_map(Queued::content));
            if !content.is_empty() {
                self.push("user", content);
            }
            self.turn();
        }
    }

    /// Stopped by a usage limit before a restart: tries the same request
    /// again at once (the account may have changed, and a refused request
    /// costs nothing); if it's still limited, OAK waits for the reset then.
    /// True when it ran the turn.
    fn retry_limited(&self) -> bool {
        let (limited, resumable) = {
            let mut c = self.conv.lock().unwrap();
            let limited = c.limit_wait.take().is_some();
            if limited {
                c.save();
            }
            // (Only then: it reads the messages.)
            (limited, limited && last_is(&c, "user"))
        };
        if !limited {
            return false;
        }
        if !resumable {
            self.finish_limit("stopped");
            return false;
        }
        self.resume.store(false, Ordering::SeqCst);
        self.set(|l| l.busy = true);
        self.turn();
        true
    }

    /// The carry-on the goal would have got when the model's turn ended,
    /// had it not been paused then.
    fn goal_carry_on(&self) -> Option<Value> {
        let mut c = self.conv.lock().unwrap();
        let block = if last_is(&c, "assistant") && !c.goal.is_empty() { after_turn(&mut c, false, api::now_ms()) } else { None };
        if block.is_some() {
            c.save();
        }
        block
    }

    /// Idle: keeps the cache warm (a ping due now goes now) and sleeps
    /// until woken, or the next heartbeat or keep-alive is due. False when
    /// the session is gone.
    fn sleep_idle(&self) -> bool {
        self.idle();
        let mut ping = self.keep_alive_due();
        if ping.is_some_and(|at| at <= api::now_ms()) {
            self.keep_warm();
            ping = self.keep_alive_due();
        }
        let beat = self.conv.lock().unwrap().heartbeats.iter().map(|h| h.next_at).min();
        let next = [beat, ping].into_iter().flatten().min();
        let woke = match next {
            Some(at) => self.wake_rx.lock().unwrap().recv_timeout(Duration::from_millis((at - api::now_ms()).clamp(0, 60_000) as u64)),
            None => self.wake_rx.lock().unwrap().recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        woke != Err(RecvTimeoutError::Disconnected)
    }

    /// An untitled conversation's first title: the first line of the
    /// user's first message, or its image's name.
    fn first_title(&self, first: &Queued) {
        let mut c = self.conv.lock().unwrap();
        if c.title.is_empty() {
            let line = first.text.lines().next().unwrap_or("").head(60);
            c.title = match first.images.first() {
                Some(b) if line.trim().is_empty() => b["_oak_file"].as_str().map_or("Image".into(), |n| n.to_string()),
                _ => line,
            };
        }
    }

    /// Names a new main session from its first message (or its goal) with a
    /// side request to a small model (`title_for`), so its own first request
    /// goes straight to the work. The first line of the message stays the
    /// title if that fails, or if the user renamed the session meanwhile.
    fn name_session(&self, text: Option<&str>) {
        let c = self.conv.lock().unwrap();
        if !c.messages.is_empty() || c.titled || c.parent.is_some() {
            return;
        }
        let text = match text.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => t.to_string(),
            None if !c.goal.trim().is_empty() => c.goal.clone(),
            None => return,
        };
        let (before, model) = (c.title.clone(), c.model.clone());
        drop(c);
        let (conv, ctx) = (self.conv.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let (title, usd) = title_for(&text, &model);
            let mut c = conv.lock().unwrap();
            c.spent += usd; // through OpenRouter, the title is the session's too
            let Some(title) = title else { return c.save() };
            if c.titled || c.title != before {
                return;
            }
            c.title = title;
            c.titled = true;
            c.save();
            drop(c);
            ctx.request_repaint();
        });
    }

    /// Reverts the conversation (see `Session::revert`) and clears what
    /// was under way.
    fn apply_revert(&self, to: &RevertTo) {
        let mut c = self.conv.lock().unwrap();
        let done = match to {
            RevertTo::Block(msg, block, was) => revert_to(&mut c, *msg, *block, was),
            RevertTo::End(n) => revert_to_end(&mut c, *n),
        };
        if done {
            c.save();
        }
        drop(c);
        self.resume.store(false, Ordering::SeqCst);
        self.marks.lock().unwrap().clear();
        *self.last_request.lock().unwrap() = None;
        self.set(|l| {
            l.reverts += 1;
            l.blocks.clear();
            l.ask = None;
            l.error = None;
            l.status.clear();
        });
    }

    /// Waits until woken or `d` passes. False when OAK is closing.
    fn nap(&self, d: Duration) -> bool {
        self.wake_rx.lock().unwrap().recv_timeout(d) != Err(RecvTimeoutError::Disconnected)
    }

    /// A revert is waiting: the turn stops.
    fn reverting(&self) -> bool {
        self.live.lock().unwrap().revert.is_some()
    }

    fn set(&self, f: impl FnOnce(&mut Live)) {
        f(&mut self.live.lock().unwrap());
        self.ctx.request_repaint();
    }

    /// Nothing under way: not busy, no status.
    fn idle(&self) {
        self.set(|l| {
            l.busy = false;
            l.status.clear();
        });
    }

    /// Saves the conversation, lets it go and repaints.
    #[track_caller]
    fn save(&self, c: store::SharedGuard) {
        c.save();
        drop(c);
        self.ctx.request_repaint();
    }

    /// A benchmark timeline event for this conversation (`OAK_BENCH_LOG`).
    fn bench(&self, ev: &str, extra: Value) {
        bench(&self.conv, ev, extra);
    }

    /// Freebuff bills the hour as it runs: a line when the hour limit is
    /// reached and one when the grace period ends, plus a message asking the
    /// model to carry on.
    fn freebuff_alerts(&self) {
        if !crate::freebuff::serves(&self.conv.lock().unwrap().model) {
            return;
        }
        crate::freebuff::poll_alerts();
        let alerts = crate::freebuff::take_alerts();
        if alerts.is_empty() {
            return;
        }
        let now = api::now_ms();
        for a in alerts {
            let mut c = self.conv.lock().unwrap();
            let at = c.messages.len();
            match a {
                crate::freebuff::Alert::Grace { left_ms } => {
                    let text = format!("Freebuff hour limit was reached, grace period activated for {}m", (left_ms / 60_000).max(1));
                    c.marks.push(Mark::new(at, MarkKind::FreebuffHour, text, now));
                }
                crate::freebuff::Alert::GraceOver => {
                    c.marks.push(Mark::new(at, MarkKind::FreebuffGrace, String::new(), now));
                    drop(c);
                    let mut l = self.live.lock().unwrap();
                    let id = l.next_queued_id();
                    l.queued.push(Queued { id, text: "Continue where you left off.".into(), images: vec![], held: false });
                }
            }
        }
    }

    /// Appends a message, merging into the last one when it has the same
    /// role (after an abandoned reply, the transcript still ends in a user turn).
    /// Each block gets `_oak_at`, when it went in (ms since 1970; dropped
    /// before sending), for the time the transcript shows.
    fn push(&self, role: &str, mut content: Vec<Value>) {
        let now = api::now_ms();
        for b in &mut content {
            if let Some(o) = b.as_object_mut() {
                o.entry("_oak_at").or_insert(json!(now));
            }
        }
        let mut c = self.conv.lock().unwrap();
        // (Looked at first: changing the last message redoes its saved JSON.)
        match c.messages.last().filter(|last| last["role"] == role) {
            Some(_) => c.messages.last_mut().unwrap()["content"].as_array_mut().unwrap().append(&mut content),
            None => c.messages.push(json!({"role": role, "content": content})),
        }
        c.last_message_at = now;
        self.save(c);
    }

    /// Sends the queued user messages after an interrupt. False if there
    /// were none, or a revert is waiting (the messages then wait for it).
    fn absorb_queued(&self) -> bool {
        let users = {
            let mut l = self.live.lock().unwrap();
            if l.revert.is_some() {
                return false;
            }
            l.take_queued()
        };
        if users.is_empty() {
            return false;
        }
        self.push("user", users.into_iter().flat_map(Queued::content).collect());
        true
    }

    /// Puts the notifications of tasks that finished since the last request,
    /// and messages from other sessions, into the user turn it carries (after
    /// the tool results), so the model hears of them mid-turn, where they
    /// happened, rather than all at once when its turn ends. Not while a
    /// revert waits.
    fn absorb_notices(&self) {
        if self.reverting() {
            return;
        }
        let inbox = std::mem::take(&mut self.conv.lock().unwrap().inbox);
        let notes = std::mem::take(&mut self.live.lock().unwrap().notes);
        let mut content: Vec<Value> = notes.iter().map(|(_, t)| oak::text(t)).collect();
        content.extend(inbox.iter().map(|t| oak::text(t)));
        if !content.is_empty() {
            self.push("user", content);
        }
    }

    /// A subagent's turn ended: its final reply (or why it stopped) goes to
    /// the session that started it, unless that session stopped it.
    fn report_to_parent(&self) {
        let c = self.conv.lock().unwrap();
        let Some(parent) = c.parent.clone() else { return };
        let from = agent_label(c.agent_id, &c.title);
        let reply = reply_text(&c);
        drop(c);
        let (error, quiet) = {
            let mut l = self.live.lock().unwrap();
            (l.error.clone(), std::mem::take(&mut l.quiet_stop))
        };
        if quiet || self.reverting() || self.deleted.load(Ordering::SeqCst) {
            return;
        }
        // A turn that ended with a reply succeeded (an error shown from an
        // earlier turn may still be set); without one, it failed or was cut short.
        let body = match (reply, error) {
            (Some(t), _) if !t.trim().is_empty() => t,
            (Some(_), _) => "(The agent ended its turn without a final message.)".into(),
            (None, Some(e)) => format!("(The agent stopped with an error: {e})"),
            (None, None) => "(The agent was stopped before it finished its turn.)".into(),
        };
        if let Some(s) = find_session(&parent) {
            s.deliver(agent_message(&from, true, &body));
        }
    }

    /// Applies the queued commands. Returns the blocks that tell the model
    /// about a new goal, for the next user turn.
    fn take_commands(&self) -> Vec<Value> {
        let cmds = std::mem::take(&mut self.live.lock().unwrap().commands);
        if cmds.is_empty() {
            return vec![];
        }
        let mut c = self.conv.lock().unwrap();
        let mut blocks = vec![];
        for cmd in cmds {
            let (block, mark) = apply_command(&mut c, cmd);
            let sent = block.as_ref().and_then(|b| b["text"].as_str()).map(String::from).or_else(|| c.held.last().cloned()).unwrap_or_default();
            blocks.extend(block);
            self.marks.lock().unwrap().push((mark.0, mark.1, sent));
        }
        self.save(c);
        blocks
    }

    /// The notes that waited for a request (the goal was cleared), as blocks.
    fn take_held(&self) -> Vec<Value> {
        let mut c = self.conv.lock().unwrap();
        if c.held.is_empty() {
            return vec![];
        }
        let held = std::mem::take(&mut c.held);
        held.iter().map(|t| goal_block(t)).collect()
    }

    /// Puts the pending command lines in the transcript, before the next
    /// message: all of them once a reply came, else only the efforts and
    /// models (they take effect without the model being told).
    fn add_marks(&self, replied: bool) {
        let (marks, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut *self.marks.lock().unwrap())
            .into_iter()
            .partition(|(kind, _, _)| replied || matches!(kind, MarkKind::Effort | MarkKind::Model));
        *self.marks.lock().unwrap() = wait;
        if marks.is_empty() {
            return;
        }
        let mut c = self.conv.lock().unwrap();
        for (kind, text, sent) in marks {
            add_mark(&mut c, kind, text, sent, api::now_ms());
        }
        self.save(c);
    }

    /// The user interrupted, or the session was deleted (which every wait
    /// and tool sees, for good).
    fn interrupted(&self) -> bool {
        self.deleted.load(Ordering::SeqCst) || self.interrupt.swap(false, Ordering::SeqCst)
    }

    /// Whether a running tool should stop: the user interrupted, or
    /// ultrapaused (which every running tool sees, until resumed).
    fn stopping(&self) -> bool {
        self.ultra() || self.interrupted()
    }

    fn ultra(&self) -> bool {
        self.live.lock().unwrap().ultra
    }

    /// Sleeps `ms` unless something for the worker comes in (a wake, or a
    /// message from another session). Tools that wait on something use it,
    /// so a pause, an interrupt or a new message ends their wait at once.
    fn wait_a_bit(&self, ms: u64) {
        let _ = self.wake_rx.lock().unwrap().recv_timeout(Duration::from_millis(ms));
    }

    /// Notes that the usage limit was reached: a line in the transcript
    /// (or the waiting one, with the new reset time) and the time to wait for.
    fn record_limit(&self, at: i64) {
        let mut c = self.conv.lock().unwrap();
        c.limit_wait = Some(at);
        match c.marks.iter_mut().rev().find(|m| m.kind == MarkKind::Limit && m.text.is_empty()) {
            Some(m) => m.time = at,
            None => {
                add_mark(&mut c, MarkKind::Limit, String::new(), String::new(), at);
            }
        }
        self.save(c);
    }

    /// The model ended its turn. With a goal set and not finished, asks it
    /// to carry on and returns true; with the goal finished, ends the goal
    /// (control goes back to the user). An interrupt ends the turn as usual.
    fn goal_step(&self) -> bool {
        let mut c = self.conv.lock().unwrap();
        // (Paused, it carries on once resumed.)
        let step = after_turn(&mut c, self.interrupted(), api::now_ms());
        self.save(c);
        match step {
            Some(block) => {
                self.push("user", vec![block]);
                self.set(|l| l.status = "Continuing the goal".into());
                true
            }
            None => false,
        }
    }

    /// The account rotated: a line in the transcript (never sent).
    fn note_rotation(&self, label: String) {
        let mut c = self.conv.lock().unwrap();
        add_mark(&mut c, MarkKind::Rotated, label, String::new(), api::now_ms());
        c.save();
        drop(c);
        self.set(|l| l.status = "Waiting for the model".into());
    }

    /// Marks the waiting limit line "continued" or "stopped".
    fn finish_limit(&self, how: &str) {
        let mut c = self.conv.lock().unwrap();
        let now = api::now_ms();
        let mut changed = false;
        for m in c.marks.iter_mut().filter(|m| m.kind == MarkKind::Limit && m.text.is_empty()) {
            m.text = how.into();
            m.done = now;
            changed = true;
        }
        if changed {
            self.save(c);
        }
    }

    /// Waits until a usage limit resets, if one was hit. False if the user
    /// interrupted meanwhile (then OAK stops waiting).
    fn wait_for_limit(&self) -> bool {
        let Some(at) = self.conv.lock().unwrap().limit_wait else { return true };
        self.set(|l| {
            l.blocks.clear();
            l.status = "Waiting for the usage limit to reset".into();
        });
        let since = api::accounts_freed();
        let cleared = loop {
            if self.interrupted() {
                break false;
            }
            let left = at + LIMIT_SLACK_MS - api::now_ms();
            // Done at the reset, or at once when the user signed in with
            // another account or one came free early.
            if limit_over(left, since, api::accounts_freed()) {
                break true;
            }
            // (Paused meanwhile, nothing runs: the pause has taken hold.)
            self.live.lock().unwrap().held = self.paused.load(Ordering::SeqCst);
            if !self.nap(Duration::from_millis(left.min(1000) as u64)) {
                return false;
            }
        };
        self.live.lock().unwrap().held = false;
        let mut c = self.conv.lock().unwrap();
        c.limit_wait = None;
        c.save();
        drop(c);
        if cleared {
            self.set(|l| l.status = "Waiting for the model".into());
        }
        cleared
    }

    /// Waits while the session is paused. False if the user interrupted
    /// meanwhile (the turn ends there).
    fn hold_while_paused(&self) -> bool {
        if !self.paused.load(Ordering::SeqCst) {
            return true;
        }
        self.set(|l| {
            l.blocks.clear();
            l.status = "Paused".into();
            l.held = true;
        });
        let resumed = loop {
            if !self.paused.load(Ordering::SeqCst) {
                break true;
            }
            if self.interrupted() {
                break false;
            }
            if !self.nap(Duration::from_millis(200)) {
                break false;
            }
        };
        self.set(|l| l.held = false);
        resumed
    }

    /// Waits `d` before a retry. False if the user interrupted meanwhile;
    /// a pause ends the wait early (the turn loop then holds until resume).
    fn retry_sleep(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        loop {
            if self.interrupted() {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || self.paused.load(Ordering::SeqCst) {
                return true;
            }
            if !self.nap(left.min(Duration::from_millis(200))) {
                return false;
            }
        }
    }

    fn turn(&self) {
        // The tool calls run on threads of this scope (they may start while
        // their reply still streams in).
        std::thread::scope(|s| self.turn_loop(s));
        // A limit line still waiting means OAK gave up on it (interrupted,
        // or a different error after the reset).
        self.finish_limit("stopped");
        if crate::bench::on() {
            let c = self.conv.lock().unwrap();
            let (id, parent, text) = (c.id.clone(), c.parent.clone(), reply_text(&c).unwrap_or_default());
            drop(c);
            let error = self.live.lock().unwrap().error.clone();
            crate::bench::log(&id, "turn_end", json!({"parent": parent, "text": text, "error": error}));
        }
        self.report_to_parent();
    }

    fn turn_loop<'scope, 'env>(&'env self, scope: &'scope std::thread::Scope<'scope, 'env>) {
        self.interrupt.store(false, Ordering::SeqCst);
        let mut failures = 0;
        let mut refusals = 0;
        let mut overflowed = false;
        loop {
            if self.reverting() {
                return;
            }
            if self.interrupted() && !self.absorb_queued() {
                return;
            }
            if !self.wait_for_limit() && !self.absorb_queued() {
                return;
            }
            if !self.hold_while_paused() {
                return; // interrupted while paused: what's queued waits for resume
            }
            self.gather_input();
            let mut batch = Batch::new(self, scope);
            let (blocks, stop) = match self.ask(&mut batch) {
                Reply::Done(b, s, usage) => {
                    self.finish_limit("continued");
                    self.add_marks(true);
                    self.after_reply(usage);
                    (b, s)
                }
                Reply::Interrupted => {
                    if self.absorb_queued() {
                        continue;
                    }
                    return;
                }
                Reply::Paused => continue,
                Reply::Limited(at) => {
                    self.record_limit(at);
                    continue;
                }
                Reply::Failed(e) if e.contains("prompt is too long") && !overflowed => {
                    // Outgrew the model's context before a summary landed:
                    // summarize now (blocking) and send again.
                    overflowed = true;
                    match self.summarize_now(&e) {
                        Ok(()) => continue,
                        Err(why) => return self.set(|l| l.error = Some(format!("{e}\n(Couldn't make room by summarizing: {why})"))),
                    }
                }
                Reply::Failed(e) => {
                    // Overloaded and dropped streams are worth another go.
                    failures += 1;
                    if failures < 4 && (e.contains("verloaded") || e.starts_with("Stream")) {
                        self.set(|l| l.status = format!("Retrying after: {e}"));
                        std::thread::sleep(Duration::from_secs(3 * failures));
                        continue;
                    }
                    return self.set(|l| l.error = Some(e));
                }
            };
            failures = 0;
            if let Some(detail) = stop.strip_prefix("refusal") {
                refusals += 1;
                if !self.wait_after_refusal(refusals, detail) {
                    return;
                }
                continue;
            }
            refusals = 0;
            let calls = self.keep_reply(blocks);
            if calls.is_empty() {
                // A goal that isn't finished keeps the model going.
                if self.goal_step() {
                    continue;
                }
                let error = match stop.as_str() {
                    oak::stop::MAX_TOKENS => "The reply hit the output token limit.",
                    oak::stop::CONTEXT => "The reply filled the model's context window.",
                    _ => return,
                };
                return self.set(|l| l.error = Some(error.into()));
            }
            let stopped = batch.finish(&calls, &stop);
            // Files OAK's own commands changed aren't news to the model.
            if calls.iter().any(|b| matches!(b["name"].as_str(), Some("EXECUTE" | "TASK_NEW")) || (b["name"] == "TASK" && b["input"]["action"] == "new")) {
                let t = Instant::now();
                self.files.lock().unwrap().refresh();
                self.bench("files_refresh", json!({"ms": ms_since(t)}));
            }
            // Stopped by an ultrapause, the turn holds (paused) and carries
            // on from the killed calls' results once resumed.
            if stopped && !self.paused.load(Ordering::SeqCst) && !self.absorb_queued() {
                return;
            }
        }
    }

    /// Messages and commands typed meanwhile ride along with the next
    /// request (the transcript ends in a user turn here, after the tool
    /// results, so they join it), and so do messages from other sessions.
    fn gather_input(&self) {
        self.absorb_notices();
        self.absorb_queued();
        let mut goal = self.take_commands();
        goal.extend(self.take_held());
        if !goal.is_empty() {
            self.push("user", goal);
        }
        self.report_changes();
    }

    /// The reply was declined by a safety classifier (the partial reply is
    /// dropped): it is retried on the same schedule as failed requests,
    /// then every minute and a half until it goes through or the user
    /// interrupts. Messages typed meanwhile go with the next try. False
    /// when the turn ends instead.
    fn wait_after_refusal(&self, refusals: u32, detail: &str) -> bool {
        let why = detail.trim();
        let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
        let wait = api::retry_wait(refusals);
        let of = if refusals <= api::RETRIES { format!("{refusals}/{}", api::RETRIES) } else { refusals.to_string() };
        self.set(|l| l.status = format!("Retry {of} in {wait}s after: a safety classifier declined the request{why}"));
        self.retry_sleep(Duration::from_secs(wait)) || self.absorb_queued()
    }

    /// Adds a complete reply to the transcript; returns its tool calls.
    /// Unslop: the transcript shows the text rewritten; the model's own
    /// text stays as it is, and is what gets sent.
    fn keep_reply(&self, mut blocks: Vec<Value>) -> Vec<Value> {
        let calls: Vec<Value> = blocks.iter().filter(|b| oak::is_call(b)).cloned().collect();
        for b in &mut blocks {
            b.as_object_mut().unwrap().remove("_oak_raw");
        }
        if !blocks.is_empty() {
            let unslop = crate::unslop::applies(&self.conv.lock().unwrap()) && crate::unslop::mark(&mut blocks);
            let rewrote = blocks.iter().any(|b| b.get(crate::unslop::KEY).is_some());
            self.push("assistant", blocks);
            if unslop {
                crate::unslop::start(&self.conv, &self.ctx);
            } else if rewrote {
                // Thinking rewritten while the reply streamed in.
                crate::unslop::settle(&self.conv);
            }
        }
        calls
    }

}

/// A tool call's result block.
fn result_block(call: &Value, content: Value, is_error: bool) -> Value {
    oak::tool_result(&call["id"], content, is_error)
}

/// Reverts the conversation to before block `block` of message `msg`,
/// which must still be `was`: that block and everything after it go, and
/// so do the transcript lines after it and a summary that covered any of
/// it. Files aren't touched. False if the conversation no longer matches.
///
/// In a user message the earlier blocks (tool results) stay. In a reply
/// the earlier blocks stay too, unless only thinking is left (thinking at
/// the end of a reply goes with what it led to); a call among them keeps
/// its result, and the results of the calls cut go with them. The
/// conversation then ends where it can take a new message from the user.
fn revert_to(c: &mut Conversation, msg: usize, block: usize, was: &Value) -> bool {
    let Some(m) = c.messages.get_mut(msg) else { return false };
    let user = m["role"] == "user";
    let Some(content) = m["content"].as_array_mut() else { return false };
    let matches = match content.get(block) {
        Some(b) if user => b == was,
        Some(b) => same_block(b, was),
        None => false,
    };
    if !matches {
        return false;
    }
    content.truncate(block);
    if !user {
        while content.last().is_some_and(oak::is_reasoning) {
            content.pop();
        }
    }
    let calls: Vec<Value> = content.iter().filter(|b| oak::is_call(b)).cloned().collect();
    let keep = !content.is_empty();
    // The kept calls' results, from the user turn after the reply (a call
    // whose result never came gets the one a stopped call gets).
    let results: Vec<Value> = if user || calls.is_empty() {
        vec![]
    } else {
        let next: Vec<Value> = c.messages.get(msg + 1).filter(|m| m["role"] == "user").and_then(|m| m["content"].as_array()).cloned().unwrap_or_default();
        calls
            .iter()
            .map(|call| next.iter().find(|r| oak::is_result(r) && r["call_id"] == call["id"]).cloned().unwrap_or_else(|| result_block(call, json!(STOPPED_RESULT), true)))
            .collect()
    };
    cut_messages(c, if keep { msg + 1 } else { msg }, msg);
    if !results.is_empty() {
        c.messages.push(json!({"role": "user", "content": results}));
    }
    true
}

/// Reverts the conversation to its first `n` messages (before a reply that
/// was streaming in): what came after them goes. False if it has fewer.
fn revert_to_end(c: &mut Conversation, n: usize) -> bool {
    if c.messages.len() < n {
        return false;
    }
    cut_messages(c, n, n);
    true
}

/// Keeps the first `n` messages, `changed` being the first one that is no
/// longer as it was: the transcript lines after it go, and so does a
/// summary that covered it.
fn cut_messages(c: &mut Conversation, n: usize, changed: usize) {
    c.messages.truncate(n);
    c.marks.retain(|m| m.at <= changed);
    if c.rolling.summary.as_ref().is_some_and(|s| s.cut > changed) {
        c.rolling.summary = None;
        c.rolling.measured.clear();
    }
    c.rolling.measured.retain(|m| m.n <= changed);
    c.limit_wait = None;
}

/// Whether a reply's block is still `was`, the one the user saw: OAK adds
/// its own fields to a reply's blocks after the fact (a rewrite, token
/// counts), so only what the model sent counts.
fn same_block(b: &Value, was: &Value) -> bool {
    ["type", "id", "text", "sig", "data", "input"].iter().all(|k| b.get(k) == was.get(k))
}

/// The result of a call that a revert keeps but whose result never came.
const STOPPED_RESULT: &str = "No result: the user stopped this call before it finished. Check its effects before relying on them.";

/// The result given to a tool call whose own result never made it into
/// the transcript (OAK closed or crashed while it ran).
const LOST_RESULT: &str = "No result: Oak closed while this call was running, so it may not have finished. Check its effects before relying on them, and run it again if needed.";

/// Gives every tool call without a result one (`LOST_RESULT`), so the
/// transcript is valid again: the APIs refuse a call not answered by a
/// result in the next message. Calls are saved before they run and
/// their results after, so closing OAK meanwhile leaves them unanswered.
/// The results go first in the next user message (where the API wants
/// them), or in a new one at the end. True if anything changed.
/// Whether any call in messages kept as JSON has no result, as
/// `answer_lost_calls` would find; None when they don't read as it expects.
fn lost_calls_in(messages: &[std::sync::Arc<[u8]>]) -> Option<bool> {
    #[derive(serde::Deserialize)]
    struct Message<'a> {
        #[serde(default)]
        role: Value,
        #[serde(default, borrow)]
        content: Option<&'a serde_json::value::RawValue>,
    }
    #[derive(serde::Deserialize)]
    struct Block {
        #[serde(default, rename = "type")]
        kind: Value,
        #[serde(default)]
        id: Value,
        #[serde(default)]
        call_id: Value,
    }
    // Each message's role and its blocks' kinds and ids, nothing else.
    let read = |m: &[u8]| -> Option<(Value, Vec<Block>)> {
        let m: Message = serde_json::from_slice(m).ok()?;
        let blocks = match m.content {
            Some(c) if c.get().trim_start().starts_with('[') => serde_json::from_str(c.get()).ok()?,
            _ => vec![],
        };
        Some((m.role, blocks))
    };
    // From the end, with the message after each.
    let mut next: Option<(Value, Vec<Block>)> = None;
    for m in messages.iter().rev() {
        let (role, blocks) = read(m)?;
        let mut calls = blocks.iter().filter(|b| b.kind == "tool_call").peekable();
        if role == "assistant" && calls.peek().is_some() {
            let answered: Option<Vec<&Value>> = match &next {
                Some((role, next)) if *role == "user" => Some(next.iter().filter(|b| b.kind == "tool_result").map(|b| &b.call_id).collect()),
                Some(_) => None, // two replies in a row: not something it mends
                None => Some(vec![]),
            };
            if let Some(answered) = answered
                && calls.any(|call| !answered.contains(&&call.id))
            {
                return Some(true);
            }
        }
        next = Some((role, blocks));
    }
    Some(false)
}

/// Looks for calls without a result in a conversation not yet read from
/// its file, as `answer_lost_calls` will (to do it ahead, in parallel).
pub(crate) fn look_for_lost_calls(c: &mut Conversation) {
    if let Some(raw) = c.messages.unread()
        && lost_calls_in(&raw) == Some(false)
    {
        c.messages.no_lost_calls = true;
    }
}

pub(crate) fn answer_lost_calls(c: &mut Conversation) -> bool {
    // Not yet read from its file: looked at there, without parsing it all
    // (every conversation is looked at, at every start).
    if let Some(raw) = c.messages.unread()
        && (c.messages.no_lost_calls || lost_calls_in(&raw) == Some(false))
    {
        c.messages.no_lost_calls = true;
        return false;
    }
    let mut changed = false;
    for i in 0..c.messages.len() {
        // (Looked at in place: every conversation is, at every start, and
        // nearly every call has its result.)
        let m = &c.messages[i];
        if m["role"] != "assistant" {
            continue;
        }
        let calls = || m["content"].as_array().into_iter().flatten().filter(|b| oak::is_call(b));
        if calls().next().is_none() {
            continue;
        }
        let answered: Vec<&Value> = match c.messages.get(i + 1) {
            Some(next) if next["role"] == "user" => next["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| oak::is_result(b))
                .map(|b| &b["call_id"])
                .collect(),
            Some(_) => continue, // two replies in a row: not something this can mend
            None => vec![],
        };
        let lost: Vec<Value> = calls().filter(|call| !answered.contains(&&call["id"])).map(|call| result_block(call, json!(LOST_RESULT), true)).collect();
        if lost.is_empty() {
            continue;
        }
        changed = true;
        match c.messages.get_mut(i + 1) {
            Some(next) => {
                let rest = match next["content"].take() {
                    Value::Array(a) => a,
                    Value::String(s) if !s.is_empty() => vec![oak::text(&s)],
                    _ => vec![],
                };
                next["content"] = Value::Array(lost.into_iter().chain(rest).collect());
            }
            None => c.messages.push(json!({"role": "user", "content": lost})),
        }
    }
    changed
}

/// Applies a command to the conversation: the block telling the model
/// (for a goal) and the line for the transcript.
fn apply_command(c: &mut Conversation, cmd: Command) -> (Option<Value>, (MarkKind, String)) {
    match cmd {
        Command::Goal(goal) => {
            let cleared = goal.is_empty();
            let note = if cleared {
                "The user cleared the goal: you no longer need to call GOAL.".to_string()
            } else {
                goal_prompt(&goal)
            };
            // A cleared goal gives the model nothing to do: its note waits
            // for the next request. A new goal supersedes a waiting one.
            c.held.clear();
            if cleared {
                c.held.push(note.clone());
            }
            // A local model has the goal's tools only while there is one
            // (it can't reach a tool missing from its list): they come and
            // go at once, so its prefix is renewed. Every other model always
            // has them, so a goal changes nothing before the conversation.
            if crate::local::serves(&c.model) && c.goal.is_empty() != cleared {
                c.cache.cold();
            }
            c.goal = goal.clone();
            c.goal_justification.clear();
            c.goal_finished = false;
            c.goal_paused = false;
            end_ask_request(c);
            ((!cleared).then(|| goal_block(&note)), (MarkKind::Goal, goal))
        }
        Command::Effort(e) => {
            // Claude's effort is part of its cache key: the next request
            // writes it all, which makes it a moment to bring the prefix up
            // to date. Unless it can change per message (`crate::prefix`).
            let per_message = crate::prefix::mode(c.provider(), &c.model) == Some(crate::prefix::Mode::Claude) && crate::prefix::supports_effort(&c.model);
            if e != c.effort && c.provider() == api::Provider::Claude && !per_message {
                c.cache.cold();
            }
            c.effort = e.clone();
            (None, (MarkKind::Effort, e))
        }
        // The model isn't told: it reads the conversation as it was. The
        // effort stays as set, and goes as the new model takes it
        // (`effort_for`).
        Command::Model(p, m) => {
            if m != c.model || Some(p) != c.provider {
                c.model = m.clone();
                c.cache.cold();
                // Another model may be served elsewhere on OpenRouter.
                c.served_by.clear();
            }
            c.provider = Some(p);
            (None, (MarkKind::Model, m))
        }
    }
}

/// How long the cache of `c`'s next request (`messages` long) should live:
/// the setting's, or with it on auto the conversation's own pick
/// (`cache_ttl`). Only Claude has the choice.
fn cache_choice(c: &Conversation, provider: api::Provider, messages: usize, now: i64) -> crate::cache_ttl::Choice {
    let five = crate::cache_ttl::Choice { ttl: oak::Ttl::FiveMinutes, anchor: None };
    if provider != api::Provider::Claude {
        return five;
    }
    if let Some(ttl) = api::cache_fixed() {
        return crate::cache_ttl::Choice { ttl, anchor: None };
    }
    // Kept warm while idle, five minutes beats an hour (replayed: the
    // hour's double write price buys nothing pings don't).
    if api::keep_alive() {
        return five;
    }
    // The context rolls over at the trigger, or else fills the window.
    let window = api::context_window(&c.model).unwrap_or(200_000);
    let limit = if c.rolling.off { window } else { rolling::config_for(&c.model).trigger.min(window) };
    let choice = c.cache.choose(now, c.rolling.context, c.rolling.base(), limit);
    // An anchor past the messages (they changed) is no anchor.
    crate::cache_ttl::Choice { anchor: choice.anchor.filter(|&i| i + 1 < messages), ..choice }
}

/// The transcript as `provider` can take it: reasoning another provider
/// wrote left out, as its signature means nothing to this one (Claude
/// refuses a thinking block it didn't sign; Codex's reasoning rides in the
/// signature). Within a provider it stays: Claude models read each other's,
/// as Codex models do. A reply left with nothing says so in a line, so the
/// turn still has content. Borrowed as it is when nothing is left out.
fn for_provider(messages: &[Value], provider: api::Provider) -> std::borrow::Cow<'_, [Value]> {
    let foreign = |b: &Value| oak::is_reasoning(b) && b["by"].as_str().is_some_and(|by| by != provider.id());
    let has = |m: &Value| m["role"] == "assistant" && m["content"].as_array().is_some_and(|c| c.iter().any(foreign));
    if !messages.iter().any(has) {
        return std::borrow::Cow::Borrowed(messages);
    }
    let mut out = messages.to_vec();
    for m in out.iter_mut().filter(|m| has(m)) {
        let content = m["content"].as_array_mut().unwrap();
        content.retain(|b| !foreign(b));
        if content.is_empty() {
            content.push(oak::text("(No reply text.)"));
        }
    }
    std::borrow::Cow::Owned(out)
}

/// The model ended its turn: with the goal finished, the goal ends (its
/// line holds the justification); with it not finished, returns the
/// message asking the model to carry on (and marks it), unless the user
/// interrupted or paused the goal (then nothing is sent or marked: the
/// model never learns of the pause).
fn after_turn(c: &mut Conversation, interrupted: bool, now: i64) -> Option<Value> {
    if c.goal.is_empty() {
        return None;
    }
    if c.goal_finished {
        let text = std::mem::take(&mut c.goal_justification);
        add_mark(c, MarkKind::GoalFinished, text, String::new(), now);
        c.goal.clear();
        c.goal_finished = false;
        end_ask_request(c);
        return None;
    }
    if interrupted || c.goal_paused {
        return None;
    }
    let text = goal_continue(&c.goal);
    add_mark(c, MarkKind::GoalContinued, String::new(), text.clone(), now);
    Some(goal_block(&text))
}

/// Heartbeats that are due, as messages for the model; each is next due one
/// interval from now (beats missed while busy are skipped, not queued).
fn due_beats(c: &mut Conversation, now: i64) -> Vec<String> {
    let mut out = vec![];
    for h in c.heartbeats.iter_mut().filter(|h| h.next_at <= now) {
        out.push(format!(
            "{BEAT_TAG}Heartbeat {} (every {}): {}\nStop it with HEARTBEAT_STOP id {} when it is no longer needed.</heartbeat>",
            h.id,
            every(h.every_min),
            h.message,
            h.id
        ));
        h.next_at = now + h.every_min as i64 * 60_000;
    }
    if !out.is_empty() {
        c.save();
    }
    out
}

pub fn every(min: u64) -> String {
    match min {
        1 => "minute".into(),
        m if m % 60 == 0 && m >= 120 => format!("{} hours", m / 60),
        60 => "hour".into(),
        m => format!("{m} minutes"),
    }
}

/// The transcript line for instruction files loaded, with paths in the
/// working folder relative to it and the home folder as ~.
fn loaded_line(paths: &[String], skipped: &[PathBuf], folder: &Path) -> String {
    let home = instructions::home();
    let short = |p: &String| {
        let path = Path::new(p);
        match (path.strip_prefix(folder), path.strip_prefix(&home)) {
            (Ok(rel), _) => rel.display().to_string(),
            (_, Ok(rel)) => format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display()),
            _ => p.clone(),
        }
    };
    let list: Vec<String> = paths.iter().map(short).collect();
    let mut s = if paths.is_empty() { "no instruction files any more".to_string() } else { format!("instructions loaded: {}", list.join(", ")) };
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(|p| p.display().to_string()).collect();
        s += &format!(" (left out imports outside the folder: {})", list.join(", "));
    }
    s
}

/// The first request of a session (and one after a switch of provider,
/// `at` the message it comes before): lines at the top of the transcript for
/// what the model is given besides the conversation, each opening to it
/// word for word, as the model's provider sends it. For Claude models, the
/// billing header and identity that come before the system prompt; then the
/// instructions (the role's system prompt, plus the instruction files' line that
/// `memory_for_request` just put in at `from`, folded into this one); then
/// the tool definitions.
fn start_marks(c: &mut Conversation, from: usize, p: &api::Preamble, context: &str, at: usize) {
    let files = c.marks.drain(from.min(c.marks.len())..).find(|m| m.kind == MarkKind::Instructions);
    let now = api::now_ms();
    let mark = |kind, text: String, sent: String| Mark { sent, ..Mark::new(at, kind, text, now) };
    if let Some([billing, identity]) = &p.identity {
        let sent = store::sent_parts(&[("billing header", billing), ("identity", identity)]);
        c.marks.push(mark(MarkKind::Identity, "Claude Code identity and billing header".into(), sent));
    }
    let listed = files.as_ref().and_then(|m| m.text.strip_prefix("instructions loaded: ")).map(|rest| format!(", {rest}")).unwrap_or_default();
    let mut parts = vec![("system prompt", p.system.as_str())];
    if !context.is_empty() {
        parts.push(("start of the first message", context));
    }
    // How much of the context each takes, roughly.
    let tokens = |chars: usize| format!("{} tokens", crate::rolling::fmt_tokens(crate::rolling::estimate(chars)));
    let instructions = tokens(parts.iter().map(|(_, t)| t.len()).sum());
    let label = if crate::freebuff::serves(&c.model) {
        format!("instructions loaded: Freebuff system prompt{listed} ({instructions})")
    } else {
        format!("instructions loaded: {} system prompt{listed} ({instructions})", prompt::role(&c.role).name)
    };
    c.marks.push(mark(MarkKind::Instructions, label, store::sent_parts(&parts)));
    let tool_chars = p.tools.iter().map(|t| t.to_string().len()).sum();
    c.marks.push(mark(MarkKind::Tools, format!("tools loaded: {} tools ({})", p.tools.len(), tokens(tool_chars)), tools_sent(p)));
}

/// The tools line's text when opened: each definition, under its name.
fn tools_sent(p: &api::Preamble) -> String {
    let list: Vec<(String, String)> = p
        .tools
        .iter()
        .map(|t| (t["name"].as_str().or(t["function"]["name"].as_str()).unwrap_or("").to_string(), serde_json::to_string_pretty(t).unwrap_or_default()))
        .collect();
    let parts: Vec<(&str, &str)> = list.iter().map(|(n, d)| (n.as_str(), d.as_str())).collect();
    store::sent_parts(&parts)
}

/// Whether the transcript's last tools and identity lines no longer say
/// what goes with a request (`p`): the model moved to another provider
/// (identity there or not, another tool set). Only the tool names count, not
/// their wording: descriptions change mid-session (the AGENT tool's model
/// list follows the providers that are connected), which is no switch.
fn preamble_changed(c: &Conversation, p: &api::Preamble) -> bool {
    let last = |k| c.marks.iter().rev().find(|m| m.kind == k);
    let Some(t) = last(MarkKind::Tools) else { return false };
    let identity = last(MarkKind::Identity).is_some_and(|m| m.at == t.at && m.time == t.time);
    let names = |sent: &str| store::split_sent(sent).into_iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>();
    identity != p.identity.is_some() || names(&t.sent) != names(&tools_sent(p))
}

/// The messages to send with the context block (instruction files and
/// memory index) at the start of the first one. It goes there, not in the
/// system prompt, as Claude Code sends it; the first message is the
/// summary once there is one, so the block outlives summaries.
fn with_context(mut messages: Vec<Value>, context: &str) -> Vec<Value> {
    if context.is_empty() {
        return messages;
    }
    if let Some(first) = messages.first_mut().filter(|m| m["role"] == "user") {
        let block = oak::text(context);
        match first["content"].as_array_mut() {
            Some(a) => a.insert(0, block),
            None => {
                let text = first["content"].as_str().unwrap_or("").to_string();
                first["content"] = json!([block, oak::text(&text)]);
            }
        }
    }
    messages
}

/// The context block (instruction files and memory index) requests carry.
fn memory_context(c: &Conversation) -> &str {
    c.memory.as_ref().map_or("", |m| m.context.as_str())
}

/// `messages` (the start of the transcript) as a request sends them: in
/// the rolling view, fit for the model, with the context block.
fn request_messages(c: &Conversation, messages: &[Value]) -> Vec<Value> {
    with_context(c.rolling.view(&for_provider(messages, c.provider())), memory_context(c))
}

/// What to summarize of the first `n` messages of a request built on the
/// summary cut `base`, whose prompt came to `total` tokens.
fn plan_summary(c: &Conversation, n: usize, base: usize, total: u64) -> Option<Plan> {
    let cleared = c.rolling.cleared_to();
    let keep = rolling::config_for(&c.model).keep;
    rolling::plan(&c.messages[..n.min(c.messages.len())], base, cleared, total, &c.rolling.measured, keep, true)
}

/// Writes a summary (`rolling::summarize`), with its benchmark events.
fn summarize(conv: &store::Shared, request: oak::Request, blocking: bool) -> Result<(String, rolling::Cost), String> {
    bench(conv, "summary_start", json!({"blocking": blocking}));
    let t = Instant::now();
    let result = rolling::summarize(request);
    bench(conv, "summary_end", json!({"ok": result.is_ok(), "ms": ms_since(t), "blocking": blocking}));
    result
}

/// Tokens of a request that no summary can remove: system prompt, tools
/// and the context block.
fn overhead(req: &oak::Request) -> u64 {
    let first = req.messages.first().map(|m| &m["content"][0]["text"]);
    let context = first.and_then(Value::as_str).filter(|t| t.starts_with("<system-reminder>\nAs you answer")).map_or(0, str::len);
    let p = api::preamble(&req.model, &req.system, &Value::Array(req.tools.clone()));
    let identity = p.identity.map_or(0, |[b, i]| b.len() + i.len());
    rolling::estimate(identity + p.system.len() + Value::Array(p.tools).to_string().len() + context + 300)
}

/// Installs a finished summary, unless the conversation moved on to another
/// one meanwhile (or was reverted: `reverts` is the count it began at), and
/// records a failure so it isn't retried at once.
fn finish_summary(conv: &store::Shared, live: &Mutex<Live>, base: usize, plan: Plan, overhead: u64, reverts: u64, result: Result<(String, rolling::Cost), String>) {
    match result {
        Ok((text, cost)) => {
            let mut c = conv.lock().unwrap();
            c.spent += cost.usd; // paid whether or not the summary is used
            let reverted = live.lock().unwrap().reverts != reverts;
            if c.rolling.base() == base && !reverted {
                // Taken out: everything before the cut except system and tools; put in: the summary.
                let added = rolling::estimate(text.len() + 400);
                let saved = plan.before.saturating_sub(overhead + added) + plan.cleared;
                let made_at = api::now_ms();
                // The transcript only grows: the line goes where it stands
                // now, and points back at the cut.
                add_mark(&mut c, MarkKind::Summarized, text.clone(), String::new(), made_at).done = plan.cut as i64;
                c.rolling.apply(Summary { cut: plan.cut, text, made_at, clear: plan.clear }, saved, cost);
                c.save();
            }
            live.lock().unwrap().rolling_failed = None;
        }
        Err(e) => {
            // A refused summary is tried again after a minute and a half
            // (its quick retries are done), anything else after five.
            let wait = Duration::from_secs(if e.contains("stop reason: refusal") { api::retry_wait(api::RETRIES + 1) } else { 300 });
            let mut c = conv.lock().unwrap();
            c.rolling.last_error = Some(e);
            c.save();
            live.lock().unwrap().rolling_failed = Some((Instant::now(), c.rolling.context, wait));
        }
    }
    live.lock().unwrap().compacting = false;
}

/// Makes a streamed reply fit to send back: empty text blocks are never valid.
fn finish_blocks(blocks: Vec<Value>) -> Vec<Value> {
    blocks.into_iter().filter(|b| !(b.is_null() || (b["type"] == "text" && b["text"].as_str().unwrap_or("").is_empty()))).collect()
}

/// Puts a reply's output speed (tokens per second, from the provider's
/// count) on its text blocks (`_oak_tps`), for the message bar to show.
fn mark_speed(blocks: &mut [Value], tps: f64) {
    for b in blocks.iter_mut().filter(|b| b["type"] == "text") {
        b["_oak_tps"] = json!((tps * 10.0).round() / 10.0);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live;

#[cfg(test)]
mod request_bench;
