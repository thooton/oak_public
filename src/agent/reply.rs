//! Building requests, keeping their caches warm, processing streamed replies
//! and scheduling context summaries. The turn loop lives in the parent module.

use super::memory::memory_for_request;
use super::prompt::system_prompt_on;
use super::*;
use crate::api::Sse;
use crate::oak::Event;

/// How long after the user's last request keep-alive goes on keeping the
/// cache warm (`Worker::keep_warm`): replaying OAK's conversations, 30
/// minutes saved the most.
const KEEP_ALIVE_FOR: i64 = 30 * 60_000;

/// A reply's events for the benchmark timeline: the usage as reported,
/// and whether the first event came in yet.
#[derive(Default)]
struct BenchReply {
    usage: oak::Usage,
    started: bool,
}

impl BenchReply {
    fn event(&mut self, w: &Worker, ev: &Event) {
        match ev {
            Event::Headers(status) => w.bench("response_headers", json!({"status": status})),
            Event::ThreadRestart(reason) => w.bench("thread_restart", json!({"reason": reason})),
            Event::Retry { .. } => {}
            ev => {
                if !std::mem::replace(&mut self.started, true) {
                    w.bench("first_event", json!({}));
                }
                if let Event::Usage(u) = ev {
                    self.usage.merge(u);
                }
                if matches!(ev, Event::Stop { .. }) {
                    w.bench("last_event", json!({}));
                }
            }
        }
    }

    fn done(self, w: &Worker, stop: &str) {
        let u = self.usage;
        w.bench("reply_processed", json!({
            "stop": stop, "input": u.input, "cache_write": u.write, "cache_read": u.cache_read, "output": u.output,
            "usage": {"ephemeral_5m_input_tokens": u.write_5m, "ephemeral_1h_input_tokens": u.write_1h, "reasoning": u.reasoning},
        }));
    }
}

/// Puts a reply's thinking-token count on its thinking blocks (`_oak_tokens`).
/// The API gives one count per reply; when the reply thought more than once
/// (say, around a server-side tool), the count is shared out by how long
/// each block's visible thinking is, so the parts still add up to it. A
/// redacted block has no visible thinking (its `data` is encrypted, far
/// longer than any summary), so it weighs as much as the visible ones do on
/// average. A share of 0 is left off: the row then shows how long it
/// thought instead.
pub(super) fn spread_thinking_tokens(blocks: &mut [Value], n: u64) {
    let mut thinking: Vec<&mut Value> =
        blocks.iter_mut().filter(|b| oak::is_reasoning(b)).collect();
    if n == 0 || thinking.is_empty() {
        return;
    }
    fn shown(b: &Value) -> Option<&str> {
        b["text"].as_str().filter(|_| b["redacted"] != true)
    }
    let visible: Vec<u64> = thinking
        .iter()
        .filter_map(|b| shown(b))
        .map(|t| t.len().max(1) as u64)
        .collect();
    let average = (visible.iter().sum::<u64>() / visible.len().max(1) as u64).max(1);
    let weight = |b: &Value| shown(b).map_or(average, |t| t.len().max(1) as u64);
    let total: u64 = thinking.iter().map(|b| weight(b)).sum();
    let last = thinking.len() - 1;
    let mut given = 0;
    for (k, b) in thinking.iter_mut().enumerate() {
        let share = if k == last {
            n - given
        } else {
            n * weight(b) / total
        };
        given += share;
        if share > 0 {
            b["_oak_tokens"] = json!(share);
        }
    }
}

/// What a reply being streamed in keeps between its events.
#[derive(Default)]
struct Streaming {
    /// The server-side thread the reply continues (`api::threads`).
    thread: Option<api::Thread>,
    stop: String,
    usage: Usage,
    /// When each block began.
    started: Vec<Instant>,
    rewrite_thinking: bool,
    /// Thinking sections waiting to go to be rewritten (Unslop).
    rewrites: Vec<(String, String)>,
    /// The block the model is writing, and what the status line says of
    /// it ("Thinking", "Writing READ").
    writing: Option<(usize, String)>,
    /// The provider runs the calls it makes as it goes (Quick): they come
    /// whole, and are running, not being written, until their result.
    provider_runs: bool,
    /// The provider's calls running now (their blocks), until their result.
    running: Vec<usize>,
    /// The batch's `generation` the status line was last brought up to.
    seen: u64,
}

