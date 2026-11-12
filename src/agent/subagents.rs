//! The subagent tools: AGENT_NEW, AGENT_SEND, AGENT_LIST, AGENT_SET,
//! AGENT_PAUSE, AGENT_RESUME, AGENT_STOP, AGENT_DELETE and AGENT_WAIT.

use crate::tools::{Head, one_line};
use super::*;

impl Worker {
    /// The label and session of one of this session's own subagents.
    fn own_agent(&self, id: u64) -> Result<(String, Session), String> {
        let (_, contacts) = self.contacts();
        let (_, label, conv) = contacts.into_iter().find(|c| c.0 == id && id != 0).ok_or(format!("No agent {id}. AGENT action=list shows your agents."))?;
        let s = find_session(&conv).ok_or(format!("{label} isn't running."))?;
        Ok((label, s))
    }

    /// AGENT_NEW: starts a subagent, a session of its own in the same
    /// folder, on the model and effort asked for (or the main session's),
    /// whose first message is the prompt (from the main session). The app
    /// finds it with `take_spawned`.
    pub(crate) fn agent_new(&self, input: &Value) -> Result<String, String> {
        let name: String = one_line(input["name"].as_str().unwrap_or("")).head(60);
        let prompt = input["prompt"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Err("The name is empty.".into());
        }
        if prompt.is_empty() {
            return Err("The prompt is empty.".into());
        }
        let (model, effort) = model_and_effort(input)?;
        let model = model.unwrap_or_else(|| self.conv.lock().unwrap().model.clone());
        // A Freebuff agent gets its prompt as a plain message, reworded by
        // Gemma first when it has a trigger word in it (`rewrite.rs`); if
        // the rewrite fails, the prompt goes as it is.
        let freebuff = api::Provider::of(&model) == api::Provider::Freebuff;
        let mut prompt = prompt.to_string();
        let mut reworded = "";
        if freebuff && rewrite::triggered(&prompt) {
            self.set(|l| l.status = "Rewording the agent's prompt".into());
            let (out, cost) = rewrite::rewrite(&prompt);
            self.conv.lock().unwrap().spent += cost;
            match out {
                Ok(p) => {
                    prompt = p;
                    reworded = " Its prompt was reworded before it got it.";
                }
                Err(e) => eprintln!("agent prompt rewrite failed: {e}"),
            }
        }
        let mut c = self.conv.lock().unwrap();
        let effort = effort.unwrap_or_else(|| c.effort.clone());
        // Only the main session can make a sub-orchestrator.
        let spawner = input["can_spawn"].as_bool().unwrap_or(false) && c.parent.is_none();
        c.next_agent_id += 1;
        let n = c.next_agent_id;
        let now = api::now_ms();
        let conv = Conversation {
            id: format!("{}-agent{n}", c.id),
            title: name.clone(),
            titled: true,
            folder: c.folder.clone(),
            provider: Some(if model == c.model { c.provider() } else { api::Provider::of(&model) }),
            model: model.clone(),
            effort: effort.clone(),
            // Its system prompt is the main session's role's.
            role: c.role.clone(),
            last_message_at: now,
            rolling: rolling::State { off: c.rolling.off, ..Default::default() },
            parent: Some(c.id.clone()),
            agent_id: n,
            spawner,
            ..Default::default()
        };
        c.agents.push(crate::store::AgentRef { id: n, conv: conv.id.clone(), name: name.clone() });
        c.save();
        drop(c);
        conv.save();
        let id = conv.id.clone();
        let s = Session::start(conv, self.ctx.clone());
        s.deliver(if freebuff { prompt.trim().to_string() } else { agent_message(MAIN_LABEL, false, &prompt) });
        SPAWNED.lock().unwrap().push((id, s));
        self.ctx.request_repaint();
        Ok(format!(
            "Started agent {n} ({name}) on {model} at {effort} effort.{reworded} It works in the background; its messages come to you as <agent-message> blocks with your \
             next request, and so does its final reply when it ends its turn. AGENT action=send id {n} sends it a message."
        ))
    }

    /// AGENT_SET: another model and/or effort for a subagent, from its
    /// next request (as when the user picks one in its session box).
    pub(crate) fn agent_set(&self, id: u64, input: &Value) -> Result<String, String> {
        let (model, effort) = model_and_effort(input)?;
        if model.is_none() && effort.is_none() {
            return Err("Give a `model`, an `effort` or both.".into());
        }
        let (label, s) = self.own_agent(id)?;
        let (old_model, old_effort) = {
            let c = s.conv.lock().unwrap();
            (c.model.clone(), c.effort.clone())
        };
        let model = model.unwrap_or(old_model);
        let effort = effort_for(&model, &effort.unwrap_or(old_effort));
        s.command(Command::Model(api::Provider::of(&model), model.clone()));
        s.command(Command::Effort(effort.clone()));
        Ok(format!("{label} goes on {model} at {effort} effort from its next request."))
    }

