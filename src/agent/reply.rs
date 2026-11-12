//! Building requests, keeping their caches warm, processing streamed replies
//! and scheduling context summaries. The turn loop lives in the parent module.

use super::*;
use super::memory::memory_for_request;
use super::prompt::system_prompt_on;
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
    let mut thinking: Vec<&mut Value> = blocks.iter_mut().filter(|b| oak::is_reasoning(b)).collect();
    if n == 0 || thinking.is_empty() {
        return;
    }
    fn shown(b: &Value) -> Option<&str> {
        b["text"].as_str().filter(|_| b["redacted"] != true)
    }
    let visible: Vec<u64> = thinking.iter().filter_map(|b| shown(b)).map(|t| t.len().max(1) as u64).collect();
    let average = (visible.iter().sum::<u64>() / visible.len().max(1) as u64).max(1);
    let weight = |b: &Value| shown(b).map_or(average, |t| t.len().max(1) as u64);
    let total: u64 = thinking.iter().map(|b| weight(b)).sum();
    let last = thinking.len() - 1;
    let mut given = 0;
    for (k, b) in thinking.iter_mut().enumerate() {
        let share = if k == last { n - given } else { n * weight(b) / total };
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
}

impl Streaming {
    /// Puts one streamed event into the reply's blocks (`Live::blocks`);
    /// a tool call that is complete goes to `batch`.
    fn apply(&mut self, l: &mut Live, batch: &mut Batch, ev: Event) {
        match ev {
            Event::Retry { attempt, wait, reason } => {
                l.status = format!("Retry {attempt}/{} in {wait}s after: {reason}", api::RETRIES);
            }
            Event::Block { index: i, block: b } => {
                l.status = match b["type"].as_str() {
                    Some("reasoning") => "Thinking".into(),
                    Some("tool_call") => format!("Writing {}", crate::transcript::shown(&b)),
                    _ => "Writing".into(),
                };
                if l.blocks.len() <= i {
                    l.blocks.resize(i + 1, Value::Null);
                }
                l.blocks[i] = b;
                self.started.resize(self.started.len().max(i + 1), Instant::now());
                self.started[i] = Instant::now();
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
                        self.rewrites.extend(crate::unslop::mark_thinking(b, false));
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
                if let Some(b) = l.blocks.get_mut(index) {
                    match b.get_mut("_oak_partial") {
                        Some(Value::String(s)) => s.push_str(&d),
                        _ => b["_oak_partial"] = json!(d),
                    }
                }
            }
            Event::BlockEnd { index: i } => {
                let Some(b) = l.blocks.get_mut(i) else { return };
                // How long the model thought, for "Thought for Ns". OAK's
                // own keys are taken out before requests (rolling::view).
                if oak::is_reasoning(b)
                    && let Some(t) = self.started.get(i)
                {
                    b["_oak_ms"] = json!(t.elapsed().as_millis() as u64);
                    if self.rewrite_thinking {
                        self.rewrites.extend(crate::unslop::mark_thinking(b, true));
                    }
                }
                if oak::is_call(b) {
                    finish_call(b);
                    batch.add(b.clone());
                }
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
}

/// A streamed tool call is complete: its input, streamed as text, is
/// parsed (and repaired); input that doesn't parse is kept raw.
fn finish_call(b: &mut Value) {
    let partial = b.as_object_mut().and_then(|o| o.remove("_oak_partial"));
    let raw = partial.as_ref().and_then(Value::as_str).unwrap_or("").trim();
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
        let (messages, dead) = self.warm.lock().unwrap().as_ref().map(|(req, dead)| (req.messages.len(), *dead))?;
        let c = self.conv.lock().unwrap();
        let last = c.cache.last?;
        let now = api::now_ms();
        let due = c.cache.expires_at()? - 30_000;
        let fits = !dead && !c.cache.cold && last.base == c.rolling.base() && last.messages == messages;
        (fits && now < due + 25_000 && due - c.cache.asked < KEEP_ALIVE_FOR).then_some(due)
    }

    /// Reads the cache again with a request that generates nothing
    /// (`api::keep_warm`), so it lives five more minutes. A ping that wrote
    /// most of the context instead (the cache was gone) or failed ends
    /// keep-alive for this request.
    pub(super) fn keep_warm(&self) {
        let Some(req) = self.warm.lock().unwrap().as_ref().map(|w| w.0.clone()) else { return };
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
        // Nothing cached is left (expired, another model or account, a
        // summary): what would otherwise cost a full cache write is free now.
        let provider = c.provider();
        let cold = provider != api::Provider::Freebuff && c.cache.is_cold(now, c.rolling.base());
        if cold && !c.rolling.off {
            let messages = std::mem::take(&mut c.messages);
            c.rolling.clear_cold(&messages);
            c.messages = messages;
        }
        let memory = memory_for_request(&mut c, now, cold);
        let scratch = scratchpad(&mut c);
        let context = memory_context(&c).to_string();
        let agent = c.parent.is_some().then(|| agent_label(c.agent_id, &c.title));
        // A model on the user's own server gets the basics only, and the
        // goal's tools while there is one.
        let local = crate::local::serves(&c.model);
        let mut kits = c.prefix.kits.clone();
        if local && !c.goal.is_empty() {
            kits.push("GOAL".into());
        }
        // (A model without effort levels goes without.)
        let effort = Some(effort_for(&c.model, &c.effort).to_string()).filter(|e| !e.is_empty());
        let tools = tools::definitions_with_kits(agent.is_some(), c.spawner, local, &kits);
        let built = crate::prefix::Pin { day: crate::time::today_utc(), kits, effort, tools };
        // The stable prefix: the date, kits, tools and effort the cache was
        // written with, and messages for what changed since (`crate::prefix`).
        let mode = crate::prefix::mode(provider, &c.model);
        let (n, cut) = (c.messages.len(), c.rolling.base());
        let pin = match mode {
            Some(mode) => {
                let add = last_is(&c, "user");
                let effort_per_message = mode == crate::prefix::Mode::Claude && crate::prefix::supports_effort(&c.model);
                c.prefix.prepare(crate::prefix::Setup { mode, cold, n, cut, add, effort_per_message, now: built })
            }
            None => built,
        };
        let system = system_prompt_on(&pin.day, &c.folder, &scratch, &c.model, agent.as_deref(), &pin.kits, &c.role);
        let tools = Value::Array(pin.tools);
        // Again after a switch to a model whose provider sends other things
        // (another identity, prompt or tools), where it takes effect.
        let p = api::preamble(&c.model, &system, &tools);
        if start || preamble_changed(&c, &p) {
            let at = if start { 0 } else { c.messages.len() };
            start_marks(&mut c, marks_before, &p, &context, at);
        }
        let mut messages = request_messages(&c, &c.messages);
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
            route: (provider == api::Provider::OpenRouter && !c.served_by.is_empty()).then(|| c.served_by.clone()),
            // The Freebuff sidecar signs and snapshots the project the same
            // way the CLI does; it needs the folder.
            folder: Some(c.folder.to_string_lossy().into_owned()),
            conv: Some(c.id.clone()),
            ttl: cache.ttl,
            anchor: cache.anchor,
            ..Default::default()
        };
        (req, c.messages.len(), c.rolling.base())
    }

    /// One request to the model, its reply streamed into `Live::blocks`. A pause the user asked for meanwhile
    /// (`Session::request_pause`) starts once it is over, however it ended:
    /// the reply is kept, its tool calls run, and the next request waits.
    pub(super) fn ask(&self, batch: &mut Batch) -> Reply {
        self.set(|l| l.requesting = true);
        let reply = self.ask_inner(batch);
        self.end_request();
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
            return Reply::Done(batch.kept(blocks), tool_calls::CUT_SHORT.into(), Usage::default());
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
            c.save();
        }
    }