impl Streaming {
    /// Puts one streamed event into the reply's blocks (`Live::blocks`);
    /// a tool call that is complete goes to `batch`.
    fn apply(&mut self, l: &mut Live, batch: &mut Batch, ev: Event) {
        match ev {
            Event::Retry {
                attempt,
                wait,
                reason,
            } => {
                // (Past RETRIES: OpenRouter waiting out a rate limit.)
                l.status = if attempt <= api::RETRIES {
                    format!(
                        "Retry {attempt}/{} in {wait}s after: {reason}",
                        api::RETRIES
                    )
                } else {
                    format!("Retry {attempt} in {wait}s after: {reason}")
                };
            }
            Event::Block { index: i, block: b } => {
                let new = if self.provider_runs && oak::is_call(&b) {
                    // The model has moved on from what it wrote before.
                    self.writing = None;
                    if b.get("_oak_result").is_none() && !self.running.contains(&i) {
                        self.running.push(i);
                    }
                    false
                } else {
                    let what = match b["type"].as_str() {
                        Some("reasoning") => "Thinking".into(),
                        Some("tool_call") => format!("Writing {}", crate::transcript::shown(&b)),
                        _ => "Writing".into(),
                    };
                    let new = self.writing.as_ref().is_none_or(|(w, _)| *w != i);
                    self.writing = Some((i, what));
                    new
                };
                if l.blocks.len() <= i {
                    l.blocks.resize(i + 1, Value::Null);
                }
                l.blocks[i] = b;
                self.started
                    .resize(self.started.len().max(i + 1), Instant::now());
                self.started[i] = Instant::now();
                self.refresh(l, batch, new);
            }
            Event::Text { index, text } => {
                if let Some(b) = l.blocks.get_mut(index) {
                    append(b, "text", &text);
                }
            }
            Event::Reasoning { index, text } => {
                if let Some(b) = l.blocks.get_mut(index) {
                    append(b, "text", &text);
                    if self.rewrite_thinking {
                        self.rewrites
                            .extend(crate::rewrites::mark_thinking(b, false));
                    }
                }
            }
            Event::Sig { index, sig } => {
                if let Some(b) = l.blocks.get_mut(index) {
                    b["sig"] = json!(sig);
                }
            }
            Event::Input { index, json: d } => {
                // The input so far, for the UI to show a WRITE or EDIT as
                // it comes in (taken out when it's done).
                let last = index + 1 == l.blocks.len();
                let mut named = None;
                if let Some(b) = l.blocks.get_mut(index) {
                    match b.get_mut("_oak_partial") {
                        Some(Value::String(s)) => s.push_str(&d),
                        _ => b["_oak_partial"] = json!(d),
                    }
                    // A kit's tool through LOAD is named as it once its
                    // input says which.
                    if last && b["name"] == "LOAD" {
                        named = Some(format!("Writing {}", crate::transcript::shown(b)));
                    }
                }
                if let Some(n) = named
                    && let Some((w, what)) = self.writing.as_mut()
                    && *w == index
                {
                    *what = n;
                    self.refresh(l, batch, false);
                }
            }
            Event::BlockEnd { index: i } => {
                if self.writing.as_ref().is_some_and(|(w, _)| *w == i) {
                    self.writing = None;
                }
                self.running.retain(|&r| r != i);
                self.end_block(l, batch, i);
                self.refresh(l, batch, false);
            }
            Event::Start { id, .. } => {
                if let Some(t) = self.thread.as_mut().filter(|_| !id.is_empty()) {
                    t.id = id;
                }
            }
            Event::Usage(u) => {
                self.usage.read(&u);
                // How many of the output tokens were thinking, for
                // "Thought for N tokens": for the whole reply.
                if let Some(n) = u.reasoning {
                    spread_thinking_tokens(&mut l.blocks, n);
                }
            }
            // A refusal carries the classifier's category: kept after a
            // newline, for the error.
            Event::Stop { reason, detail } => {
                self.stop = match detail {
                    Some(d) => format!("{reason}\n{d}"),
                    None => reason,
                };
            }
            _ => {}
        }
    }

    /// Block `i` ended.
    fn end_block(&mut self, l: &mut Live, batch: &mut Batch, i: usize) {
        let Some(b) = l.blocks.get_mut(i) else { return };
        // How long the model thought, for "Thought for Ns". OAK's
        // own keys are taken out before requests (rolling::view).
        if oak::is_reasoning(b)
            && let Some(t) = self.started.get(i)
        {
            b["_oak_ms"] = json!(t.elapsed().as_millis() as u64);
            if self.rewrite_thinking {
                self.rewrites
                    .extend(crate::rewrites::mark_thinking(b, true));
            }
        }
        if oak::is_call(b) {
            // A call its provider already ran (Quick asks Oak to run
            // its tools as it goes) comes with its result: that goes
            // in the transcript, and the call isn't run again.
            let ran = b.as_object_mut().and_then(|o| o.remove("_oak_result"));
            match ran {
                Some(result) => {
                    // `l` is locked already: the result is held with
                    // the reply's early results, which go in the
                    // transcript after it (delivering it here would
                    // lock `live` again, and hang).
                    let call = b.clone();
                    if let Some(held) = l.early.as_mut() {
                        held.push(result.clone());
                    }
                    batch.provided(call, result);
                }
                None => {
                    finish_call(b);
                    batch.add(b.clone());
                }
            }
        }
    }

    /// Brings the status line up to what the reply is doing: the model
    /// writing a block ("Thinking", "Writing EDIT"), else the calls running
    /// (the provider's, and those that started while the reply streams
    /// in), else, once those are done, waiting for the model. `force`: a
    /// block began, so it is said even if something else set the status
    /// line since.
    fn refresh(&mut self, l: &mut Live, batch: &mut Batch, force: bool) {
        self.seen = batch.generation();
        let status = match &self.writing {
            Some((_, what)) => what.clone(),
            None => {
                let named = |quiet: bool| -> Vec<String> {
                    self.running
                        .iter()
                        .filter_map(|&i| l.blocks.get(i))
                        .filter(|b| b.get(crate::quick::QUIET_KEY).is_some() == quiet)
                        .map(crate::transcript::shown)
                        .collect()
                };
                // A snippet's calls are named, not the snippet (it shows
                // only when it has none running).
                let mut names = named(false);
                names.extend(batch.running_names());
                if names.is_empty() {
                    names = named(true);
                }
                if !names.is_empty() {
                    format!("Running {}", names.join(", "))
                } else if batch.shown().starts_with("Running ") {
                    "Waiting for the model".into()
                } else {
                    return;
                }
            }
        };
        batch.show(l, status, force);
    }

    /// Calls that started or ended since the status line was last brought
    /// up to date show on it.
    fn calls_changed(&mut self, w: &Worker, batch: &mut Batch) {
        if batch.generation() != self.seen {
            w.set(|l| self.refresh(l, batch, false));
        }
    }
}

/// A streamed tool call is complete: its input, streamed as text, is
/// parsed (and repaired); input that doesn't parse is kept raw.
fn finish_call(b: &mut Value) {
    let partial = b.as_object_mut().and_then(|o| o.remove("_oak_partial"));
    let raw = partial
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw }) {
        Ok(v) if v.is_object() => {
            let name = b["name"].as_str().unwrap_or("").to_string();
            b["input"] = tools::repair(&name, v);
        }
        _ => {
            b["input"] = json!({});
            b["_oak_raw"] = json!(raw);
        }
    }
}

