//! The session's memory: its goal, notes, heartbeats and progress meters,
//! the tools that change them, and the block they go to the model in with
//! each request.

use super::*;

/// NOTE, HEARTBEAT, PROGRESS and GOAL: the tools that change the session's
/// memory (`name` is the action's tool, as `tools::canonical` gives it).
pub(crate) fn memory_tool(c: &mut Conversation, name: &str, input: &Value, now: i64) -> Result<String, String> {
    let id = input["id"].as_u64().unwrap_or(0);
    match name {
        "GOAL_JUSTIFICATION" => {
            let text = input["justification"].as_str().unwrap_or("").trim().to_string();
            if c.goal.is_empty() {
                return Err("No goal is set, so there is nothing to justify.".into());
            }
            if text.is_empty() {
                return Err("The justification is empty.".into());
            }
            c.goal_justification = text;
            Ok("Saved the justification. When it accurately reflects how you have fulfilled the user's request in its \
                entirety, call GOAL action=finish; if it doesn't yet, keep working and call GOAL action=justify again."
                .into())
        }
        "GOAL_FINISH" => {
            if c.goal.is_empty() {
                return Err("No goal is set, so there is nothing to finish.".into());
            }
            if c.goal_justification.is_empty() {
                return Err("Call GOAL action=justify first, with how you have completed the user's goal.".into());
            }
            c.goal_finished = true;
            Ok("The goal is finished. Control goes back to the user when you end your turn.".into())
        }
        "NOTE_NEW" => {
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err("The note is empty.".into());
            }
            c.next_note_id += 1;
            let id = c.next_note_id;
            c.notes.push(Note { id, text, rewrite: None });
            Ok(format!(
                "Saved note {id}. It stays in your context for the rest of this session; update it with NOTE action=update when what it \
                 tracks changes, and delete it with NOTE action=delete when it is stale.{}",
                notes_alert(c)
            ))
        }
        "NOTE_UPDATE" => {
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err("The note is empty; delete it with NOTE action=delete instead.".into());
            }
            let Some(n) = c.notes.iter_mut().find(|n| n.id == id) else { return Err(format!("No note {id}.")) };
            n.text = text;
            n.rewrite = None;
            Ok(format!("Updated note {id}.{}", notes_alert(c)))
        }
        "NOTE_DELETE" => {
            let before = c.notes.len();
            c.notes.retain(|n| n.id != id);
            if c.notes.len() == before { Err(format!("No note {id}.")) } else { Ok(format!("Deleted note {id}.")) }
        }
        "HEARTBEAT_START" => {
            let message = input["message"].as_str().unwrap_or("").trim().to_string();
            let min = interval(input)?;
            if message.is_empty() {
                return Err("The message is empty.".into());
            }
            c.next_heartbeat_id += 1;
            let id = c.next_heartbeat_id;
            c.heartbeats.push(Heartbeat { id, message, every_min: min, next_at: now + min as i64 * 60_000 });
            Ok(format!(
                "Started heartbeat {id}: every {} you will get its message, the first in {min} min. \
                 It arrives when you are idle (beats missed while busy are skipped). Stop it with HEARTBEAT action=stop id {id}.",
                every(min)
            ))
        }
        "HEARTBEAT_STOP" => {
            let before = c.heartbeats.len();
            c.heartbeats.retain(|h| h.id != id);
            if c.heartbeats.len() == before { Err(format!("No heartbeat {id}.")) } else { Ok(format!("Stopped heartbeat {id}.")) }
        }
        "PROGRESS_SET" => set_meter(c, input, now),
        "PROGRESS_REMOVE" => {
            let before = c.meters.len();
            c.meters.retain(|m| m.id != id);
            if c.meters.len() == before { Err(format!("No progress meter {id}.")) } else { Ok(format!("Removed progress meter {id}.")) }
        }
        _ => Err(format!("Unknown tool {name}")),
    }
}

/// A heartbeat's or meter's `interval_minutes`, from 1 to 10080 (a week).
fn interval(input: &Value) -> Result<u64, String> {
    match input["interval_minutes"].as_u64() {
        Some(min @ 1..=10_080) => Ok(min),
        _ => Err("interval_minutes must be from 1 to 10080 (a week).".into()),
    }
}

/// PROGRESS_SET: a new meter, due at once, or a new command for the meter
/// with that label (any case), which then starts again without a value.
/// Its watcher runs it (`progress::watch`).
fn set_meter(c: &mut Conversation, input: &Value, now: i64) -> Result<String, String> {
    let label = crate::tools::one_line(input["label"].as_str().unwrap_or(""));
    let command = input["command"].as_str().unwrap_or("").trim().to_string();
    let min = interval(input)?;
    if label.is_empty() {
        return Err("The label is empty.".into());
    }
    if command.is_empty() {
        return Err("The command is empty.".into());
    }
    let what = |id: u64, done: &str| {
        format!(
            "{done} progress meter {id} ({label}): Oak runs the command now and then every {}, and shows the user the value \
             from the last line it prints. Remove it with PROGRESS action=remove id {id}.",
            every(min)
        )
    };
    if let Some(m) = c.meters.iter_mut().find(|m| m.label.to_lowercase() == label.to_lowercase()) {
        *m = store::Meter { id: m.id, label: label.clone(), command, every_min: min, next_at: now, generation: m.generation + 1, watched: m.watched, ..Default::default() };
        return Ok(what(m.id, "Replaced"));
    }
    if c.meters.len() >= crate::progress::MAX_METERS {
        return Err(format!("There are already {} progress meters; remove one with PROGRESS action=remove first.", c.meters.len()));
    }
    c.next_meter_id += 1;
    let id = c.next_meter_id;
    c.meters.push(store::Meter { id, label: label.clone(), command, every_min: min, next_at: now, ..Default::default() });
    Ok(what(id, "Set up"))
}

