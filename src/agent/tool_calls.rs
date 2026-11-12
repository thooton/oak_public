//! Running the model's tool calls: together, each as soon as it has
//! streamed in (calls on a file one of them writes keep their order, ASK
//! runs on its own), each by name, with EXECUTE, the task tools and ASK
//! here; files are tracked for changes on disk.

use crate::tools::Head;
use super::*;

impl Worker {
    fn alloc_task_id(&self) -> u64 {
        let mut c = self.conv.lock().unwrap();
        let id = c.next_task_id;
        c.next_task_id += 1;
        c.save();
        id
    }

    /// Registers a running command as a task that reports when it exits.
    fn adopt(&self, p: &Arc<Proc>, summary: &str) -> u64 {
        let id = self.alloc_task_id();
        self.tasks.add(id, p.clone(), summary);
        self.tell_exit(id, p);
        id
    }

    /// The tasks left running when Oak last closed: the model is told when
    /// they end, as of its other tasks (at once of those that ended while
    /// Oak was closed).
    pub(super) fn watch_revived(&self) {
        for (id, p) in self.tasks.take_revived() {
            self.tell_exit(id, &p);
        }
    }

    /// When task `id` exits, the model is told (unless it already knows).
    fn tell_exit(&self, id: u64, p: &Arc<Proc>) {
        let (tasks, proc): (Weak<Tasks>, Weak<Proc>) = (Arc::downgrade(&self.tasks), Arc::downgrade(p));
        let (live, wake, ctx) = (self.live.clone(), self.wake.clone(), self.ctx.clone());
        p.on_exit(Box::new(move || {
            let (Some(tasks), Some(p)) = (tasks.upgrade(), proc.upgrade()) else { return };
            if tasks.get(id).is_none() {
                return; // deleted: the model already knows
            }
            let cmd: String = p.command.head(60);
            let note = format!(
                "{NOTE_TAG}Task {id} (`{}`) {} after running {} in all. It has {} unread output lines; read them with TASK action=output.</task-notification>",
                cmd.replace('\n', " "),
                p.status(),
                tasks::fmt_took(p.elapsed()),
                p.unread()
            );
            let mut l = live.lock().unwrap();
            if l.reported.contains(&id) {
                return; // the model already saw it exit
            }
            l.notes.push((id, note));
            drop(l);
            let _ = wake.send(());
            ctx.request_repaint();
        }));
    }

    /// Records that the model has seen task `id` finish (or be removed), and
    /// drops any notification about it that hasn't been sent yet.
    fn mark_reported(&self, id: u64) {
        let mut l = self.live.lock().unwrap();
        l.reported.insert(id);
        l.notes.retain(|(n, _)| *n != id);
    }

    /// Task `id` as `TASK_GET_NEW_OUTPUT` and `TASK_WAIT` see it, or why
    /// there is none.
    fn task(&self, id: u64) -> Result<Arc<Proc>, String> {
        self.tasks.get(id).ok_or(format!("No task {id}."))
    }

    /// READ on an image file, or SCREENSHOT: tools whose result is an image.
    fn image_tool(&self, name: &str, input: &Value) -> Option<(Result<Vec<Value>, String>, bool)> {
        let folder = self.conv.lock().unwrap().folder.clone();
        match name {
            "READ" => {
                let path = tools::resolve(&folder, input["path"].as_str()?);
                crate::images::is_image_path(&path).then(|| (crate::images::read_for_model(&path), false))
            }
            "SCREENSHOT" => {
                let save = input["path"].as_str().map(|p| tools::resolve(&folder, p));
                let input = input.clone();
                Some(self.abandonable("Screenshot", move || crate::web::screenshot(&input, save)))
            }
            _ => None,
        }
    }