impl Worker {
    /// When the idle conversation's cache is next to be kept warm (half a
    /// minute before its five-minute entry would expire), if it is: the
    /// setting on, not paused, the last request (`warm`) the one cached, and
    /// the user's last request less than `KEEP_ALIVE_FOR` ago.
    pub(super) fn keep_alive_due(&self) -> Option<i64> {
        if !api::keep_alive() || self.paused.load(Ordering::SeqCst) {
            return None;
        }
        let (messages, dead) = self
            .warm
            .lock()
            .unwrap()
            .as_ref()
            .map(|(req, dead)| (req.messages.len(), *dead))?;
        let c = self.conv.lock().unwrap();
        let last = c.cache.last?;
        let now = api::now_ms();
        let due = c.cache.expires_at()? - 30_000;
        let fits =
            !dead && !c.cache.cold && last.base == c.rolling.base() && last.messages == messages;
        (fits && now < due + 25_000 && due - c.cache.asked < KEEP_ALIVE_FOR).then_some(due)
    }

    /// Reads the cache again with a request that generates nothing
    /// (`api::keep_warm`), so it lives five more minutes. A ping that wrote
    /// most of the context instead (the cache was gone) or failed ends
    /// keep-alive for this request.
    pub(super) fn keep_warm(&self) {
        let Some(req) = self.warm.lock().unwrap().as_ref().map(|w| w.0.clone()) else {
            return;
        };
        let at = api::now_ms();
        let result = api::keep_warm(&req);
        let mut c = self.conv.lock().unwrap();
        let alive = match &result {
            Ok(u) => u.write <= (u.input + u.cache_read + u.write) / 2,
            Err(e) => {
                eprintln!("Keep-alive failed: {e}");
                false
            }
        };
        if alive {
            c.cache.warm(at, req.anchor.is_some());
            // The session memory goes on as it was: it is in the cache.
            if let Some(m) = c.memory.as_mut() {
                m.used_at = m.used_at.max(at);
            }
            c.save();
        } else if let Some(w) = self.warm.lock().unwrap().as_mut() {
            w.1 = true;
        }
    }