/// Notes past about this many tokens (all of them together) get the model
/// a reminder to tidy them up: they ride along with every request.
pub const NOTES_ALERT_TOKENS: u64 = 1_700;

/// About how many tokens the notes take.
pub fn notes_tokens(notes: &[store::Note]) -> u64 {
    crate::rolling::estimate(notes.iter().map(|n| n.text.len()).sum())
}

// Whether the model gets that reminder at all (the "Notes reminder"
// setting); on by default.
switch!(NOTES_ALERT, notes_alert_on, set_notes_alert, true);

/// For NOTE_NEW and NOTE_UPDATE results (and the memory block): a reminder
/// when the notes have grown past `NOTES_ALERT_TOKENS` and the reminder is
/// on; empty otherwise.
fn notes_alert(c: &Conversation) -> String {
    let tokens = notes_tokens(&c.notes);
    if tokens <= NOTES_ALERT_TOKENS || !notes_alert_on() {
        return String::new();
    }
    format!(
        "\n\nYour notes are getting big: {} notes, about {tokens} tokens (the limit is {NOTES_ALERT_TOKENS}), and they are sent with \
         every request. Tidy them up now: merge notes about the same thing into one with NOTE action=update, cut progress notes down \
         to the current state, and delete what is done or stale with NOTE action=delete.",
        c.notes.len()
    )
}

/// The goal, notes, heartbeats and meters, as a block for the system prompt.
pub(crate) fn render_memory(c: &Conversation) -> String {
    if c.goal.is_empty() && c.notes.is_empty() && c.heartbeats.is_empty() && c.meters.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n# Session memory\n");
    if !c.goal.is_empty() {
        s += &format!("\nGoal, set by the user (when it is done: GOAL action=justify, then action=finish):\n{}\n", c.goal);
    }
    if !c.notes.is_empty() {
        s += "\nYour notes (NOTE; action=update keeps one current):\n";
        for n in &c.notes {
            s += &format!("[{}] {}\n", n.id, n.text);
        }
        let alert = notes_alert(c);
        if !alert.is_empty() {
            s += alert.trim_start();
            s += "\n";
        }
    }
    if !c.heartbeats.is_empty() {
        s += "\nYour running heartbeats:\n";
        for h in &c.heartbeats {
            s += &format!("[{}] every {}: {}\n", h.id, every(h.every_min), h.message);
        }
    }
    if !c.meters.is_empty() {
        s += "\nProgress meters the user sees (PROGRESS action=set with the same label replaces one):\n";
        for m in &c.meters {
            s += &format!("[{}] {}, every {}: `{}`\n", m.id, m.label, every(m.every_min), m.command);
        }
    }
    s += "\nThis block is updated from time to time: goals, notes, heartbeats and progress meters changed in the conversation since then are the current ones.";
    s
}

/// The session memory for the next request. Changing the system prompt
/// rewrites the whole cached context, so the block is only brought up to
/// date when the cache is being rebuilt anyway (`cold`: a summary was
/// applied, the cache expired, another model or account). In between,
/// changes are visible to the model as the tool calls and goal notes that
/// made them: a new note isn't added twice. (Once the block has it, the
/// call's text is folded: `prefix::State::fold`.) The instruction files and memory index (the context block) are
/// read only when the session starts and after each summary. The transcript
/// gets a line when the files loaded change.
pub(crate) fn memory_for_request(c: &mut Conversation, now: i64, cold: bool) -> String {
    let base = c.rolling.base();
    let reload = c.memory.as_ref().is_none_or(|m| m.base != base);
    // Kept warm by keep-alive requests (`Worker::keep_warm`), the cache
    // lives on past its lifetime from the last request: they move `used_at`.
    if reload || cold || c.cache.cold || c.memory.as_ref().is_some_and(|m| now - m.used_at > c.cache.ttl_ms() + 5_000) {
        let (context, paths) = match c.memory.take() {
            Some(m) if !reload => (m.context, m.files),
            old => {
                let mut skipped = vec![];
                let files = instructions::load(&c.folder, &instructions::home(), &mut skipped);
                let context = instructions::render(&files, &instructions::memory_index(&c.folder));
                let paths: Vec<String> = files.iter().map(|f| f.path.display().to_string()).collect();
                let before = old.map(|m| m.files).unwrap_or_default();
                if paths != before && (!paths.is_empty() || !before.is_empty()) {
                    let line = loaded_line(&paths, &skipped, &c.folder);
                    add_mark(c, MarkKind::Instructions, line, context.clone(), now);
                }
                (context, paths)
            }
        };
        c.memory = Some(Memory { text: render_memory(c), context, files: paths, base, used_at: now });
    }
    let m = c.memory.as_mut().unwrap();
    m.used_at = now;
    m.text.clone()
}