    /// Runs `f` on its own thread; the user can interrupt the wait for it.
    /// The bool is true when they did.
    fn abandonable<T: Send + 'static>(&self, what: &str, f: impl FnOnce() -> Result<T, String> + Send + 'static) -> (Result<T, String>, bool) {
        let (tx, rx) = channel();
        std::thread::spawn(move || tx.send(f()));
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => return (r, false),
                Err(RecvTimeoutError::Timeout) if self.stopping() => return (Err(format!("{what} abandoned: the user sent a message.")), true),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return (Err(format!("{what} failed.")), false),
            }
        }
    }

    /// ASK: shows the questions and waits for the answers, or for the user
    /// to skip them. Messages written meanwhile stay queued until then.
    fn ask_user(&self, input: &Value) -> (Result<String, String>, bool) {
        let ask = Ask::new(input).and_then(|ask| {
            let mut c = self.conv.lock().unwrap();
            let gate = ask_gate(&mut c);
            c.save();
            gate.map(|()| ask)
        });
        let ask = match ask {
            Ok(ask) => ask,
            Err(e) => return (Err(e), false),
        };
        self.set(|l| {
            l.ask = Some(ask);
            l.status = "Waiting for your answer".into();
        });
        let _waiting = Running::new(&self.live, true);
        loop {
            if self.stopping() {
                self.set(|l| l.ask = None);
                return (Err("Not answered: the user sent a message.".into()), true);
            }
            {
                let mut l = self.live.lock().unwrap();
                if l.ask.as_ref().is_some_and(|a| a.sent) {
                    let answers = l.ask.take().unwrap().answers();
                    drop(l);
                    self.ctx.request_repaint();
                    return (Ok(answers), false);
                }
                // Messages written meanwhile wait in the queue: only
                // answering or skipping ends the wait.
                if l.ask.as_ref().is_some_and(|a| a.skipped) {
                    l.ask = None;
                    let text = if l.ready_count() > 0 {
                        "The user skipped these questions without answering; they wrote a message instead (it follows)."
                    } else {
                        "The user skipped these questions without answering."
                    };
                    drop(l);
                    self.ctx.request_repaint();
                    return (Ok(text.into()), false);
                }
            }
            if !self.nap(Duration::from_millis(100)) {
                return (Err("Oak is closing.".into()), true);
            }
        }
    }

    /// Runs the tool calls of a reply (see `Batch`), all at once. True when
    /// the user sent a message.
    #[cfg(test)]
    pub(crate) fn run_calls(&self, calls: &[Value], stop: &str) -> bool {
        std::thread::scope(|s| Batch::new(self, s).finish(calls, stop))
    }

    /// Puts a call's result in the transcript, or holds it while its reply
    /// is still streaming in (`Live::early`).
    fn deliver(&self, r: Value) {
        {
            let mut l = self.live.lock().unwrap();
            if let Some(held) = l.early.as_mut() {
                held.push(r);
                drop(l);
                self.ctx.request_repaint();
                return;
            }
        }
        self.push_result(r);
    }

    /// The reply is in the transcript: the results held for it go there too.
    fn release_early(&self) {
        let held = self.live.lock().unwrap().early.take();
        for r in held.into_iter().flatten() {
            self.push_result(r);
        }
    }

    /// The error result for a call that never ran, in the transcript.
    fn not_run(&self, call: &Value, why: &str) -> Value {
        self.push_result(result_block(call, json!(why), true))
    }

    /// Runs one call of a batch (`run_call`), counted as running and timed
    /// for the benchmark. An interrupt sets `stopped`.
    fn run_traced(&self, call: &Value, stop: &str, stopped: &AtomicBool) -> Value {
        let (id, name) = (call["id"].as_str().unwrap_or(""), call["name"].as_str().unwrap_or(""));
        self.bench("tool_start", json!({"id": id, "name": name}));
        let t = Instant::now();
        let running = Running::new(&self.live, false);
        let (r, interrupted) = self.run_call(call, stop);
        drop(running);
        self.bench("tool_end", json!({"id": id, "name": name, "ms": ms_since(t)}));
        if interrupted {
            stopped.store(true, Ordering::SeqCst);
        }
        r
    }

    /// Puts a call's result in the transcript, and returns it.
    fn push_result(&self, r: Value) -> Value {
        self.push("user", vec![r.clone()]);
        r
    }

    /// Before a request: tells the model about files it had seen that
    /// changed on disk since, not by OAK, and puts a line in the transcript.
    pub(crate) fn report_changes(&self) {
        if !last_is(&self.conv.lock().unwrap(), "user") {
            return; // nothing to attach it to; it waits for the next one
        }
        let changes = self.files.lock().unwrap().check();
        if changes.is_empty() {
            return;
        }
        let mut c = self.conv.lock().unwrap();
        let folder = c.folder.clone();
        let names: Vec<String> = changes
            .iter()
            .map(|ch| ch.path().strip_prefix(&folder).unwrap_or(ch.path()).display().to_string())
            .collect();
        let message = changes::message(&changes);
        add_mark(&mut c, MarkKind::FilesChanged, names.join(", "), message.clone(), api::now_ms());
        drop(c);
        self.push("user", vec![context_block(&message)]);
    }

    /// Remembers a file the model just read, wrote or edited, as it is now.
    fn track(&self, name: &str, input: &Value) {
        if let ("READ" | "WRITE" | "EDIT", Some(p)) = (name, input["path"].as_str()) {
            let folder = self.conv.lock().unwrap().folder.clone();
            self.files.lock().unwrap().record(&tools::resolve(&folder, p));
        }
    }

    /// Runs one tool call and returns its tool_result block, and whether
    /// the user sent a message.
    fn run_call(&self, call: &Value, stop: &str) -> (Value, bool) {
        // A family's call (`TASK` with an action) runs as its action's tool.
        let canon = tools::canonical(call["name"].as_str().unwrap_or(""), &call["input"]);
        let (name, input) = match &canon {
            Ok((n, i)) => (n.as_str(), i),
            Err(_) => (call["name"].as_str().unwrap_or(""), &call["input"]),
        };
        let (out, is_error, interrupted) = if stop == crate::oak::stop::MAX_TOKENS || stop == crate::oak::stop::CONTEXT {
            (format!("Not run: the reply hit {stop} before this call was complete. Retry with smaller input (e.g. split a large WRITE)."), true, false)
        } else if let Some(raw) = call.get("_oak_raw") {
            (json!({"INVALID_JSON": raw}).to_string(), true, false)
        } else if name == "LOAD" {
            return self.load(call, input, stop);
        } else if let Some(e) = self.unloaded(name) {
            (e, true, false)
        } else if call["name"] == "BROWSER" || name.starts_with("BROWSER_") {
            return self.browser_call(call, canon.as_ref().ok().map(|(n, i)| (n.as_str(), i)));
        } else if let Err(e) = &canon {
            (format!("Invalid input: {e}"), true, false)
        } else if let Err(e) = tools::validate(name, input) {
            (format!("Invalid input: {e}"), true, false)
        } else if name == "DISPLAY" {
            // The files go with the result for the chat to show (never sent).
            let folder = self.conv.lock().unwrap().folder.clone();
            match tools::display(&folder, input) {
                Ok((text, shown)) => {
                    let mut r = result_block(call, json!(text), false);
                    r["_oak_display"] = json!(shown);
                    return (r, false);
                }
                Err(e) => (e, true, false),
            }
        } else if let Some((blocks, interrupted)) = self.image_tool(name, input) {
            // An image file or a screenshot: the result is the image itself.
            match blocks {
                Ok(b) => {
                    self.track(name, input);
                    return (result_block(call, json!(b), false), interrupted);
                }
                Err(e) => (e, true, interrupted),
            }
        } else {
            let (result, interrupted) = self.tool_for(call["id"].as_str().unwrap_or(""), name, input);
            match result {
                Ok(s) => (s, false, interrupted),
                Err(s) => (s, true, interrupted),
            }
        };
        if !is_error {
            self.track(name, input);
        }
        let out = if out.is_empty() { "(no output)".to_string() } else { out };
        // A command's output too long to send goes to the scratchpad, with a
        // preview: running it again might not give it back. (Other tools'
        // output can be asked for again, or comes in pages.)
        let out = if name == "EXECUTE" && tools::too_large(&out) {
            let dir = scratchpad(&mut self.conv.lock().unwrap());
            tools::persist(&out, &dir)
        } else {
            out
        };
        (result_block(call, json!(out), is_error), interrupted)
    }

    /// Why a call of tool `name` can't run yet: its kit isn't loaded.
    pub(crate) fn unloaded(&self, name: &str) -> Option<String> {
        let c = self.conv.lock().unwrap();
        let agent = c.parent.is_some();
        let kit = tools::kit_of(name, agent).filter(|k| tools::kits_for(agent).contains(k))?;
        (!c.prefix.kits.iter().any(|k| k == kit)).then(|| format!("{} needs the {kit} kit: call LOAD with name={kit} first.", tools::call_name(name)))
    }

    /// LOAD: adds a kit to the session (its tools join the list, at once or
    /// at the next renewal: `crate::prefix`) and returns its instructions.
    /// With `input`, calls the loaded kit's tool through LOAD (where tools
    /// can't be added mid-conversation, until its tool is in the list).
    fn load(&self, call: &Value, input: &Value, stop: &str) -> (Value, bool) {
        let fail = |e: String| (result_block(call, json!(e), true), false);
        let kit = input["name"].as_str().unwrap_or("").trim().to_uppercase();
        let (agent, label, conv, mode, loaded) = {
            let c = self.conv.lock().unwrap();
            let agent = c.parent.is_some();
            let label = agent.then(|| agent_label(c.agent_id, &c.title));
            (agent, label, c.id.clone(), crate::prefix::mode(c.provider(), &c.model), c.prefix.kits.contains(&kit))
        };
        let kits = tools::kits_for(agent);
        if !kits.contains(&kit.as_str()) {
            return fail(format!("No kit {kit} here: you can load {}.", kits.join(", ")));
        }
        if let Some(inner) = input.get("input").filter(|v| !v.is_null()) {
            if !loaded {
                return fail(format!("Load {kit} first (LOAD without `input`)."));
            }
            if kit == "WINDOW" {
                return fail("WINDOW is instructions only: it has no tool to call.".into());
            }
            return self.run_call(&json!({"id": call["id"], "name": kit, "input": inner}), stop);
        }
        if kit == "BROWSER" && !crate::browser::is_open(&conv) {
            return fail(crate::browser::CLOSED.into());
        }
        if loaded {
            return (result_block(call, json!(format!("{kit} is loaded already.")), false), false);
        }
        {
            let mut c = self.conv.lock().unwrap();
            c.prefix.kits.push(kit.clone());
            c.save();
        }
        let tool = match mode {
            _ if kit == "WINDOW" => String::new(),
            Some(crate::prefix::Mode::Generic) => format!(
                "Its tool isn't in your tool list until Oak next renews the list: until then, call it through LOAD, with name={kit} and the tool's input in `input`, e.g. {{\"name\": \"{kit}\", \"input\": {{\"action\": \"...\"}}}}.\n{}",
                tools::kit_help(&kit, agent)
            ),
            _ => format!("The {kit} tool is in your tool list now."),
        };
        let text = format!("Loaded {kit}. {tool}\n\n{}", super::prompt::kit_prompt(&kit, label.as_deref()));
        (result_block(call, json!(text.trim()), false), false)
    }

    /// A BROWSER call (`canon`: its action's tool and input, if the action
    /// is one). With the pane closed every call fails the same way, help
    /// too, so the model learns nothing of the browser until the user opens
    /// it; with it open, help (or a call without a known action) gives the
    /// actions.
    fn browser_call(&self, call: &Value, canon: Option<(&str, &Value)>) -> (Value, bool) {
        let (conv, folder, agent) = {
            let c = self.conv.lock().unwrap();
            (c.id.clone(), c.folder.clone(), c.parent.is_some())
        };
        let fail = |e: String| (result_block(call, json!(e), true), false);
        if agent {
            return fail("BROWSER isn't available to a subagent.".into());
        }
        if !crate::browser::is_open(&conv) {
            return fail(crate::browser::CLOSED.into());
        }
        let Some((name, input)) = canon.filter(|(n, _)| *n != "BROWSER_HELP") else {
            let help = tools::browser_help();
            return match canon {
                Some(_) => (result_block(call, json!(help), false), false),
                None => fail(format!("Give `action`, one of these.\n{help}")),
            };
        };
        if let Err(e) = tools::validate(name, input) {
            return fail(format!("Invalid input: {e}"));
        }
        if name == "BROWSER_SCREENSHOT" {
            let (r, interrupted) = self.abandonable("Screenshot", move || crate::browser::screenshot(&conv));
            return match r {
                Ok(b) => (result_block(call, json!(b), false), interrupted),
                Err(e) => (result_block(call, json!(e), true), interrupted),
            };
        }
        let (name, input) = (name.to_string(), input.clone());
        let (r, interrupted) = self.abandonable("The browser call", move || crate::browser::tool(&conv, &name, &input, &folder));
        match r {
            Ok(s) => (result_block(call, json!(s), false), interrupted),
            Err(e) => (result_block(call, json!(e), true), interrupted),
        }
    }

    /// Runs a tool that changes the conversation, then saves it.
    fn edit(&self, f: impl FnOnce(&mut Conversation) -> Result<String, String>) -> Result<String, String> {
        let mut c = self.conv.lock().unwrap();
        let r = f(&mut c);
        self.save(c);
        r
    }

    /// Runs a tool. The bool is true when the user sent a message.
    #[cfg(test)]
    pub(crate) fn tool(&self, name: &str, input: &Value) -> (Result<String, String>, bool) {
        self.tool_for("", name, input)
    }

    /// Runs a tool for call `call_id` (EXECUTE streams its output to the
    /// call's row). The bool is true when the user sent a message.
    fn tool_for(&self, call_id: &str, name: &str, input: &Value) -> (Result<String, String>, bool) {
        let (folder, agent, spawner) = {
            let c = self.conv.lock().unwrap();
            (c.folder.clone(), c.parent.is_some(), c.spawner)
        };
        if !tools::allowed(name, agent, spawner) {
            return (Err(format!("{} isn't available to a subagent.", tools::call_name(name))), false);
        }
        let id = input["id"].as_u64().unwrap_or(0);
        let result = match name {
            "READ" => tools::read(&folder, input),
            "WRITE" => tools::write(&folder, input),
            "EDIT" => tools::edit(&folder, input),
            "TASK_LIST" => Ok(format!("id\ttime\tstatus\tcommand\n{}", self.tasks.list())),
            "TASK_NEW" => crate::guard::check(input["command"].as_str().unwrap(), &folder).and_then(|_| tasks::spawn(input["command"].as_str().unwrap(), &folder)).map(|p| format!("Started task {}.", self.adopt(&p, input["summary"].as_str().unwrap_or("")))),
            "TASK_GET_NEW_OUTPUT" => self.task(id).map(|p| self.task_output(id, &p, "")),
            "TASK_WAIT" => return self.task_wait(id, input),
            "TASK_DELETE" => match self.tasks.remove(id) {
                None => Err(format!("No task {id}.")),
                Some(p) => {
                    self.mark_reported(id);
                    let how = p.stop();
                    self.tasks.retire(id, &p);
                    Ok(format!("Task {id} {how} after running {} in all, and was removed.", tasks::fmt_took(p.elapsed())))
                }
            },
            "SEARCH" => {
                let input = input.clone();
                return self.abandonable("Search", move || tools::search(&input));
            }
            "FETCH" => {
                let input = input.clone();
                return self.abandonable("Fetch", move || crate::web::fetch(&input));
            }
            "ASK" => return self.ask_user(input),
            "REQUEST_ASK" => self.edit(|c| request_ask(c, input)),
            "NOTE_NEW" | "NOTE_UPDATE" | "NOTE_DELETE" | "HEARTBEAT_START" | "HEARTBEAT_STOP" | "GOAL_JUSTIFICATION" | "GOAL_FINISH" => {
                self.edit(|c| memory_tool(c, name, input, api::now_ms()))
            }
            "PROGRESS_SET" => {
                let r = match self.edit(|c| memory_tool(c, name, input, api::now_ms())) {
                    Ok(r) => r,
                    Err(e) => return (Err(e), false),
                };
                crate::progress::watch(&self.conv, &self.ctx);
                return (Ok(format!("{r}\n{}", self.first_reading(input))), false);
            }
            "PROGRESS_REMOVE" => self.edit(|c| memory_tool(c, name, input, api::now_ms())),
            "EXECUTE" => return self.execute(call_id, input, &folder),
            "AGENT_NEW" => self.agent_new(input),
            "AGENT_SEND" => self.agent_send(id, input["message"].as_str().unwrap_or("")),
            "AGENT_LIST" => Ok(self.agent_list()),
            "AGENT_SET" => self.agent_set(id, input),
            "AGENT_PAUSE" => self.agent_pause(id),
            "AGENT_RESUME" => self.agent_resume(id),
            "ACCOUNTS" => Ok(self.accounts_tool()),
            "AGENT_STOP" => self.agent_stop(id),
            "AGENT_DELETE" => self.agent_delete(id),
            "AGENT_WAIT" => return self.agent_wait(input),
            _ => Err(format!("Unknown tool {name}")),
        };
        (result, false)
    }


    /// After PROGRESS_SET: the meter's first reading, or why it gave none,
    /// if its first run ends within FIRST_READING (so a command that
    /// prints the wrong thing gets fixed at once). An interrupt ends the wait.
    fn first_reading(&self, input: &Value) -> String {
        let label = crate::tools::one_line(input["label"].as_str().unwrap_or("")).to_lowercase();
        let t = Instant::now();
        while t.elapsed() < FIRST_READING && !self.stopping() {
            {
                let c = self.conv.lock().unwrap();
                let Some(m) = c.meters.iter().find(|m| m.label.to_lowercase() == label) else { return String::new() };
                match (&m.value, m.error.as_str()) {
                    (Some(r), _) if r.note.is_empty() => return format!("First reading: {}.", r.value),
                    (Some(r), _) => return format!("First reading: {} ({}).", r.value, r.note),
                    (None, e) if !e.is_empty() => {
                        return format!("The first run gave no value: {e}\nFix the command with PROGRESS action=set (same label).");
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        format!("The first run is still going after {}s; the value shows when it ends.", t.elapsed().as_secs())
    }

    /// A task's state and the output it printed since the last read, as
    /// TASK_GET_NEW_OUTPUT returns them; `waited` goes after the state.
    fn task_output(&self, id: u64, p: &Proc, waited: &str) -> String {
        // Read the exit first: if it is set, the output is complete.
        let exit = p.exit();
        let (out, more) = p.take_new(MAX_OUTPUT);
        let out = if out.is_empty() { "(no new output)\n".to_string() } else { out };
        let more = if more { "\n[more output waiting; call TASK action=output]" } else { "" };
        // How long it ran, as the notice of its end says it.
        let took = tasks::fmt_took(p.elapsed());
        let state = match exit {
            Some(_) => {
                self.mark_reported(id);
                format!("{} after running {took} in all", p.status())
            }
            None => format!("is running ({took} so far)"),
        };
        format!("Task {id} {state}{waited}.\n{out}{more}")
    }

    /// TASK_WAIT: waits for a task to exit, at most the `timeout` the model
    /// gave (up to TASK_WAIT_MAX seconds), then returns its new output. A
    /// message from the user or an interrupt ends the wait early.
    fn task_wait(&self, id: u64, input: &Value) -> (Result<String, String>, bool) {
        let p = match self.task(id) {
            Ok(p) => p,
            Err(e) => return (Err(e), false),
        };
        let timeout = match input["timeout"].as_u64() {
            Some(s @ 1..=TASK_WAIT_MAX) => Duration::from_secs(s),
            _ => return (Err(format!("`timeout` must be from 1 to {TASK_WAIT_MAX} seconds.")), false),
        };
        let t = Instant::now();
        let waiting = Running::new(&self.live, true);
        let (why, interrupted) = loop {
            if p.exit().is_some() {
                break (String::new(), false);
            }
            if self.stopping() {
                break ("; the user sent a message, which ended the wait".to_string(), true);
            }
            if self.live.lock().unwrap().ready_count() > 0 {
                break (format!("; the wait ended after {} because the user sent a message", fmt_duration(t.elapsed())), false);
            }
            if !self.conv.lock().unwrap().inbox.is_empty() {
                break (format!("; the wait ended after {} because a message arrived (it follows)", fmt_duration(t.elapsed())), false);
            }
            if t.elapsed() >= timeout {
                break (format!("; still running after waiting {}s", timeout.as_secs()), false);
            }
            self.wait_a_bit(30);
        };
        drop(waiting);
        (Ok(self.task_output(id, &p, &why)), interrupted)
    }

    fn execute(&self, call_id: &str, input: &Value, folder: &std::path::Path) -> (Result<String, String>, bool) {
        // The model must give the timeout (the schema requires it); a bad
        // one is refused before the command starts. A long sleep (`sleep 60;
        // cat log`) is refused too: it ties the session up however soon
        // what it waits for is ready.
        let command = input["command"].as_str().unwrap();
        let started = execute_timeout(input).and_then(|timeout| {
            crate::sleeps::check(command)?;
            crate::guard::check(command, folder)?;
            Ok((timeout, tasks::spawn(command, folder)?))
        });
        let (timeout, p) = match started {
            Ok(started) => started,
            Err(e) => return (Err(e), false),
        };
        // The output shows under the call's row as it comes in.
        let _streaming = tasks::Streaming::new(call_id, &p);
        let mut shown_lines = 0;
        let t = Instant::now();
        loop {
            if let Some(code) = p.exit() {
                let (out, _) = p.take_new(usize::MAX);
                // How long it ran, when that is worth knowing.
                let ran = p.elapsed();
                let ran = if ran >= Duration::from_secs(1) { format!(", ran {}", tasks::fmt_took(ran)) } else { String::new() };
                // Not clipped: a long output is saved whole (see run_call).
                return (Ok(format!("{}\n[exit code {code}{ran}]", out.trim_end())), false);
            }
            if self.ultra() {
                // Ultrapause: killed, not moved to the background.
                p.kill();
                let (out, _) = p.take_new(MAX_OUTPUT);
                let msg = format!(
                    "Output so far:\n{}\n\nThe user stopped everything (ultrapause), so the command was killed after {}.",
                    if out.trim().is_empty() { "(none)" } else { out.trim_end() },
                    tasks::fmt_took(t.elapsed())
                );
                return (Err(msg), true);
            }
            let interrupted = self.interrupted();
            if interrupted || t.elapsed() > timeout {
                let (out, more) = p.take_new(MAX_OUTPUT);
                let id = self.adopt(&p, input["summary"].as_str().unwrap_or(""));
                let took = tasks::fmt_took(t.elapsed());
                let why = if interrupted {
                    format!("The user sent a message after the command had run for {took}, so it was moved to the background")
                } else {
                    format!("The command was still running after {took} (timeout {}s), so it was moved to the background", timeout.as_secs())
                };
                let more = if more { " More output is already waiting." } else { "" };
                let msg = format!(
                    "Output so far:\n{}\n\n{why}: it is now task {id} and still running.{more} Use TASK action=output with id {id} to read new output; you will be notified when it finishes.",
                    if out.is_empty() { "(none)" } else { out.trim_end() }
                );
                return (Ok(msg), interrupted);
            }
            // New output: a frame for the row, unless the transcript isn't
            // being drawn (it catches up when it is).
            if let Some(n) = tasks::streaming_len(call_id).filter(|&n| n != shown_lines) {
                shown_lines = n;
                let drawn = self.live.lock().unwrap().drawn;
                if drawn + 1 >= self.ctx.cumulative_pass_nr_for(eframe::egui::ViewportId::ROOT) {
                    self.ctx.request_repaint();
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

/// How long PROGRESS_SET waits for the meter's first reading.
const FIRST_READING: Duration = Duration::from_secs(15);

/// EXECUTE's `timeout`, if it is in range (EXECUTE_TIMEOUT_MIN to _MAX).
pub(crate) fn execute_timeout(input: &Value) -> Result<Duration, String> {
    match input["timeout"].as_u64() {
        Some(s @ tools::EXECUTE_TIMEOUT_MIN..=tools::EXECUTE_TIMEOUT_MAX) => Ok(Duration::from_secs(s)),
        _ => Err(format!("`timeout` must be from {} to {} seconds.", tools::EXECUTE_TIMEOUT_MIN, tools::EXECUTE_TIMEOUT_MAX)),
    }
}

/// While a goal is active, ASK needs the user's leave: a REQUEST_ASK they
/// accepted, which this ASK uses up. Without a goal, ASK always works.
pub(crate) fn ask_gate(c: &mut Conversation) -> Result<(), String> {
    if c.goal.is_empty() || std::mem::take(&mut c.ask_granted) {
        return Ok(());
    }
    let waiting = if c.ask_request.is_empty() {
        "Call REQUEST_ASK with what you want to ask the user about, and keep working on the goal".to_string()
    } else {
        format!("Your REQUEST_ASK (about: {}) is still waiting for the user, so keep working on the goal", c.ask_request)
    };
    Err(format!("ASK isn't available while a goal is active. {waiting}: if the user accepts, a message tells you so, and you may then call ASK once."))
}

/// REQUEST_ASK: shows the user what the model wants to ask them about,
/// for them to accept or decline (`Session::answer_ask_request`). It
/// returns at once.
fn request_ask(c: &mut Conversation, input: &Value) -> Result<String, String> {
    let about = input["about"].as_str().unwrap_or("").trim();
    if about.is_empty() {
        return Err("`about` is empty: say what you want to ask the user about.".into());
    }
    if c.goal.is_empty() {
        return Err("No goal is active, so you don't need REQUEST_ASK: call ASK directly.".into());
    }
    if c.ask_granted {
        return Err("The user already accepted a request: you may call ASK once now.".into());
    }
    if !c.ask_request.is_empty() {
        return Err(format!("A request is already waiting for the user (about: {}). You'll get a message when they answer it.", c.ask_request));
    }
    c.ask_request = about.to_string();
    Ok("The request is shown to the user. Keep working on the goal meanwhile: if they accept, a message tells you so (with your next request), and you may then call ASK once. If they decline, a message tells you that instead.".into())
}

/// The stop reason of a reply cut short (interrupted, paused, broken off,
/// refused) after some of its tool calls had started: it is kept up to
/// the last of them.
pub(crate) const CUT_SHORT: &str = "cut_short";

/// A reply's tool calls, as many at once as can be (see `waits_for`), each
/// on its own thread of the turn's scope. Each starts as soon as it has
/// streamed in and the calls it waits for are done, while the rest of the
/// reply still comes in
/// (ASK and a call whose input didn't parse wait for the reply's end);
/// their results join the transcript once the reply is in it (`finish`),
/// and the model gets them all in one go, as always.
pub(crate) struct Batch<'scope, 'env> {
    w: &'env Worker,
    scope: &'scope std::thread::Scope<'scope, 'env>,
    calls: Vec<Value>,
    pub(super) started: Vec<bool>,
    running: Vec<Option<std::thread::ScopedJoinHandle<'scope, Value>>>,
    results: Vec<Option<Value>>,
    /// The user sent a message: no more calls start.
    stopped: Arc<AtomicBool>,
    done_tx: Sender<usize>,
    done_rx: Receiver<usize>,
    /// A call came in or ended since the last `pump`.
    dirty: bool,
}

impl<'scope, 'env> Batch<'scope, 'env> {
    pub(crate) fn new(w: &'env Worker, scope: &'scope std::thread::Scope<'scope, 'env>) -> Self {
        let (done_tx, done_rx) = channel();
        w.live.lock().unwrap().early = Some(vec![]);
        Batch { w, scope, calls: vec![], started: vec![], running: vec![], results: vec![], stopped: Arc::default(), done_tx, done_rx, dirty: false }
    }

    /// A call that has streamed in whole.
    pub(crate) fn add(&mut self, call: Value) {
        self.calls.push(call);
        self.dirty = true;
        self.started.push(false);
        self.running.push(None);
        self.results.push(None);
    }

    pub(crate) fn any_started(&self) -> bool {
        self.started.iter().any(|&s| s)
    }

    /// A reply cut short: its blocks up to the last call that started (the
    /// ones after it, and any not whole, dropped).
    pub(crate) fn kept(&self, blocks: Vec<Value>) -> Vec<Value> {
        let started: Vec<&Value> = self.calls.iter().zip(&self.started).filter(|(_, s)| **s).map(|(c, _)| &c["id"]).collect();
        let last = blocks.iter().rposition(|b| oak::is_call(b) && started.contains(&&b["id"]));
        let mut kept: Vec<Value> = blocks.into_iter().take(last.map_or(0, |i| i + 1)).filter(|b| b.get("_oak_partial").is_none()).collect();
        // A call that started has its input as it ran.
        for b in &mut kept {
            if let Some(c) = self.calls.iter().find(|c| oak::is_call(b) && c["id"] == b["id"]) {
                *b = c.clone();
            }
        }
        kept
    }

    /// Takes in the calls that are done, then starts those that may start
    /// now: those whose calls they wait for (`waits_for`) are done (`last`:
    /// the reply is complete, so all may; otherwise not ASK, nor a call
    /// whose input didn't parse). Once the user sent a message, none
    /// start, and at the end the rest are not run; nor are they when the
    /// reply was cut short (`stop`).
    pub(crate) fn pump(&mut self, last: bool, stop: &str) {
        while let Ok(i) = self.done_rx.try_recv() {
            self.reap(i);
        }
        if !std::mem::take(&mut self.dirty) && !last {
            return;
        }
        let folder = self.w.conv.lock().unwrap().folder.clone();
        let stopped = self.stopped.load(Ordering::SeqCst);
        let mut starting = vec![];
        for (i, deps) in waits_for(&self.calls, &folder).into_iter().enumerate() {
            if self.started[i] {
                continue;
            }
            if last && (stopped || stop == CUT_SHORT) {
                let why = if stopped { "Not run: the user sent a message." } else { "Not run: the reply was cut short before this call started." };
                self.results[i] = Some(self.w.not_run(&self.calls[i], why));
                self.started[i] = true;
            } else if !stopped && (last || early_ok(&self.calls[i])) && deps.iter().all(|&j| self.results[j].is_some()) {
                self.start(i, stop);
                starting.push(i);
            }
        }
        if last && !starting.is_empty() {
            let names: Vec<&str> = starting.iter().map(|&i| self.calls[i]["name"].as_str().unwrap_or("")).collect();
            self.w.set(|l| l.status = format!("Running {}", names.join(", ")));
        }
    }

    fn start(&mut self, i: usize, stop: &str) {
        let (w, call, stop, stopped, done) = (self.w, self.calls[i].clone(), stop.to_string(), self.stopped.clone(), self.done_tx.clone());
        self.started[i] = true;
        self.running[i] = Some(self.scope.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| w.run_traced(&call, &stop, &stopped)))
                .unwrap_or_else(|_| result_block(&call, json!("The tool failed unexpectedly (Oak caught a crash)."), true));
            w.deliver(r.clone());
            let _ = done.send(i);
            r
        }));
    }

    fn reap(&mut self, i: usize) {
        self.dirty = true;
        if let Some(h) = self.running[i].take() {
            self.results[i] = h.join().ok();
        }
    }

    /// The reply is complete (and in the transcript): `calls` are its tool
    /// calls (those not yet in the batch join it). Runs them all, then puts
    /// their results in the calls' order (the API pairs them by ID, but the
    /// transcript reads better this way). True when the user sent a message.
    pub(crate) fn finish(mut self, calls: &[Value], stop: &str) -> bool {
        for c in calls {
            if !self.calls.iter().any(|b| b["id"] == c["id"]) {
                self.add(c.clone());
            }
        }
        self.w.release_early();
        loop {
            self.pump(true, stop);
            if self.results.iter().all(Option::is_some) {
                break;
            }
            match self.done_rx.recv() {
                Ok(i) => self.reap(i),
                Err(_) => break,
            }
        }
        let ordered: Vec<Value> = std::mem::take(&mut self.results).into_iter().flatten().collect();
        let mut c = self.w.conv.lock().unwrap();
        if let Some(last) = c.messages.last_mut().filter(|m| m["role"] == "user") {
            let ids: Vec<&Value> = ordered.iter().map(|r| &r["call_id"]).collect();
            let others: Vec<Value> = last["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| !(b["type"] == "tool_result" && ids.contains(&&b["call_id"])))
                .cloned()
                .collect();
            last["content"] = Value::Array(ordered.iter().cloned().chain(others).collect());
            c.save();
        }
        drop(c);
        self.w.ctx.request_repaint();
        self.stopped.load(Ordering::SeqCst)
    }
}

/// Results held for a reply that never made it to the transcript go to it
/// anyway (rather than being lost).
impl Drop for Batch<'_, '_> {
    fn drop(&mut self) {
        self.w.release_early();
    }
}

/// Whether a call may start while its reply still streams in: not ASK
/// (the user is asked once the reply is done), nor one whose input didn't
/// parse (the reply's end says why).
fn early_ok(call: &Value) -> bool {
    call["name"] != "ASK" && call.get("_oak_raw").is_none()
}

/// The goal changed or ended: a REQUEST_ASK waiting for the user, or one
/// they accepted, goes with it.
pub(crate) fn end_ask_request(c: &mut Conversation) {
    c.ask_request.clear();
    c.ask_granted = false;
}
/// For each of a reply's tool calls, the calls before it that it waits
/// for; the rest run at the same time. ASK waits for all before it, and
/// all after it wait for it (it runs on its own); a call on a file waits
/// for the calls before it on that file when one of them writes it
/// (WRITE, EDIT, a SCREENSHOT saved there). Calls on one file thus run in
/// the order they were made.
pub(crate) fn waits_for(calls: &[Value], folder: &Path) -> Vec<Vec<usize>> {
    let touches: Vec<(Vec<String>, bool, bool)> = calls
        .iter()
        .map(|c| {
            let name = c["name"].as_str().unwrap_or("");
            // Windows paths ignore case. DISPLAY reads several.
            let mut paths: Vec<String> = c["input"]["path"].as_str().into_iter().map(|p| tools::path_key(&tools::resolve(folder, p))).collect();
            if name == "DISPLAY" {
                let more = c["input"]["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str());
                paths.extend(more.map(|p| tools::path_key(&tools::resolve(folder, p.trim()))));
            }
            // The browser's calls act on one page: in the order they were made.
            if name == "BROWSER" {
                paths.push("\0browser".into());
            }
            (paths, matches!(name, "WRITE" | "EDIT" | "SCREENSHOT" | "BROWSER"), name == "ASK")
        })
        .collect();
    (0..calls.len())
        .map(|i| {
            let (paths, writes, ask) = &touches[i];
            (0..i)
                .filter(|&j| {
                    let (earlier, wrote, asked) = &touches[j];
                    *ask || *asked || ((*writes || *wrote) && paths.iter().any(|p| earlier.contains(p)))
                })
                .collect()
        })
        .collect()
}