    /// One round's request. The provider adds what it sends besides (for
    /// Claude: the Claude Code identity, billing header and cache
    /// breakpoints, sent as the official client does). Returns the request
    /// and the transcript length and summary cut it used.
    pub(super) fn request_body(&self) -> (oak::Request, usize, usize) {
        let mut c = self.conv.lock().unwrap();
        let now = api::now_ms();
        let start = c.memory.is_none();
        let marks_before = c.marks.len();
        let provider = c.provider();
        // What was sent stays as it was (`crate::prefix`): the session memory
        // is read again only with a summary, like the rest.
        let memory = memory_for_request(&mut c, now);
        // A chat has no folder, so no scratchpad either.
        let scratch = if c.chat {
            PathBuf::new()
        } else {
            scratchpad(&mut c)
        };
        let context = memory_context(&c).to_string();
        let agent = c
            .parent
            .is_some()
            .then(|| super::swarm::self_label(&c));
        // A model on the user's own server (or a plugin's) gets the basics
        // only, and the goal's tools while there is one.
        let local = crate::local::basic(&c.model);
        let mut kits = c.prefix.kits.clone();
        if local && !c.goal.is_empty() {
            kits.push("GOAL".into());
        }
        // (A model without effort levels goes without.)
        let effort = Some(effort_for(&c.model, &c.effort).to_string()).filter(|e| !e.is_empty());
        let (n, cut) = (c.messages.len(), c.rolling.base());
        // What the request sends is read (from the summary's cut on).
        c.messages.load_from(cut);
        // A kit whose LOAD the view shows stays out of the system prompt and
        // the prefix's tools (its LOAD's result has its instructions, its
        // tools join after it); one a summary covers (or loaded with no
        // LOAD) is in them.
        let loads = if c.chat {
            vec![]
        } else {
            crate::prefix::loads_of(&c.messages)
        };
        let shown = |k: &String| loads.iter().find(|l| &l.kit == k).filter(|l| l.call >= cut);
        // Code Mode's one tool is REPL; the others are functions in it.
        let (code_mode, spawner) = (c.code_mode && !c.chat, c.spawner);
        let inner =
            |kits: &[String]| tools::definitions_with_kits(agent.is_some(), spawner, local, kits);
        let is_chat = c.chat;
        // A Mix has SWITCH too (`crate::mix`).
        let mix = c.mix.is_some() && !c.chat && !code_mode;
        let defs = |kits: &[String]| {
            if is_chat {
                tools::chat_definitions()
            } else if code_mode {
                vec![crate::code_mode::tool_definition()]
            } else {
                let mut defs = inner(kits);
                if mix {
                    defs.push(crate::mix::tool_definition());
                }
                defs
            }
        };
        let held: Vec<String> = kits
            .iter()
            .filter(|k| shown(k).is_none())
            .cloned()
            .collect();
        let held_tools = defs(&held);
        let mut joins = vec![];
        for l in kits.iter().filter_map(shown) {
            let with = defs(&[held.clone(), vec![l.kit.clone()]].concat());
            for t in with
                .iter()
                .filter(|t| !held_tools.iter().any(|h| h["name"] == t["name"]))
            {
                let tool = t["name"].as_str().unwrap_or("").to_string();
                joins.push(crate::prefix::Join {
                    tool,
                    kit: l.kit.clone(),
                    at: l.join,
                    through: l.through,
                });
            }
        }
        let model = c.model.clone();
        let tools = defs(&kits);
        // (A session with no NOTE tool, a local model's, has no notes limit to tell.)
        let has_notes = |tools: &[Value]| {
            tools
                .iter()
                .any(|t| t["name"].as_str().is_some_and(|n| n.starts_with("NOTE")))
        };
        let notes_limit = if has_notes(&tools) || (code_mode && has_notes(&inner(&kits))) {
            super::notes_limit()
        } else {
            0
        };
        let built = crate::prefix::Pin {
            day: crate::time::today_utc(),
            kits: kits.clone(),
            effort,
            tools,
            folder: c.folder.display().to_string(),
            model,
            system: String::new(),
            notes_limit,
        };
        let base = crate::prefix::Pin {
            kits: held,
            tools: held_tools,
            ..built.clone()
        };
        // The prefix as first sent since the last summary: its system prompt,
        // tools and effort, and messages for what changed since
        // (`crate::prefix`).
        let mode = crate::prefix::mode(provider, &c.model);
        let (chat, role, model_now) = (c.chat, c.role.clone(), c.model.clone());
        let render = |pin: &crate::prefix::Pin| {
            // (The folder and model the prompt was first sent with: a CD's
            // result and a message say the new ones.)
            let model = if pin.model.is_empty() {
                &model_now
            } else {
                &pin.model
            };
            if chat {
                super::prompt::chat_prompt(&pin.day, model, pin.notes_limit)
            } else if code_mode {
                let base = system_prompt_on(
                    &pin.day,
                    Path::new(&pin.folder),
                    &scratch,
                    model,
                    agent.as_deref(),
                    &pin.kits,
                    &role,
                    pin.notes_limit,
                );
                format!(
                    "{}\n\n{}",
                    base.trim_end(),
                    crate::code_mode::prompt(&inner(&pin.kits))
                )
            } else {
                let base = system_prompt_on(
                    &pin.day,
                    Path::new(&pin.folder),
                    &scratch,
                    model,
                    agent.as_deref(),
                    &pin.kits,
                    &role,
                    pin.notes_limit,
                );
                if mix {
                    format!("{}\n\n{}", base.trim_end(), crate::mix::prompt().trim_end())
                } else {
                    base
                }
            }
        };
        let (pin, system) = match mode {
            Some(mode) => {
                let add = last_is(&c, "user");
                let notes = super::notes_tokens(&c.notes);
                let effort_per_message =
                    mode == crate::prefix::Mode::Claude && crate::prefix::supports_effort(&c.model);
                let pin = c.prefix.prepare(crate::prefix::Setup {
                    mode,
                    n,
                    cut,
                    add,
                    effort_per_message,
                    now: built,
                    base,
                    joins,
                    notes,
                });
                let system = c.prefix.system(render);
                (pin, system)
            }
            // Freebuff sends its own prompt and tools: these go to the
            // sidecar only for what it adds of its own.
            None => {
                let pin = crate::prefix::Pin {
                    kits: base.kits,
                    ..built
                };
                let system = render(&pin);
                (pin, system)
            }
        };
        let tools = Value::Array(pin.tools);
        // Again after a switch to a model whose provider sends other things
        // (another identity, prompt or tools), where it takes effect.
        let p = api::preamble(&c.model, &system, &tools);
        if start || preamble_changed(&c, &p) {
            let at = if start { 0 } else { c.messages.len() };
            start_marks(&mut c, marks_before, &p, &context, at);
        }
        if provider == api::Provider::Quick {
            super::open_compacted_thread(&mut c);
        }
        let mut messages = request_messages(&c, &c.messages);
        // Quick keeps the conversation itself: what other models wrote
        // since it last replied goes with the request, so a switch to it
        // carries the conversation over.
        let unseen = if provider == api::Provider::Quick {
            unseen_since(&c.messages, &messages, provider, cut, super::quick_opened(&c))
        } else {
            // A Quick turn parked after a steer would go on generating on
            // Quick's server (its calls running) while another model works.
            crate::quick::turn::drop_parked(&c.id, false);
            vec![]
        };
        if let Some(mode) = mode {
            c.prefix.fold(&mut messages, cut, n);
            messages = c.prefix.inject(mode, messages, cut, n);
        }
        let cache = cache_choice(&c, provider, messages.len(), now);
        let req = oak::Request {
            provider,
            model: c.model.clone(),
            max_tokens: api::max_output(&c.model),
            system: system + &memory,
            messages,
            tools: tools.as_array().cloned().unwrap_or_default(),
            thinking: true,
            effort: pin.effort,
            // Through OpenRouter: the provider the last reply came from,
            // which the next request stays with while it is a good pick (its
            // cache).
            route: (provider == api::Provider::OpenRouter && !c.served_by.is_empty())
                .then(|| c.served_by.clone()),
            // The Freebuff sidecar signs and snapshots the project the same
            // way the CLI does; it needs the folder.
            folder: (!c.chat).then(|| c.folder.to_string_lossy().into_owned()),
            conv: Some(c.id.clone()),
            ttl: cache.ttl,
            anchor: cache.anchor,
            unseen,
            quick_session: c.quick_session.clone().filter(|_| provider == api::Provider::Quick),
            // Quick's cut conversations and Freebuff's briefing go there.
            scratch: matches!(provider, api::Provider::Quick | api::Provider::Freebuff)
                .then(|| super::prompt::scratchpad(&mut c).to_string_lossy().into_owned()),
            ..Default::default()
        };
        (req, c.messages.len(), c.rolling.base())
    }

    /// What the request's user turn went to Quick as, once it is over
    /// (`agent::quick_request_over`): kept on its message for a revert to
    /// edit when Quick replied to it, and an edit Quick didn't reply to
    /// given up for a fresh thread.
    fn note_quick_sent(&self, reply: &Reply) {
        let n = match self.last_request.lock().unwrap().as_ref() {
            Some((body, n, ..)) if body.provider == api::Provider::Quick => *n,
            _ => return,
        };
        // Quick answered: some of a reply came, or it ended as replies do,
        // not on one of Quick's own stop reasons (`quick::recovery`).
        let replied = match reply {
            Reply::Done(blocks, stop, _) => !blocks.is_empty() || !stop.starts_with("quick_"),
            _ => false,
        };
        let mut c = self.conv.lock().unwrap();
        let sent = crate::quick::turn::take_sent(&c.id);
        if !sent.is_empty() && super::quick_request_over(&mut c, n, sent, replied) {
            c.save();
        }
    }

