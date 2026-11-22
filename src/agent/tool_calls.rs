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

    /// The TASK waits Oak closed on (`open_waits`) run again, first thing,
    /// for what was left of their timeouts: the session is busy with them
    /// as with any call, paused or not, and once they end the turn carries
    /// on (when resumed, if paused), as it would have without the restart.
    pub(super) fn carry_on_waits(&self) {
        let calls = std::mem::take(&mut *self.open_waits.lock().unwrap());
        if calls.is_empty() {
            return;
        }
        self.set(|l| l.busy = true);
        std::thread::scope(|s| Batch::new(self, s).finish(&calls, ""));
        self.resume.store(true, Ordering::SeqCst);
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

    /// READ on an image file, SCREENSHOT, or SEARCH for images: calls whose result is an image.
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
            "SEARCH" if tools::wants_images(input) => {
                // The images' index and those seen in full live in the scratchpad.
                let dir = scratchpad(&mut self.conv.lock().unwrap()).join("images");
                let input = input.clone();
                Some(self.abandonable("The image search", move || tools::images(&input, &dir)))
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

    /// ASK: shows the questions. While a goal is active it returns at once
    /// and the model carries on: the questions wait with the conversation
    /// (`Conversation::pending_ask`) and the answers come as a message
    /// (`Session::answer_questions`). Otherwise it waits for the answers,
    /// or for the user to skip them, and messages written meanwhile stay
    /// queued until then. Either way it replaces questions of an earlier
    /// ASK still unanswered, and says so.
    fn ask_user(&self, input: &Value) -> (Result<String, String>, bool) {
        let ask = match Ask::new(input) {
            Ok(ask) => ask,
            Err(e) => return (Err(e), false),
        };
        let (goal, replaced) = {
            let mut c = self.conv.lock().unwrap();
            let goal = goal_active(&c);
            let replaced = std::mem::take(&mut c.pending_ask);
            if goal {
                c.pending_ask = json!({"questions": input["questions"]});
            }
            if goal || !replaced.is_null() {
                c.save();
            }
            (goal, Ask::new(&replaced).ok())
        };
        let replaced = replaced.map_or(String::new(), |r| {
            format!(
                "\n\nThis ASK replaced your earlier one, which the user hadn't answered (they no longer see it):\n{}If you still want those answered too, call ASK again with the full list of questions you want answered: each ASK replaces the one before.",
                r.listed()
            )
        });
        if goal {
            self.set(|l| l.ask = Some(Ask { background: true, ..ask }));
            return (Ok(format!("{ASKED_DURING_GOAL}{replaced}")), false);
        }
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
                    return (Ok(format!("{answers}{replaced}")), false);
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
                    return (Ok(format!("{text}{replaced}")), false);
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
    pub(super) fn track(&self, name: &str, input: &Value) {
        if let ("READ" | "WRITE" | "EDIT", Some(p)) = (name, input["path"].as_str()) {
            let folder = self.conv.lock().unwrap().folder.clone();
            self.files.lock().unwrap().record(&tools::resolve(&folder, p));
        }
    }

    /// Why a chat (`Conversation::chat`) won't run tool `name` (as OAK
    /// runs it, or a family's name when its action is invalid): any tool
    /// not in `tools::CHAT_TOOLS`. Checked whatever the model was given, so
    /// no call reaches the user's files or runs anything on the machine.
    fn chat_refuses(&self, name: &str) -> Option<String> {
        let chat = self.conv.lock().unwrap().chat;
        (chat && !tools::chat_allows(name) && name != "NOTE").then(|| {
            format!("{} isn't available in a chat: it has only web search, fetch, ASK and notes, no files, commands or other agents.", tools::call_name(name))
        })
    }

    /// Why a Code Mode conversation (`Conversation::code_mode`) won't run a
    /// call made directly, not from a REPL script (`repl::FROM_SCRIPT`).
    fn code_mode_refuses(&self, call: &Value) -> Option<String> {
        let code_mode = self.conv.lock().unwrap().code_mode;
        (code_mode && call.get(super::repl::FROM_SCRIPT).is_none()).then(|| {
            let name = call["name"].as_str().unwrap_or("");
            format!("In Code Mode your one tool is REPL: call {name} from a script there, as `await {name}({{...}})`.")
        })
    }

    /// Runs one tool call and returns its tool_result block, and whether
    /// the user sent a message.
    pub(super) fn run_call(&self, call: &Value, stop: &str) -> (Value, bool) {
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
        } else if let Some(e) = self.chat_refuses(name) {
            // First: a chat runs nothing else, whatever the call is.
            (e, true, false)
        } else if call["name"] == "REPL" {
            return self.repl_call(call, stop);
        } else if let Some(e) = self.code_mode_refuses(call) {
            // Code Mode runs the rest from a script only.
            (e, true, false)
        } else if name == "LOAD" {
            return self.load(call, input, stop);
        } else if let Some(e) = self.unloaded(name) {
            (e, true, false)
        } else if name.starts_with("mcp__") || crate::mcp::split(name).is_some() {
            return self.mcp_call(call, name, input);
        } else if call["name"] == "BROWSER" || name.starts_with("BROWSER_") {
            return self.browser_call(call, canon.as_ref().ok().map(|(n, i)| (n.as_str(), i)));
        } else if cfg!(windows) && (call["name"] == "WINDOW" || name.starts_with("WINDOW_")) && canon.is_ok() {
            return self.window_call(call, name, input);
        } else if name == "REQUEST_ASK" {
            (REQUEST_ASK_GONE.to_string(), true, false)
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
        // A protocol's tool (BLENDER, BLENDER_SCENE_INFO, or an older Oak's
        // mcp__blender__get_scene_info).
        if name.starts_with("mcp__") || crate::mcp::split(name).is_some() || crate::mcp::is_family(name) {
            let server = crate::mcp::resolve_tool(name).map(|(c, _)| c).or_else(|| crate::mcp::named(name).filter(|c| !c.native()));
            let Some(server) = server else {
                return Some(format!("No protocol has a tool {name}."));
            };
            let c = self.conv.lock().unwrap();
            if c.chat {
                return Some(format!("{} isn't available in a chat.", server.family));
            }
            return (!crate::mcp::loaded_in(&c.prefix.kits, &server)).then(|| format!("{} needs its kit: call LOAD with name={} first.", tools::call_name(name), server.family));
        }
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
        if let Some(server) = crate::mcp::find(input["name"].as_str().unwrap_or("")).filter(|_| !crate::local::basic(&self.conv.lock().unwrap().model)) {
            return self.load_server(call, &server, input);
        }
        let kit = input["name"].as_str().unwrap_or("").trim().to_uppercase();
        let (agent, label, mode, loaded) = {
            let c = self.conv.lock().unwrap();
            let agent = c.parent.is_some();
            let label = agent.then(|| agent_label(c.agent_id, &c.title));
            (agent, label, crate::prefix::mode(c.provider(), &c.model), c.prefix.kits.contains(&kit))
        };
        let kits = tools::kits_for(agent);
        if !kits.contains(&kit.as_str()) {
            let servers: Vec<String> = crate::mcp::kits().into_iter().map(|(n, _)| n).collect();
            let all: Vec<String> = kits.iter().map(|k| k.to_string()).chain(servers).collect();
            return fail(format!("No kit {kit} here: you can load {}.", all.join(", ")));
        }
        if let Some(inner) = input.get("input").filter(|v| !v.is_null()) {
            if !loaded {
                return fail(format!("Load {kit} first (LOAD without `input`)."));
            }
            if kit == "WINDOW" && !cfg!(windows) {
                return fail("WINDOW is instructions only: it has no tool to call.".into());
            }
            return self.run_call(&json!({"id": call["id"], "name": kit, "input": inner}), stop);
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
            _ if kit == "WINDOW" && !cfg!(windows) => String::new(),
            Some(crate::prefix::Mode::Generic) => format!(
                "Its tool isn't in your tool list: {}, with name={kit} and the tool's input in `input`, e.g. {{\"name\": \"{kit}\", \"input\": {{\"action\": \"...\"}}}}.\n{}",
                crate::prefix::THROUGH_LOAD,
                tools::kit_help(&kit, agent)
            ),
            _ => format!("The {kit} tool is in your tool list now."),
        };
        let text = format!("Loaded {kit}. {tool}\n\n{}", super::prompt::kit_prompt(&kit, label.as_deref()));
        (result_block(call, json!(text.trim()), false), false)
    }

    /// LOAD for an MCP server: connects to it (starting it), adds its kit
    /// (its tool joins the list as Oak's kits' do) and returns its tool and
    /// instructions. With `input` (the tool's: {"action", ...}, or an older
    /// Oak's {"tool", "arguments"}), calls its tool through LOAD, where
    /// tools can't join the list mid-conversation.
    fn load_server(&self, call: &Value, server: &crate::mcp::Config, input: &Value) -> (Value, bool) {
        let fail = |e: String| (result_block(call, json!(e), true), false);
        let kit = crate::mcp::kit_id(server);
        let (mode, loaded, chat) = {
            let c = self.conv.lock().unwrap();
            (crate::prefix::mode(c.provider(), &c.model), crate::mcp::loaded_in(&c.prefix.kits, server), c.chat)
        };
        if chat {
            return fail(format!("{kit} isn't available in a chat."));
        }
        if let Some(inner) = input.get("input").filter(|v| !v.is_null()) {
            if !loaded {
                return fail(format!("Load {kit} first (LOAD without `input`)."));
            }
            if let Some(tool) = inner["tool"].as_str().filter(|_| inner.get("action").is_none()) {
                let tool = tool.rsplit("__").next().unwrap_or(tool);
                let Some(action) = crate::mcp::offer(server).and_then(|o| crate::mcp::actions(server, &o).into_iter().find(|(_, t)| t["name"] == tool)).map(|(a, _)| a) else {
                    return fail(format!("{kit} has no tool {tool}."));
                };
                let args = inner.get("arguments").cloned().unwrap_or_else(|| json!({}));
                return self.mcp_call(call, &format!("{kit}_{}", action.to_uppercase()), &args);
            }
            return match crate::mcp::canonical(&kit, inner) {
                Some(Ok((name, args))) => self.mcp_call(call, &name, &args),
                Some(Err(e)) => fail(format!("Invalid input: {e}")),
                None => fail(format!("{kit} lists no tools.")),
            };
        }
        let name = server.name.clone();
        let (r, interrupted) = self.abandonable(&format!("Starting {}", server.title), move || crate::mcp::connect(&name));
        if let Err(e) = r {
            return (result_block(call, json!(format!("Couldn't load {kit}: {e}")), true), interrupted);
        }
        if !loaded {
            let mut c = self.conv.lock().unwrap();
            c.prefix.kits.push(kit.clone());
            c.save();
        }
        let through_load = mode == Some(crate::prefix::Mode::Generic);
        let mut text = crate::mcp::load_text(server, through_load);
        if matches!(server.transport, crate::mcp::Transport::Builtin("blender")) {
            let caller = self.blender_caller();
            let canceled = caller.cancelled.clone();
            let (r, interrupted) = self.abandonable("Starting your private Blender", move || crate::mcp::blender_run::with_caller(caller, || crate::mcp::blender_run::use_own(None, None).map(Some)));
            if interrupted { canceled.store(true, std::sync::atomic::Ordering::SeqCst); }
            match r {
                Ok(Some(note)) => text.push_str(&format!("\n\n{note}")),
                Ok(None) => text.push_str("\n\nYour private Blender is ready."),
                Err(e) => return (result_block(call, json!(format!("{text}\n\nThe kit is loaded, but its private Blender could not start: {e}\nIf Blender is portable, use BLENDER action=use_own with program pointing to its executable.")), true), interrupted),
            }
        }
        if loaded {
            text = format!("{kit} is loaded already (connected again).\n\n{text}");
        }
        (result_block(call, json!(text), false), false)
    }

    fn blender_caller(&self) -> crate::mcp::blender_run::Caller {
        let mut c = self.conv.lock().unwrap();
        let scratch = scratchpad(&mut c);
        crate::mcp::blender_run::Caller { conv: c.id.clone(), folder: c.folder.clone(), scratch, ..Default::default() }
    }

    /// A call of a protocol's tool (`name`: BLENDER_SCENE_INFO, or an older
    /// Oak's mcp__blender__get_scene_info): its result as the server gave
    /// it (text, pictures), long text saved to the scratchpad as EXECUTE's
    /// is. The user's message interrupts it.
    fn mcp_call(&self, call: &Value, name: &str, input: &Value) -> (Value, bool) {
        let fail = |e: String| (result_block(call, json!(e), true), false);
        let Some((server, tool)) = crate::mcp::resolve_tool(name) else {
            return fail(format!("No protocol has a tool {name}."));
        };
        if !crate::mcp::enabled(&server.name) {
            return fail(format!("The user switched {} off (Home > Protocols).", server.family));
        }
        let args = match crate::mcp::arguments(&server, &tool, input) {
            Ok(a) => a,
            Err(e) => return fail(format!("Invalid input: {e}")),
        };
        let caller = self.blender_caller();
        let canceled = caller.cancelled.clone();
        let (r, interrupted) = self.abandonable(&format!("{} ({})", tools::shown_name(name), server.title), move || crate::mcp::blender_run::with_caller(caller, || crate::mcp::call(&server, &tool, args)));
        if interrupted { canceled.store(true, std::sync::atomic::Ordering::SeqCst); }
        let (blocks, is_error) = match r {
            Ok(r) => r,
            Err(e) => return (result_block(call, json!(e), true), interrupted),
        };
        let only_text = blocks.iter().all(|b| b["type"] == "text");
        let content = if only_text {
            let text: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
            let text = text.join("\n");
            let text = if text.is_empty() { "(no output)".to_string() } else { text };
            let text = if tools::too_large(&text) {
                let dir = scratchpad(&mut self.conv.lock().unwrap());
                tools::persist(&text, &dir)
            } else {
                text
            };
            json!(text)
        } else {
            json!(blocks)
        };
        (result_block(call, content, is_error), interrupted)
    }

    /// A WINDOW call (`name`: its action's tool, WINDOW_CLICK…): runs on
    /// the hidden desktop (`window_tool.rs`); a shot's picture comes back.
    fn window_call(&self, call: &Value, name: &str, input: &Value) -> (Value, bool) {
        #[cfg(windows)]
        {
            if let Err(e) = tools::validate(name, input) {
                return (result_block(call, json!(format!("Invalid input: {e}")), true), false);
            }
            let (conv, folder, scratch) = {
                let mut c = self.conv.lock().unwrap();
                let scratch = scratchpad(&mut c);
                (c.id.clone(), c.folder.clone(), scratch)
            };
            let (name, input) = (name.to_string(), input.clone());
            let label = tools::shown_name(&name);
            let conv2 = conv.clone();
            if name == "WINDOW_ATTACH" {
                self.set(|l| l.status = "Waiting for your permission".into());
            }
            let (r, interrupted) = self.abandonable(&label, move || crate::window_tool::run(&conv, std::path::Path::new(&folder), &scratch, &name, &input));
            if interrupted {
                crate::approval::cancel(&conv2);
            }
            match r {
                Ok(blocks) if blocks.len() == 1 && blocks[0]["type"] == "text" => (result_block(call, blocks[0]["text"].clone(), false), interrupted),
                Ok(blocks) => (result_block(call, json!(blocks), false), interrupted),
                Err(e) => (result_block(call, json!(e), true), interrupted),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (name, input);
            (result_block(call, json!("WINDOW's tool is Windows only."), true), false)
        }
    }

    /// A BROWSER call (`canon`: its action's tool and input, if the action
    /// is one). Open starts the conversation's browser and close ends it;
    /// the other actions fail while it is closed. Help (or a call without a
    /// known action) gives the actions.
    fn browser_call(&self, call: &Value, canon: Option<(&str, &Value)>) -> (Value, bool) {
        let (conv, folder, agent) = {
            let c = self.conv.lock().unwrap();
            (c.id.clone(), c.folder.clone(), c.parent.is_some())
        };
        let fail = |e: String| (result_block(call, json!(e), true), false);
        if agent {
            return fail("BROWSER isn't available to a subagent.".into());
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
        match name {
            "BROWSER_OPEN" => {
                let (ctx, input) = (self.ctx.clone(), input.clone());
                let (r, interrupted) = self.abandonable("Opening the browser", move || crate::browser::model_open(&conv, &ctx, &input, &folder));
                return match r {
                    Ok(s) => (result_block(call, json!(s), false), interrupted),
                    Err(e) => (result_block(call, json!(e), true), interrupted),
                };
            }
            "BROWSER_ATTACH" => {
                let (ctx, input, conv2) = (self.ctx.clone(), input.clone(), conv.clone());
                self.set(|l| l.status = "Waiting for your permission".into());
                let (r, interrupted) = self.abandonable("Waiting for your permission", move || crate::browser::model_attach(&conv, &ctx, &input));
                if interrupted {
                    crate::approval::cancel(&conv2);
                }
                return match r {
                    Ok(s) => (result_block(call, json!(s), false), interrupted),
                    Err(e) => (result_block(call, json!(e), true), interrupted),
                };
            }
            "BROWSER_CLOSE" => {
                return match crate::browser::model_close(&conv) {
                    Ok(s) => (result_block(call, json!(s), false), false),
                    Err(e) => fail(e),
                };
            }
            "BROWSER_PROFILES" => {
                let input = input.clone();
                let (r, interrupted) = self.abandonable("The browser call", move || crate::browser::model_profiles(&conv, &input));
                return match r {
                    Ok(s) => (result_block(call, json!(s), false), interrupted),
                    Err(e) => (result_block(call, json!(e), true), interrupted),
                };
            }
            _ => {}
        }
        if !crate::browser::is_open(&conv) {
            return fail(crate::browser::CLOSED.into());
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
    pub(super) fn tool_for(&self, call_id: &str, name: &str, input: &Value) -> (Result<String, String>, bool) {
        let (folder, agent, spawner) = {
            let c = self.conv.lock().unwrap();
            (c.folder.clone(), c.parent.is_some(), c.spawner)
        };
        if !tools::allowed(name, agent, spawner) {
            return (Err(format!("{} isn't available to a subagent.", tools::call_name(name))), false);
        }
        if let Some(e) = self.chat_refuses(name) {
            return (Err(e), false);
        }
        let id = input["id"].as_u64().unwrap_or(0);
        let result = match name {
            "READ" => tools::read(&folder, input),
            "WRITE" => tools::write(&folder, input),
            "EDIT" => tools::edit(&folder, input),
            "TASK_LIST" => Ok(format!("id\ttime\tstatus\tcommand\n{}", self.tasks.list())),
            "TASK_NEW" => crate::guard::check(input["command"].as_str().unwrap(), &folder).and_then(|_| tasks::spawn(input["command"].as_str().unwrap(), &folder)).map(|p| format!("Started task {}.", self.adopt(&p, input["summary"].as_str().unwrap_or("")))),
            "TASK_GET_NEW_OUTPUT" => self.task(id).map(|p| self.task_output(id, &p, "", "")),
            "TASK_WAIT" => return self.task_wait(call_id, id, input),
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
            "CD" => self.edit(|c| change_folder(c, input, &instructions::home())),
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
            "AGENT_PAUSE" | "AGENT_RESUME" if input.get("model").is_some_and(|m| !m.is_null()) => {
                if input.get("id").is_some_and(|i| !i.is_null()) {
                    Err("Give `id` or `model`, not both.".into())
                } else {
                    self.agent_group(name == "AGENT_PAUSE", input["model"].as_str().unwrap_or(""))
                }
            }
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
    /// TASK_GET_NEW_OUTPUT returns them; `waited` goes after the state, and
    /// `after` after its sentence.
    fn task_output(&self, id: u64, p: &Proc, waited: &str, after: &str) -> String {
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
        let after = if after.is_empty() { String::new() } else { format!(" {after}") };
        format!("Task {id} {state}{waited}.{after}\n{out}{more}")
    }

    /// TASK_WAIT: waits for a task to exit, at most the `timeout` the model
    /// gave (up to TASK_WAIT_MAX seconds), then returns its new output. A
    /// message from the user or an interrupt ends the wait early. Its time
    /// counts only while Oak runs: closing Oak with the background tasks
    /// left running doesn't end it, and it carries on at the next start
    /// (`carry_on_waits`) for the rest of its timeout.
    fn task_wait(&self, call_id: &str, id: u64, input: &Value) -> (Result<String, String>, bool) {
        let p = match self.task(id) {
            Ok(p) => p,
            Err(e) => return (Err(e), false),
        };
        let timeout = match input["timeout"].as_u64() {
            Some(s @ 1..=TASK_WAIT_MAX) => Duration::from_secs(s),
            _ => return (Err(format!("`timeout` must be from 1 to {TASK_WAIT_MAX} seconds.")), false),
        };
        // What it waited before (an Oak that closed on it), and since when.
        let before = self.begin_wait(call_id);
        let prior = Duration::from_millis((before.waited_ms + (api::now_ms() - before.since)).max(0) as u64);
        let t = Instant::now();
        let waited = || prior + t.elapsed();
        let waiting = Running::new(&self.live, true);
        let (why, interrupted) = loop {
            if p.exit().is_some() {
                break (String::new(), false);
            }
            let (ultra, closing) = {
                let l = self.live.lock().unwrap();
                (l.ultra, l.closing)
            };
            // Oak closing with the tasks left running: the wait stays open.
            if ultra && !closing {
                // (The task was killed with the rest: its end, if it shows.)
                p.wait_exit(Duration::from_secs(1));
                break ("; the user stopped everything (ultrapause), which ended the wait".to_string(), true);
            }
            if self.interrupted() {
                break ("; the user sent a message, which ended the wait".to_string(), true);
            }
            if self.live.lock().unwrap().ready_count() > 0 {
                break (format!("; the wait ended after {} because the user sent a message", fmt_duration(waited())), false);
            }
            if !self.conv.lock().unwrap().inbox.is_empty() {
                break (format!("; the wait ended after {} because a message arrived (it follows)", fmt_duration(waited())), false);
            }
            if waited() >= timeout {
                break (format!("; still running after waiting {}s", timeout.as_secs()), false);
            }
            self.wait_a_bit(30);
        };
        drop(waiting);
        let after = self.end_wait(call_id).filter(|w| w.closed_ms > 0).map_or(String::new(), |w| {
            // (The task's state says whether it kept running, ended or was
            // stopped meanwhile.)
            let closed = fmt_duration(Duration::from_millis(w.closed_ms as u64));
            match p.exit() {
                None => format!("(Oak was closed for {closed} during this wait; the task kept running, and the wait carried on when Oak started again, the time Oak was closed not counting toward the timeout.)"),
                Some(_) if p.status() == tasks::STOPPED_ON_CLOSE => format!("(Oak was closed for {closed} during this wait.)"),
                Some(_) => format!("(Oak was closed for {closed} during this wait, which carried on when Oak started again.)"),
            }
        });
        (Ok(self.task_output(id, &p, &why, &after)), interrupted)
    }

    /// The record of the TASK wait of call `call` (`Conversation::task_waits`):
    /// the one kept from before Oak closed, or a new one (none for no call).
    fn begin_wait(&self, call: &str) -> store::TaskWait {
        let mut c = self.conv.lock().unwrap();
        if let Some(w) = c.task_waits.iter().find(|w| w.call == call) {
            return w.clone();
        }
        let w = store::TaskWait { call: call.into(), since: api::now_ms(), ..Default::default() };
        if !call.is_empty() {
            c.task_waits.push(w.clone());
            c.save();
        }
        w
    }

    /// The wait of call `call` is over: its record goes.
    fn end_wait(&self, call: &str) -> Option<store::TaskWait> {
        let mut c = self.conv.lock().unwrap();
        let i = c.task_waits.iter().position(|w| w.call == call && !call.is_empty())?;
        let w = c.task_waits.remove(i);
        c.save();
        Some(w)
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
        let summary = input["summary"].as_str().unwrap_or("");
        match self.run_command(call_id, &p, timeout, summary) {
            Ran::Exited { out, code } => {
                // How long it ran, when that is worth knowing.
                let ran = p.elapsed();
                let ran = if ran >= Duration::from_secs(1) { format!(", ran {}", tasks::fmt_took(ran)) } else { String::new() };
                // Not clipped: a long output is saved whole (see run_call).
                (Ok(format!("{}\n[exit code {code}{ran}]", out.trim_end())), false)
            }
            Ran::Killed { out, took } => {
                let msg = format!(
                    "Output so far:\n{}\n\nThe user stopped everything (ultrapause), so the command was killed after {}.",
                    if out.trim().is_empty() { "(none)" } else { out.trim_end() },
                    tasks::fmt_took(took)
                );
                (Err(msg), true)
            }
            Ran::Moved { out, more, id, took, interrupted } => {
                let took = tasks::fmt_took(took);
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
                (Ok(msg), interrupted)
            }
        }
    }

    /// Waits for a command started for call `call_id` (its output shows
    /// under the call's row meanwhile, unless the id is empty) to exit:
    /// ultrapause kills it; an interrupt or `timeout` moves it to the
    /// background, as a task (`summary` says what it is).
    pub(super) fn run_command(&self, call_id: &str, p: &Arc<Proc>, timeout: Duration, summary: &str) -> Ran {
        let _streaming = tasks::Streaming::new(call_id, p);
        let mut shown_lines = 0;
        let t = Instant::now();
        loop {
            if let Some(code) = p.exit() {
                let (out, _) = p.take_new(usize::MAX);
                return Ran::Exited { out, code };
            }
            if self.ultra() {
                // Ultrapause: killed, not moved to the background.
                p.kill();
                let (out, _) = p.take_new(MAX_OUTPUT);
                return Ran::Killed { out, took: t.elapsed() };
            }
            let interrupted = self.interrupted();
            if interrupted || t.elapsed() > timeout {
                let (out, more) = p.take_new(MAX_OUTPUT);
                let id = self.adopt(p, summary);
                return Ran::Moved { out, more, id, took: t.elapsed(), interrupted };
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
            // Until it exits (its result goes at once), or for at most a
            // tick: what else ends the wait (an interrupt, ultrapause, the
            // timeout) and new output for the row are looked at that often.
            let left = timeout.saturating_sub(t.elapsed()).max(Duration::from_millis(1));
            p.wait_exit(EXECUTE_TICK.min(left));
        }
    }
}

/// How a command `Worker::run_command` waited for ended.
pub(super) enum Ran {
    /// With its exit code and all its output.
    Exited { out: String, code: i32 },
    /// Ultrapause killed it: its output so far.
    Killed { out: String, took: Duration },
    /// It became task `id`, after an interrupt or its timeout: its output
    /// so far (`more` waiting).
    Moved { out: String, more: bool, id: u64, took: Duration, interrupted: bool },
}

/// How often a running EXECUTE looks at what isn't its command's exit (an
/// interrupt, ultrapause, the timeout, new output to show); the exit itself
/// ends the wait at once.
const EXECUTE_TICK: Duration = Duration::from_millis(30);

/// How long PROGRESS_SET waits for the meter's first reading.
const FIRST_READING: Duration = Duration::from_secs(15);

/// EXECUTE's `timeout`, if it is in range (EXECUTE_TIMEOUT_MIN to _MAX).
pub(crate) fn execute_timeout(input: &Value) -> Result<Duration, String> {
    match input["timeout"].as_u64() {
        Some(s @ tools::EXECUTE_TIMEOUT_MIN..=tools::EXECUTE_TIMEOUT_MAX) => Ok(Duration::from_secs(s)),
        _ => Err(format!("`timeout` must be from {} to {} seconds.", tools::EXECUTE_TIMEOUT_MIN, tools::EXECUTE_TIMEOUT_MAX)),
    }
}

/// What ASK returns while a goal is active (its questions wait for the
/// user, and the turn goes on).
pub(crate) const ASKED_DURING_GOAL: &str = "The questions are shown to the user, who is asked asynchronously since a goal is active: this doesn't wait for their answers, so keep working on the goal meanwhile, on what doesn't depend on them. When the user answers (or skips the questions), a message brings you the questions and their answers together.";

/// What a call of REQUEST_ASK gets (a conversation an older Oak started
/// still has it in its tool list).
pub(crate) const REQUEST_ASK_GONE: &str = "REQUEST_ASK is gone: call ASK with your questions instead. While a goal is active it doesn't wait: the user is asked asynchronously, you keep working, and their answers come later in a message.";

/// Whether ASK asks asynchronously: a goal is set, not paused and not
/// finished (then ending the turn hands control back to the user, who can
/// answer at once).
pub(crate) fn goal_active(c: &Conversation) -> bool {
    !c.goal.is_empty() && !c.goal_paused && !c.goal_finished
}

/// CD: the conversation's working folder becomes `path` (`tools::folder_path`,
/// `home` for `~`), an existing folder. The scratchpad stays where it is:
/// its name becomes its full path, which `scratchpad` and the scratchpads'
/// scan take as it is. The system prompt keeps the folder its cache was
/// written with until the next cold moment (`prefix::Pin::folder`), so the
/// result says what changed, with the new folder's project instructions
/// (the user's own file is in the context already).
pub(crate) fn change_folder(c: &mut Conversation, input: &Value, home: &Path) -> Result<String, String> {
    let typed = input["path"].as_str().unwrap_or("").trim();
    if typed.is_empty() {
        return Err("`path` is empty: give the folder to switch to.".into());
    }
    let dir = tools::folder_path(&c.folder, typed, home);
    match std::fs::metadata(&dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(format!("{} is a file, not a folder.", dir.display())),
        Err(e) => return Err(format!("No folder {}: {e}", dir.display())),
    }
    if tools::path_key(&dir) == tools::path_key(&c.folder) {
        return Ok(format!("Your working folder is {} already.", c.folder.display()));
    }
    if !c.scratch.is_empty() && Path::new(&c.scratch).is_relative() {
        c.scratch = c.folder.join(SCRATCH_ROOT).join(&c.scratch).display().to_string();
    }
    let old = std::mem::replace(&mut c.folder, dir);
    let mut out = format!(
        "Your working folder is now {} (it was {}): relative paths, EXECUTE and new tasks use it from now on; tasks already running keep the old one.",
        c.folder.display(),
        old.display()
    );
    if !c.scratch.is_empty() {
        out += &format!(" Your scratchpad stays at {}.", c.scratch);
    }
    let mut skipped = vec![];
    let files: Vec<instructions::File> = instructions::load(&c.folder, home, &mut skipped).into_iter().filter(|f| f.kind != instructions::Kind::User).collect();
    if !files.is_empty() {
        out += &format!("\n\nThe new folder's project instructions:\n{}", instructions::render(&files, &[]));
    }
    Ok(out)
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
            let names: Vec<String> = starting.iter().map(|&i| crate::transcript::shown(&self.calls[i])).collect();
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

/// For each of a reply's tool calls, the calls before it that it waits
/// for; the rest run at the same time. ASK, CD and REPL wait for all before
/// them, and all after them wait for them (they run on their own); a call on a file waits
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
            // The browser's calls act on one page: in the order they were
            // made (also those made through LOAD, before BROWSER is in the
            // tool list).
            let browser = tools::canonical_name(name, &c["input"]).starts_with("BROWSER");
            if browser {
                paths.push("\0browser".into());
            }
            // CD changes what every later call's paths and commands mean.
            // REPL scripts share one context: one at a time, in order.
            (paths, browser || matches!(name, "WRITE" | "EDIT" | "SCREENSHOT"), matches!(name, "ASK" | "CD" | "REPL"))
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