    /// AGENT_PAUSE: a subagent pauses after the request under way (as the
    /// pause button does): it does nothing until resumed, and messages for
    /// it wait.
    pub(crate) fn agent_pause(&self, id: u64) -> Result<String, String> {
        let (label, s) = self.own_agent(id)?;
        if s.paused.load(Ordering::SeqCst) {
            return Ok(format!("{label} is paused already."));
        }
        let when = if s.request_pause() { "after its current request" } else { "now" };
        Ok(format!("{label} pauses {when}. AGENT action=resume lets it carry on."))
    }

    /// AGENT_RESUME: a paused subagent carries on where it stopped.
    pub(crate) fn agent_resume(&self, id: u64) -> Result<String, String> {
        let (label, s) = self.own_agent(id)?;
        if !s.pause_on() {
            return Ok(format!("{label} isn't paused."));
        }
        s.set_paused(false);
        Ok(format!("{label} carries on."))
    }

    /// Who this session is in messages, and the sessions it can write to:
    /// (number, label, conversation id), the main session being 0.
    fn contacts(&self) -> (String, Vec<(u64, String, String)>) {
        let c = self.conv.lock().unwrap();
        let mine = |agents: &[crate::store::AgentRef]| agents.iter().map(|a| (a.id, agent_label(a.id, &a.name), a.conv.clone())).collect::<Vec<_>>();
        match c.parent.clone() {
            None => (MAIN_LABEL.to_string(), mine(&c.agents)),
            // A sub-orchestrator writes to the main session and its own
            // agents (numbered by it), not to its siblings.
            Some(parent) if c.spawner => {
                let me = agent_label(c.agent_id, &c.title);
                let own = mine(&c.agents);
                drop(c);
                (me, std::iter::once((0, MAIN_LABEL.to_string(), parent)).chain(own).collect())
            }
            Some(parent) => {
                let me = (agent_label(c.agent_id, &c.title), c.agent_id);
                drop(c);
                let (title, siblings) = match find_session(&parent) {
                    Some(p) => {
                        let pc = p.conv.lock().unwrap();
                        (pc.title.clone(), mine(&pc.agents))
                    }
                    None => (String::new(), vec![]),
                };
                let main = if title.is_empty() { MAIN_LABEL.to_string() } else { format!("{MAIN_LABEL} ({})", title.replace('"', "'")) };
                let others = siblings.into_iter().filter(|a| a.0 != me.1);
                (me.0, std::iter::once((0, main, parent)).chain(others).collect())
            }
        }
    }

    /// Subagent `id` (not the main session): its label and conversation id,
    /// or "No agent N." and `hint`.
    fn subagent(&self, id: u64, hint: &str) -> Result<(String, String), String> {
        let (_, contacts) = self.contacts();
        let (_, label, conv) = contacts.into_iter().find(|c| c.0 == id && id != 0).ok_or(format!("No agent {id}.{hint}"))?;
        Ok((label, conv))
    }

    /// AGENT_SEND: a message for a subagent (or, from a subagent, for the
    /// main session or another agent), with the receiver's next request.
    pub(crate) fn agent_send(&self, id: u64, message: &str) -> Result<String, String> {
        if message.trim().is_empty() {
            return Err("The message is empty.".into());
        }
        let (me, contacts) = self.contacts();
        let Some((_, label, conv)) = contacts.into_iter().find(|c| c.0 == id) else {
            let (agent, mine) = {
                let c = self.conv.lock().unwrap();
                (c.parent.is_some(), c.agent_id)
            };
            return Err(match agent {
                true if mine == id => "That is you.".into(),
                true => format!("No agent {id}. Use 0 for the main session; AGENT action=list shows the others."),
                false => format!("No agent {id}. AGENT action=list shows your agents."),
            });
        };
        let s = find_session(&conv).ok_or(format!("{label} isn't running."))?;
        let idle = s.state() == "idle";
        // A Freebuff agent gets it as a plain message, as its first prompt.
        let freebuff = api::Provider::of(&s.conv.lock().unwrap().model) == api::Provider::Freebuff;
        s.deliver(if freebuff { message.trim().to_string() } else { agent_message(&me, false, message) });
        let wakes = if idle { " It was idle, so it starts on it now." } else { "" };
        Ok(format!("Sent to {label}: it gets the message with its next request.{wakes}"))
    }

    /// AGENT_LIST: the sessions this one can write to, and what they do.
    pub(crate) fn agent_list(&self) -> String {
        let (_, contacts) = self.contacts();
        if contacts.is_empty() {
            return "No agents yet: AGENT action=new starts one.".into();
        }
        let mut out = String::from("id\tstate\tlast change\tmodel\tname\n");
        for (id, label, conv) in contacts {
            let (state, at, model) = match find_session(&conv) {
                Some(s) => {
                    let c = s.conv.lock().unwrap();
                    let (at, model) = (c.last_message_at, format!("{} ({})", c.model, c.effort));
                    drop(c);
                    (s.state(), at, model)
                }
                None => ("not running".into(), 0, "-".into()),
            };
            let ago = if at > 0 { format!("{} ago", fmt_duration(Duration::from_millis((api::now_ms() - at).max(0) as u64))) } else { "-".into() };
            out += &format!("{id}\t{state}\t{ago}\t{model}\t{label}\n");
        }
        out
    }