    /// One request to the model, its reply streamed into `Live::blocks`. A pause the user asked for meanwhile
    /// (`Session::request_pause`) starts once it is over, however it ended:
    /// the reply is kept, its tool calls run, and the next request waits.
    pub(super) fn ask(&self, batch: &mut Batch) -> Reply {
        self.set(|l| l.requesting = true);
        let mut reply = self.ask_inner(batch);
        self.end_request();
        self.note_quick_sent(&reply);
        self.settle_steers();
        // Which provider wrote the reply (`BY_KEY`), on each block.
        let by = self
            .last_request
            .lock()
            .unwrap()
            .as_ref()
            .map(|(body, ..)| body.provider);
        // Cut short (or refused) once tool calls had started: the reply is
        // kept up to the last of them, which go on, and the turn carries on
        // from their results.
        let cut = match &reply {
            Reply::Done(_, stop, _) => stop.starts_with("refusal"),
            _ => true,
        };
        if cut && batch.any_started() {
            let blocks = match reply {
                Reply::Done(b, ..) => b,
                _ => finish_blocks(std::mem::take(&mut self.live.lock().unwrap().blocks)),
            };
            let mut blocks = batch.kept(blocks);
            mark_by(&mut blocks, by);
            return Reply::Done(blocks, tool_calls::CUT_SHORT.into(), Usage::default());
        }
        if let Reply::Done(blocks, ..) = &mut reply {
            mark_by(blocks, by);
        }
        if matches!(reply, Reply::Paused | Reply::Interrupted) {
            self.set(|l| l.blocks.clear());
        }
        reply
    }

    /// The request is over: a pause queued meanwhile starts now.
    pub(super) fn end_request(&self) {
        let queued = {
            let mut l = self.live.lock().unwrap();
            l.requesting = false;
            std::mem::take(&mut l.pause_queued)
        };
        if queued {
            self.paused.store(true, Ordering::SeqCst);
            let mut c = self.conv.lock().unwrap();
            c.paused = true;
            c.pause_queued = false;
            c.save();
        }
    }

    /// The thread of a Quick request's turn; None for any other provider.
    fn quick_thread(&self) -> Option<String> {
        let r = self.last_request.lock().unwrap();
        let (body, ..) = r.as_ref()?;
        if body.provider != api::Provider::Quick {
            return None;
        }
        body.conv.clone()
    }

    /// The thread of a Quick request's turn, which takes in the messages
    /// typed while it runs (`steer_quick`); None for any other provider, or
    /// with `OAK_QUICK_STEER=off`.
    fn steers_to(&self) -> Option<String> {
        if std::env::var("OAK_QUICK_STEER").as_deref() == Ok("off") {
            return None;
        }
        self.quick_thread()
    }

    /// Hands the messages queued meanwhile to the Quick turn on `thread`,
    /// as Quick's app does: its agent takes each in at its next step. They
    /// stay queued (shown as sent) till Quick says it took them in.
    fn steer_quick(&self, thread: &str) {
        // The answers to questions asked during a goal go to the turn too:
        // ASK didn't wait for them, so neither does Quick's turn.
        let answers: Vec<String> = self
            .conv
            .lock()
            .unwrap()
            .inbox
            .iter()
            .filter(|t| t.starts_with(super::ANSWERS_TAG))
            .cloned()
            .collect();
        let mut l = self.live.lock().unwrap();
        for q in l
            .queued
            .iter_mut()
            // (Not one with pictures: a mid-turn message is text alone, so
            // it goes with the next request, which uploads them with their
            // copies' notes, `images::with_copies`.)
            .filter(|q| !q.held && !q.steered && !q.text.trim().is_empty() && q.images.is_empty())
        {
            q.steered = true;
            crate::quick::turn::steer(thread, q.id, &q.text);
        }
        for text in answers {
            if l.steered_answers.iter().any(|(_, t, _)| *t == text) {
                continue;
            }
            let id = l.next_queued_id();
            crate::quick::turn::steer(thread, id, &text);
            l.steered_answers.push((id, text, false));
        }
    }

    /// The request is over: the messages Quick took in stay marked so (they
    /// join the transcript after the reply's results, and no request sends
    /// them again); the rest go with the next request as messages of their own.
    fn settle_steers(&self) {
        let id = self.conv.lock().unwrap().id.clone();
        let took = crate::quick::turn::take_ingested(&id);
        let mut l = self.live.lock().unwrap();
        for q in l.queued.iter_mut().filter(|q| q.steered) {
            q.steered = took.contains(&q.id);
        }
        // Answers Quick took in leave the inbox (they join the transcript
        // with the next request's input, `absorb_notices`); the rest stay
        // there for the next request.
        l.steered_answers.retain_mut(|(id, _, taken)| {
            *taken = *taken || took.contains(id);
            *taken
        });
        let taken: Vec<String> = l.steered_answers.iter().map(|(_, t, _)| t.clone()).collect();
        drop(l);
        if !taken.is_empty() {
            let mut c = self.conv.lock().unwrap();
            let before = c.inbox.len();
            c.inbox.retain(|t| !taken.contains(t));
            if c.inbox.len() != before {
                c.save();
            }
        }
    }