    fn ask_inner(&self, batch: &mut Batch) -> Reply {
        let (rx, thread) = self.send_request();
        self.set(|l| {
            l.blocks.clear();
            l.status = "Waiting for the model".into();
        });
        let mut s = Streaming {
            thread,
            // Rewrite thinking (Unslop): sections go to be rewritten as they
            // come in, the rest when their block ends.
            rewrite_thinking: crate::unslop::applies(&self.conv.lock().unwrap()) && crate::unslop::thinking(),
            ..Default::default()
        };
        // Output speed, from the provider's output token count only.
        let mut rate = crate::token_rate::TokenRate::default();
        let mut bench = BenchReply::default();
        loop {
            // Checked on every event too: a busy stream may never go quiet.
            // (The blocks are cleared by `ask`.)
            if self.paused.load(Ordering::SeqCst) {
                return Reply::Paused;
            }
            if self.interrupted() {
                return Reply::Interrupted;
            }
            // Tool calls that streamed in start as soon as they may.
            batch.pump(false, oak::stop::TOOL_CALLS);
            let ev = match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Sse::Event(ev)) => ev,
                Ok(Sse::Done) => break,
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
                Err(RecvTimeoutError::Disconnected) => return Reply::Failed("Stream ended unexpectedly".into()),
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
            let delta = matches!(ev, Event::Text { .. } | Event::Reasoning { .. } | Event::Input { .. } | Event::Sig { .. });
            let (drawn, viewport) = {
                let mut l = self.live.lock().unwrap();
                s.apply(&mut l, batch, ev);
                (l.drawn, l.drawn_viewport.unwrap_or(eframe::egui::ViewportId::ROOT))
            };
            if !s.rewrites.is_empty() {
                crate::unslop::start_parts(std::mem::take(&mut s.rewrites), &self.conv, &self.ctx);
            }
            // A delta only changes the transcript; any other event may
            // change what Home and the tabs show (the account's limits).
            if !delta || drawn + 1 >= self.ctx.cumulative_pass_nr_for(viewport) {
                self.ctx.request_repaint();
            }
        }
        let blocks = std::mem::take(&mut self.live.lock().unwrap().blocks);
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
            let bytes = serde_json::to_string(&body.messages).map_or(0, |m| m.len()) + body.system.len();
            self.bench("request_built", json!({"messages": n, "body_bytes": bytes, "kind": "main"}));
        }
        let thread = api::threads().then(|| api::Thread::of(&body));
        if thread.is_some() {
            let cont = self.thread.lock().unwrap().take().and_then(|t| Some((t.id.clone(), t.continues(&body)?)));
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
            let t = store::Tokens { requests: 1, input: usage.input, cache_read: usage.cache_read, cache_write: usage.write, output: usage.output, usd: 0.0 };
            self.conv.lock().unwrap().tokens.add(&t);
        }
        let Some((body, n, base, at)) = self.last_request.lock().unwrap().take() else { return };
        if usage.prompt == 0 {
            return; // no usage reported
        }
        let mut c = self.conv.lock().unwrap();
        c.rolling.record(Measure { base, n, prompt: usage.prompt }, usage.output);
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
            let choice = crate::cache_ttl::Choice { ttl, anchor: body.anchor.filter(|_| body.provider == api::Provider::Claude) };
            c.cache.record(at, choice, usage.prompt, body.messages.len(), base);
        }
        if body.provider == api::Provider::Claude {
            let warm = api::keep_alive() && body.ttl == oak::Ttl::FiveMinutes && body.purpose == oak::Purpose::Main;
            *self.warm.lock().unwrap() = warm.then(|| (body.clone(), false));
        }
        let cfg = rolling::config_for(&c.model);
        if c.rolling.off || c.rolling.base() != base || c.rolling.context < cfg.trigger {
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
        let Some(plan) = plan_summary(&c, n, base, usage.prompt) else { return };
        let prompt = rolling::prompt_for(&c.model, Some(&rolling::describe(&c.messages[plan.cut])));
        drop(c);
        // The request that just finished, plus the instruction: all but the
        // instruction was cached by that request.
        let request = rolling::summary_request(body.clone(), &prompt);
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
    /// summarize right away from a prefix short enough to send (no cache
    /// reuse on this path). Requests never wait for a summary otherwise.
    pub(super) fn summarize_now(&self, error: &str) -> Result<(), String> {
        while self.live.lock().unwrap().compacting {
            self.set(|l| l.status = "Waiting for the context summary: the conversation outgrew the model's context".into());
            std::thread::sleep(Duration::from_millis(100));
        }
        let (body, n, base) = self.request_body();
        let c = self.conv.lock().unwrap();
        if c.rolling.off {
            return Err("smart context is off for this conversation".into());
        }
        if base != self.last_request.lock().unwrap().as_ref().map_or(base, |r| r.2) {
            return Ok(()); // a summary just landed: try with it
        }
        // "prompt is too long: 213456 tokens > 200000 maximum"
        let total = error
            .split(|ch: char| !ch.is_ascii_digit())
            .find_map(|d| d.parse::<u64>().ok().filter(|&t| t > 1000))
            .unwrap_or(c.rolling.context);
        let plan = plan_summary(&c, n, base, total).ok_or("nothing old enough to summarize")?;
        let mut request = body.clone();
        request.messages = request_messages(&c, &c.messages[..plan.cut]);
        drop(c);
        let prompt = rolling::prompt_for(&request.model, None);
        let request = rolling::summary_request(request, &prompt);
        self.set(|l| {
            l.compacting = true;
            l.status = "Summarizing older context to make room".into();
        });
        let result = summarize(&self.conv, request, true);
        let ok = result.is_ok();
        let reverts = self.live.lock().unwrap().reverts;
        finish_summary(&self.conv, &self.live, base, plan, overhead(&body), reverts, result);
        self.ctx.request_repaint();
        if ok { Ok(()) } else { Err(self.conv.lock().unwrap().rolling.last_error.clone().unwrap_or_default()) }
    }

}