    /// ACCOUNTS: the usage of every account, fetched now.
    pub(crate) fn accounts_tool(&self) -> String {
        api::refresh_accounts();
        let model = self.conv.lock().unwrap().model.clone();
        alerts::accounts_text(&model)
    }

    /// AGENT_STOP: interrupts a subagent, whose turn then ends without a
    /// report back.
    pub(crate) fn agent_stop(&self, id: u64) -> Result<String, String> {
        let (label, conv) = self.subagent(id, "")?;
        let s = find_session(&conv).ok_or(format!("{label} isn't running."))?;
        let mut l = s.live.lock().unwrap();
        if !l.busy {
            return Ok(format!("{label} is idle already."));
        }
        l.quiet_stop = true;
        drop(l);
        s.interrupt();
        Ok(format!("Stopped {label}: it stops what it is doing and goes idle. AGENT action=send gives it more to do."))
    }

    /// AGENT_DELETE: stops a subagent for good and removes it: from this
    /// session's agents, from the app, and its conversation file.
    pub(crate) fn agent_delete(&self, id: u64) -> Result<String, String> {
        let (label, conv) = self.subagent(id, " AGENT action=list shows your agents.")?;
        // What it used, kept here (its own record goes with it), with what
        // its own agents used if it was a sub-orchestrator.
        let used = find_session(&conv).map(|s| {
            let c = s.conv.lock().unwrap();
            let mut t = c.tokens;
            t.usd = c.spent;
            t.add(&c.agent_tokens);
            t
        });
        {
            let mut c = self.conv.lock().unwrap();
            c.agents.retain(|a| a.id != id);
            if let Some(t) = &used {
                c.agent_tokens.add(t);
            }
            c.save();
        }
        match find_session(&conv) {
            Some(s) => s.delete(),
            None => {
                crate::store::remove_conversation(&conv);
                DELETED.lock().unwrap().push(conv);
            }
        }
        self.ctx.request_repaint();
        let used = used.filter(|t| t.requests > 0).map(|t| format!(" It used {}.", t.describe())).unwrap_or_default();
        Ok(format!("Deleted {label}: it stopped, its background tasks were killed, and its conversation was removed.{used}"))
    }

    /// AGENT_WAIT: waits until a message from another session arrives (it
    /// then follows the result, with the next request), the user writes or
    /// interrupts, or the timeout passes.
    pub(crate) fn agent_wait(&self, input: &Value) -> (Result<String, String>, bool) {
        let timeout = match input["timeout"].as_u64() {
            Some(s @ 1..=tools::AGENT_WAIT_MAX) => Duration::from_secs(s),
            _ => return (Err(format!("`timeout` must be from 1 to {} seconds.", tools::AGENT_WAIT_MAX)), false),
        };
        self.set(|l| l.status = "Waiting for agent messages".into());
        let _waiting = Running::new(&self.live, true);
        let t = Instant::now();
        loop {
            {
                let n = self.conv.lock().unwrap().inbox.len();
                let l = self.live.lock().unwrap();
                if n > 0 {
                    let what = if n == 1 { "A message arrived".to_string() } else { format!("{n} messages arrived") };
                    let follows = if n == 1 { "it follows" } else { "they follow" };
                    return (Ok(format!("{what} after {}; {follows} this result.", fmt_duration(t.elapsed()))), false);
                }
                if l.ready_count() > 0 {
                    return (Ok(format!("The wait ended after {} because the user sent a message (it follows).", fmt_duration(t.elapsed()))), false);
                }
            }
            if self.stopping() {
                return (Err("The user sent a message.".into()), true);
            }
            if t.elapsed() >= timeout {
                return (Ok(format!("No message came in {}s.", timeout.as_secs())), false);
            }
            self.wait_a_bit(30);
        }
    }
}

/// AGENT new's and set's `model` and `effort`, checked: a model this Oak
/// can use (not the local server's, which serves one request at a time)
/// and an effort level.
fn model_and_effort(input: &Value) -> Result<(Option<String>, Option<String>), String> {
    let model = input["model"].as_str().map(str::trim).filter(|m| !m.is_empty());
    if let Some(m) = model {
        let known: Vec<api::Model> = api::known_models().into_iter().filter(|k| k.provider != api::Provider::Local).collect();
        if !known.iter().any(|k| k.id == m) {
            let ids: Vec<&str> = known.iter().map(|k| k.id.as_str()).collect();
            return Err(format!("Unknown model {m}. Pick one of: {}.", ids.join(", ")));
        }
    }
    let effort = input["effort"].as_str().map(str::trim).filter(|e| !e.is_empty());
    if let Some(e) = effort
        && !EFFORTS.contains(&e)
    {
        return Err(format!("Unknown effort {e}. Pick one of: {}.", EFFORTS.join(", ")));
    }
    Ok((model.map(str::to_string), effort.map(str::to_string)))
}