    fn ask_inner(&self, batch: &mut Batch) -> Reply {
        // Freebuff's own agent works on a project (its prompt, its tools,
        // the folder's snapshot): no chat goes there.
        if let Some(why) = {
            let c = self.conv.lock().unwrap();
            chat_refuses_provider(c.chat, c.provider())
                .or_else(|| code_mode_refuses_provider(c.code_mode, c.provider()))
                .or_else(|| mix_refuses_provider(c.mix.is_some(), c.provider()))
        } {
            return Reply::Failed(why);
        }
        let (rx, thread) = self.send_request();
        let quick = self.quick_thread();
        // The Oak calls of Quick's snippets come in this reply, for its batch
        // to run (`quick::turn::run_oak_alive`).
        let _oak_calls = quick.as_deref().map(crate::quick::turn::Runner::new);
        let steer = self.steers_to();
        self.set(|l| {
            l.blocks.clear();
            batch.show(l, "Waiting for the model".into(), true);
        });
        let mut s = Streaming {
            thread,
            provider_runs: quick.is_some(),
            // Rewrite thinking (Unslop): sections go to be rewritten as they
            // come in, the rest when their block ends.
            rewrite_thinking: crate::rewrites::applies(&self.conv.lock().unwrap())
                && crate::rewrites::thinking(),
            ..Default::default()
        };
        // Output speed, from the provider's output token count only.
        let mut rate = crate::token_rate::TokenRate::default();
        let mut bench = BenchReply::default();
        let mut quick_stop: Option<(crate::quick::turn::StopKind, Instant)> = None;
        loop {
            if let Some(t) = &quick {
                let queued = self.live.lock().unwrap().pause_queued;
                crate::quick::turn::set_pause(t, queued);
            }
            // Checked on every event too: a busy stream may never go quiet.
            // (The blocks are cleared by `ask`.)
            if self.paused.load(Ordering::SeqCst) {
                match &quick {
                    Some(t) => {
                        if quick_stop.is_none() {
                            crate::quick::turn::abort(t, crate::quick::turn::StopKind::Pause);
                            quick_stop =
                                Some((crate::quick::turn::StopKind::Pause, Instant::now()));
                        }
                    }
                    None => return Reply::Paused,
                }
            }
            if self.interrupted() {
                batch.stop();
                match &quick {
                    Some(t) => {
                        crate::quick::turn::abort(t, crate::quick::turn::StopKind::Interrupt);
                        if self.reverting() {
                            crate::quick::turn::wait_stopped(t);
                            return Reply::Interrupted;
                        }
                        quick_stop =
                            Some((crate::quick::turn::StopKind::Interrupt, Instant::now()));
                    }
                    None => return Reply::Interrupted,
                }
            }
            if let Some((_, since)) = quick_stop
                && since.elapsed() >= Duration::from_secs(3)
            {
                break;
            }
            if quick_stop.is_none()
                && let Some(t) = &steer
            {
                self.steer_quick(t);
            }
            // Tool calls that streamed in start as soon as they may.
            batch.pump(false, oak::stop::TOOL_CALLS);
            s.calls_changed(self, batch);
            let ev = match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Sse::Event(ev)) => ev,
                Ok(Sse::Done) => break,
                Ok(Sse::Err(_)) if quick_stop.is_some() => break,
                Ok(Sse::Err(e)) if quick.is_some() && crate::quick::on() => {
                    s.stop = format!("{}\n{e}", crate::quick::recovery::ERROR);
                    break;
                }
                Ok(Sse::Err(e)) => return Reply::Failed(e),
                Ok(Sse::Limit(at)) => return Reply::Limited(at),
                Ok(Sse::Rotated(label)) => {
                    // The new account has none of the conversation's cache.
                    self.conv.lock().unwrap().cache.cold();
                    self.note_rotation(label);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.freebuff_alerts();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) if quick_stop.is_some() => break,
                Err(RecvTimeoutError::Disconnected) if quick.is_some() => {
                    s.stop = format!("{}\nAWS Quick stream ended unexpectedly", crate::quick::recovery::ERROR);
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Reply::Failed("Stream ended unexpectedly".into());
                }
            };
            if crate::bench::on() {
                bench.event(self, &ev);
            }
            match &ev {
                // Through OpenRouter, what the reply cost goes on the session's spend.
                Event::Cost(usd) => self.conv.lock().unwrap().spent += usd,
                Event::ServedBy(p) => self.conv.lock().unwrap().served_by = p.clone(),
                _ => {}
            }
            rate.observe(&ev, Instant::now());
            let delta = matches!(
                ev,
                Event::Text { .. }
                    | Event::Reasoning { .. }
                    | Event::Input { .. }
                    | Event::Sig { .. }
            );
            let (drawn, viewport) = {
                let mut l = self.live.lock().unwrap();
                s.apply(&mut l, batch, ev);
                l.mark_phase();
                (
                    l.drawn,
                    l.drawn_viewport.unwrap_or(eframe::egui::ViewportId::ROOT),
                )
            };
            if !s.rewrites.is_empty() {
                crate::rewrites::start_parts(
                    std::mem::take(&mut s.rewrites),
                    &self.conv,
                    &self.ctx,
                );
            }
            // A delta only changes the transcript; any other event may
            // change what Home and the tabs show (the account's limits).
            if !delta || drawn + 1 >= self.ctx.cumulative_pass_nr_for(viewport) {
                self.ctx.request_repaint();
            }
        }
        if let Some(t) = &quick
            && quick_stop.is_some()
        {
            crate::quick::turn::wait_stopped(t);
        }
        let blocks = std::mem::take(&mut self.live.lock().unwrap().blocks);
        if blocks.is_empty() {
            if let Some((kind, _)) = quick_stop {
                return match kind {
                    crate::quick::turn::StopKind::Interrupt => Reply::Interrupted,
                    crate::quick::turn::StopKind::Pause => Reply::Paused,
                };
            }
            if quick.is_some()
                && (self.paused.load(Ordering::SeqCst) || self.live.lock().unwrap().pause_queued)
            {
                return Reply::Paused;
            }
        }
        if s.stop.is_empty()
            && let Some((kind, _)) = quick_stop
        {
            s.stop = match kind {
                crate::quick::turn::StopKind::Interrupt => tool_calls::CUT_SHORT.into(),
                crate::quick::turn::StopKind::Pause => oak::stop::TOOL_CALLS.into(),
            };
        }
        // The reply is complete: the server's copy now ends with it.
        *self.thread.lock().unwrap() = s.thread;
        if crate::bench::on() {
            bench.done(self, &s.stop);
        }
        let mut blocks = finish_blocks(blocks);
        if let Some(tps) = rate.reading() {
            mark_speed(&mut blocks, tps);
        }
        Reply::Done(blocks, s.stop, s.usage)
    }

    /// Builds the round's request (kept in `last_request`) and starts
    /// streaming it on a thread of its own: its events come on the channel.
    /// In a server-side thread (`api::threads`), a request that only adds
    /// to the last one sends just what it adds; the thread is returned.
    fn send_request(&self) -> (Receiver<Sse>, Option<api::Thread>) {
        let (tx, rx) = channel();
        let (body, n, base) = self.request_body();
        *self.last_request.lock().unwrap() = Some((body.clone(), n, base, api::now_ms()));
        if crate::bench::on() {
            let bytes =
                serde_json::to_string(&body.messages).map_or(0, |m| m.len()) + body.system.len();
            self.bench(
                "request_built",
                json!({"messages": n, "body_bytes": bytes, "kind": "main"}),
            );
        }
        let thread = api::threads().then(|| api::Thread::of(&body));
        if thread.is_some() {
            let cont = self
                .thread
                .lock()
                .unwrap()
                .take()
                .and_then(|t| Some((t.id.clone(), t.continues(&body)?)));
            std::thread::spawn(move || api::stream_thread(body, cont, tx));
        } else {
            std::thread::spawn(move || api::stream_messages(body, tx));
        }
        (rx, thread)
    }

    // ------------------------------------------------------------ smart context

    /// Records what the API said the request cost and, once the context has
    /// passed the trigger, starts summarizing the older part in the background.
    pub(super) fn after_reply(&self, usage: Usage) {
        if usage.prompt > 0 || usage.output > 0 {
            let t = store::Tokens {
                requests: 1,
                input: usage.input,
                cache_read: usage.cache_read,
                cache_write: usage.write,
                output: usage.output,
                usd: 0.0,
            };
            self.conv.lock().unwrap().tokens.add(&t);
        }
        let Some((body, n, base, at)) = self.last_request.lock().unwrap().take() else {
            return;
        };
        if usage.prompt == 0 {
            return; // no usage reported
        }
        let mut c = self.conv.lock().unwrap();
        let m = Measure {
            base,
            n,
            prompt: usage.prompt,
        };
        if body.provider != api::Provider::Quick {
            c.rolling.record(m, usage.output);
        } else {
            // Quick's count is of its own thread (its prompt, its agent's
            // steps), not of the messages Oak would send: a summary made
            // after a switch to another model mustn't place its cut by it
            // (`rolling::plan`). The size stays, as a guide.
            c.rolling.context = m.prompt + usage.output;
        }
        // How long the cache lives, for its cold moments (`request_body`):
        // as Claude's request chose; the user's own server keeps its slot
        // until another conversation takes it (an hour stands for that);
        // the other providers' caches live minutes. Freebuff's isn't ours.
        let ttl = match body.provider {
            api::Provider::Claude => Some(body.ttl),
            api::Provider::Local => Some(oak::Ttl::OneHour),
            api::Provider::Freebuff => None,
            _ => Some(oak::Ttl::FiveMinutes),
        };
        if let Some(ttl) = ttl {
            let choice = crate::cache_ttl::Choice {
                ttl,
                anchor: body
                    .anchor
                    .filter(|_| body.provider == api::Provider::Claude),
            };
            c.cache
                .record(at, choice, usage.prompt, body.messages.len(), base);
        }
        if body.provider == api::Provider::Claude {
            let warm = api::keep_alive()
                && body.ttl == oak::Ttl::FiveMinutes
                && body.purpose == oak::Purpose::Main;
            *self.warm.lock().unwrap() = warm.then(|| (body.clone(), false));
        } else {
            // On another provider now: the Claude request before is no
            // longer the one to keep warm.
            *self.warm.lock().unwrap() = None;
        }
        let cfg = rolling::config_for(&c.model);
        if !rolling::applies(body.provider) || c.rolling.off || c.rolling.base() != base || c.rolling.context < cfg.trigger {
            return;
        }
        {
            let l = self.live.lock().unwrap();
            let cooling = l.rolling_failed.is_some_and(|(at, size, wait)| {
                at.elapsed() < wait && c.rolling.context < size + size / 10
            });
            if l.compacting || cooling {
                return;
            }
        }
        // Quick's count is of its own thread: where the cut goes is placed
        // by what Oak's copy of the conversation holds (`quick_view_size`),
        // and the keep is taken to the copy's units (`quick_keep`).
        let (total, keep) = if body.provider == api::Provider::Quick {
            let view = super::quick_view_size(&mut c, n, base);
            (view, super::quick_keep(cfg.keep, view, usage.prompt))
        } else {
            (usage.prompt, cfg.keep)
        };
        let Some(plan) = plan_summary(&mut c, n, base, total, keep) else {
            return;
        };
        // The request that just finished, with only the part the summary
        // replaces, as that request sent it, then the instruction.
        let request = summary_request_for(&c, &body, n, plan.cut);
        drop(c);
        self.set(|l| l.compacting = true);
        let reverts = self.live.lock().unwrap().reverts;
        let (conv, live, ctx) = (self.conv.clone(), self.live.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = summarize(&conv, request, false);
            finish_summary(&conv, &live, base, plan, overhead(&body), reverts, result);
            ctx.request_repaint();
        });
    }

    /// The request was too long for the model (the API refuses those at
    /// once, unbilled). If a summary is on its way, wait for it; otherwise
    /// summarize right away, sending only the part the summary replaces (as
    /// the background summary does). Requests never wait for a summary
    /// otherwise.
    pub(super) fn summarize_now(&self, error: &str) -> Result<(), String> {
        while self.live.lock().unwrap().compacting {
            self.set(|l| {
                l.status =
                    "Waiting for the context summary: the conversation outgrew the model's context"
                        .into()
            });
            std::thread::sleep(Duration::from_millis(100));
        }
        let (body, n, base) = self.request_body();
        let mut c = self.conv.lock().unwrap();
        if !rolling::applies(body.provider) {
            return Err("smart context doesn't apply to this provider".into());
        }
        if c.rolling.off {
            return Err("smart context is off for this conversation".into());
        }
        if base
            != self
                .last_request
                .lock()
                .unwrap()
                .as_ref()
                .map_or(base, |r| r.2)
        {
            return Ok(()); // a summary just landed: try with it
        }
        // "prompt is too long: 213456 tokens > 200000 maximum"; without a
        // size, the last measured request and what came since.
        let total = error
            .split(|ch: char| !ch.is_ascii_digit())
            .find_map(|d| d.parse::<u64>().ok().filter(|&t| t > 1000))
            .unwrap_or_else(|| {
                c.messages.load_from(base);
                let context = c.rolling.context;
                rolling::size_of(
                    &c.messages[..n.min(c.messages.len())],
                    base,
                    &c.rolling.measured,
                    context,
                )
            });
        let keep = rolling::config_for(&c.model).keep;
        let plan = match plan_summary(&mut c, n, base, total, keep) {
            Some(plan) => plan,
            // One could be made, but not in one request on this model.
            None if plan_summary_within(&mut c, n, base, total, keep, u64::MAX).is_some() => {
                return Err(too_big_to_summarize(&c.model));
            }
            None => return Err("nothing old enough to summarize".into()),
        };
        let request = summary_request_for(&c, &body, n, plan.cut);
        drop(c);
        self.set(|l| {
            l.compacting = true;
            l.status = "Summarizing older context to make room".into();
        });
        let result = summarize(&self.conv, request, true);
        let ok = result.is_ok();
        let reverts = self.live.lock().unwrap().reverts;
        finish_summary(
            &self.conv,
            &self.live,
            base,
            plan,
            overhead(&body),
            reverts,
            result,
        );
        self.ctx.request_repaint();
        if ok {
            Ok(())
        } else {
            Err(self
                .conv
                .lock()
                .unwrap()
                .rolling
                .last_error
                .clone()
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod status_line {
    use super::super::tests::idle_worker;
    use super::*;

    fn feed(w: &Worker, s: &mut Streaming, b: &mut Batch, ev: Event) {
        let mut l = w.live.lock().unwrap();
        s.apply(&mut l, b, ev);
    }

    fn status(w: &Worker) -> String {
        w.live.lock().unwrap().status.clone()
    }

    /// The status line says what the reply is doing: the model writing a
    /// block, a call that started while the reply streams in running (not
    /// "Writing" it), and the model again once it is done; a provider's
    /// own calls (Quick's) are running from when they are made.
    #[test]
    fn the_status_line_follows_the_reply_and_its_calls() {
        let dir = std::env::temp_dir().join(format!("oak-status-line-{}", api::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, s) = idle_worker(
            "status-line",
            Conversation {
                folder: dir.clone(),
                ..Default::default()
            },
        );
        s.conv
            .lock()
            .unwrap()
            .messages
            .push(json!({"role": "user", "content": [oak::text("go")]}));
        let exec = json!({"type": "tool_call", "id": "c1", "name": "EXECUTE", "input": {}});
        let running = format!("Running {}", crate::transcript::shown(&exec));
        std::thread::scope(|scope| {
            let mut b = Batch::new(&w, scope);
            let mut st = Streaming::default();
            let reasoning = json!({"type": "reasoning", "text": ""});
            feed(&w, &mut st, &mut b, Event::Block { index: 0, block: reasoning });
            assert_eq!(status(&w), "Thinking");
            feed(&w, &mut st, &mut b, Event::BlockEnd { index: 0 });
            feed(&w, &mut st, &mut b, Event::Block { index: 1, block: exec.clone() });
            assert_eq!(status(&w), format!("Writing {}", crate::transcript::shown(&exec)));
            let input = r#"{"command": "sleep 1", "summary": "Wait", "timeout": 20}"#;
            feed(&w, &mut st, &mut b, Event::Input { index: 1, json: input.into() });
            feed(&w, &mut st, &mut b, Event::BlockEnd { index: 1 });
            b.pump(false, oak::stop::TOOL_CALLS);
            st.calls_changed(&w, &mut b);
            assert_eq!(status(&w), running, "it runs while the reply streams in");
            // The model writes on meanwhile, then only the call is left.
            feed(&w, &mut st, &mut b, Event::Block { index: 2, block: oak::text("") });
            assert_eq!(status(&w), "Writing");
            feed(&w, &mut st, &mut b, Event::BlockEnd { index: 2 });
            assert_eq!(status(&w), running);
            let until = Instant::now() + Duration::from_secs(30);
            while status(&w) == running && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
                b.pump(false, oak::stop::TOOL_CALLS);
                st.calls_changed(&w, &mut b);
            }
            assert_eq!(status(&w), "Waiting for the model", "the call is done");

            // Quick's calls come whole, and it runs them.
            st.provider_runs = true;
            let search = json!({"type": "tool_call", "id": "q1", "name": "file_rag_search", "input": {"query": "x"}});
            feed(&w, &mut st, &mut b, Event::Block { index: 3, block: search.clone() });
            assert_eq!(status(&w), format!("Running {}", crate::transcript::shown(&search)));
            let mut done = search.clone();
            done["_oak_result"] = oak::tool_result(&json!("q1"), json!("found"), false);
            feed(&w, &mut st, &mut b, Event::Block { index: 3, block: done });
            feed(&w, &mut st, &mut b, Event::BlockEnd { index: 3 });
            assert_eq!(status(&w), "Waiting for the model");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
