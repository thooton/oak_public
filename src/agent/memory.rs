//! The session's memory: its goal, notes, heartbeats and progress meters,
//! the tools that change them, and the block they go to the model in with
//! each request.

use super::*;

/// What GOAL justify and finish answer for an endless goal.
const ENDLESS_REFUSAL: &str = "The user made this goal endless, so it can't be justified or finished: keep working on it. Only the user ends it.";

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
            if c.goal_endless {
                return Err(ENDLESS_REFUSAL.into());
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
            if c.goal_endless {
                return Err(ENDLESS_REFUSAL.into());
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
            let limit = notes_limit();
            let (before, after) = notes_change(&c.notes, 0, text.len());
            if after > limit {
                return Err(if before > limit {
                    format!(
                        "Not saved: your notes take about {before} tokens, over their limit of {}. Shorten or delete notes first \
                         (NOTE action=update with less text, or action=delete) until they are under it.",
                        with_commas(limit)
                    )
                } else {
                    format!(
                        "Not saved: it would take your notes to about {after} tokens, past their limit of {} (they take about {before} \
                         now). Save less, or first shorten or delete notes (NOTE action=update with less text, or action=delete).",
                        with_commas(limit)
                    )
                });
            }
            c.next_note_id += 1;
            let id = c.next_note_id;
            c.notes.push(Note { id, text, rewrite: None });
            Ok(format!(
                "Saved note {id} ({}). It stays in your context for the rest of this session; update it with NOTE action=update when what it \
                 tracks changes, and delete it with NOTE action=delete when it is stale.{}",
                sizes(c, Some(id)),
                notes_alert(c)
            ))
        }
        "NOTE_UPDATE" => {
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err("The note is empty; delete it with NOTE action=delete instead.".into());
            }
            replace_note(c, id, |_| Ok(text), "Not updated", "Updated note")
        }
        "NOTE_EDIT" => {
            let old = input["old_string"].as_str().unwrap_or("");
            let new = input["new_string"].as_str().unwrap_or("");
            let all = input["replace_all"].as_bool().unwrap_or(false);
            if old.is_empty() {
                return Err("old_string is empty: give the exact text in the note to replace.".into());
            }
            replace_note(
                c,
                id,
                |text| {
                    let found = text.matches(old).count();
                    if found == 0 {
                        return Err(format!("old_string isn't in note {id}: it must match the note's text exactly."));
                    }
                    if found > 1 && !all {
                        return Err(format!(
                            "old_string appears {found} times in note {id}: give more of the text around it, or set replace_all."
                        ));
                    }
                    let text = if all { text.replace(old, new) } else { text.replacen(old, new, 1) }.trim().to_string();
                    if text.is_empty() {
                        return Err(format!("That would leave note {id} empty; delete it with NOTE action=delete instead."));
                    }
                    Ok(text)
                },
                "Not edited",
                "Edited note",
            )
        }
        "NOTE_APPEND" => {
            let more = input["text"].as_str().unwrap_or("").trim().to_string();
            if more.is_empty() {
                return Err("The text to append is empty.".into());
            }
            replace_note(c, id, |text| Ok(format!("{text}\n{more}")), "Not appended", "Appended to note")
        }
        "NOTE_DELETE" => {
            // `ids` deletes several at once; `id` (or both) still works.
            let mut ids: Vec<u64> = input["ids"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
            if input["id"].is_u64() {
                ids.push(id);
            }
            let mut seen = std::collections::HashSet::new();
            ids.retain(|i| seen.insert(*i));
            if ids.is_empty() {
                return Err("Give the note to delete as `id`, or several as `ids`.".into());
            }
            let (gone, missing): (Vec<u64>, Vec<u64>) = ids.iter().partition(|i| c.notes.iter().any(|n| n.id == **i));
            let list = |ids: &[u64]| ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
            if gone.is_empty() {
                let s = if missing.len() > 1 { "s" } else { "" };
                return Err(format!("No note{s} {}.", list(&missing)));
            }
            c.notes.retain(|n| !gone.contains(&n.id));
            let s = if gone.len() > 1 { "s" } else { "" };
            let missing = if missing.is_empty() { String::new() } else { format!(" No note {} (skipped).", list(&missing)) };
            // (Still over the limit: what remains to do.)
            let more = if notes_tokens(&c.notes) > notes_limit() { notes_alert(c) } else { String::new() };
            Ok(format!("Deleted note{s} {} ({}).{missing}{more}", list(&gone), sizes(c, None)))
        }
        "NOTE_LIST" => {
            if c.notes.is_empty() {
                return Ok(format!("You have no notes (their limit is about {} tokens in all).", with_commas(notes_limit())));
            }
            let mut s = format!("{} note{}, {}:\n", c.notes.len(), if c.notes.len() == 1 { "" } else { "s" }, sizes(c, None));
            for n in &c.notes {
                s += &note_line(n);
            }
            Ok(s.trim_end().to_string() + &notes_alert(c))
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

/// The notes' limit by default (the "Notes limit" setting): about this many
/// tokens, all of them together.
pub const DEFAULT_NOTES_LIMIT: u64 = 2_000;

/// The limits Settings offers.
pub const NOTES_LIMITS: [(u64, &str); 5] = [(1_000, "1,000"), (2_000, "2,000"), (4_000, "4,000"), (8_000, "8,000"), (16_000, "16,000")];

/// The smallest and largest limit taken from settings.json.
const NOTES_LIMIT_RANGE: std::ops::RangeInclusive<u64> = 100..=1_000_000;

/// The hard limit on a session's notes, in tokens (`notes_tokens`): NOTE
/// refuses a new note, or an update that makes a note longer, that would
/// take them past it. The model is told it in the system prompt (and of a
/// change by a message: `prefix::State::prepare`); notes already past it
/// (the limit was lowered) are kept, and only shortening or deleting is
/// let through until they are under it.
static NOTES_LIMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(DEFAULT_NOTES_LIMIT);

#[cfg(test)]
thread_local! {
    /// A test's own limit, for this thread only.
    static TEST_NOTES_LIMIT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The notes' limit now (the setting).
pub fn notes_limit() -> u64 {
    #[cfg(test)]
    if let Some(n) = TEST_NOTES_LIMIT.with(|l| l.get()) {
        return n;
    }
    NOTES_LIMIT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Puts the "Notes limit" setting in force (from settings.json: one out of
/// `NOTES_LIMIT_RANGE` is brought into it).
pub fn set_notes_limit(n: u64) {
    NOTES_LIMIT.store(sane_notes_limit(n), std::sync::atomic::Ordering::Relaxed);
}

/// The limit a setting of `n` puts in force: the default for none (0).
pub(crate) fn sane_notes_limit(n: u64) -> u64 {
    if n == 0 { DEFAULT_NOTES_LIMIT } else { n.clamp(*NOTES_LIMIT_RANGE.start(), *NOTES_LIMIT_RANGE.end()) }
}

/// A limit for this test's thread only (None: the setting's).
#[cfg(test)]
pub(crate) fn set_test_notes_limit(n: Option<u64>) {
    TEST_NOTES_LIMIT.with(|l| l.set(n));
}

/// Notes past this many tokens get the model a reminder to tidy them up
/// (85% of the limit: 1,700 by default).
pub fn notes_alert_tokens() -> u64 {
    notes_limit() * 85 / 100
}

/// About how many tokens the notes take.
pub fn notes_tokens(notes: &[store::Note]) -> u64 {
    crate::rolling::estimate(notes.iter().map(|n| n.text.len()).sum())
}

/// The notes' tokens before and after a change that replaces `old` bytes
/// of note text (0 for a new note) with `new`.
fn notes_change(notes: &[store::Note], old: usize, new: usize) -> (u64, u64) {
    let bytes: usize = notes.iter().map(|n| n.text.len()).sum();
    (crate::rolling::estimate(bytes), crate::rolling::estimate(bytes - old + new))
}

/// About how many tokens one note takes (at least 1), as its size is shown
/// to the model; the limit goes by `notes_tokens`, of all of them together.
pub fn note_tokens(n: &store::Note) -> u64 {
    crate::rolling::estimate(n.text.len()).max(1)
}

/// A note as the model sees it listed: `[3] (~40 tokens) text`.
fn note_line(n: &Note) -> String {
    format!("[{}] (~{} tokens) {}\n", n.id, note_tokens(n), n.text)
}

/// The notes' size for a NOTE result: "note 3 takes about 40 tokens; your
/// notes about 450 of 2,000 in all" (without `id`, only the total).
fn sizes(c: &Conversation, id: Option<u64>) -> String {
    let total = format!("about {} of {} tokens in all", notes_tokens(&c.notes), with_commas(notes_limit()));
    match id.and_then(|id| c.notes.iter().find(|n| n.id == id)) {
        Some(n) => format!("note {} takes about {} tokens; your notes {total}", n.id, note_tokens(n)),
        None => format!("your notes take {total}"),
    }
}

/// NOTE update, edit and append: note `id`'s text becomes what `change`
/// makes of it. Over the limit, a note may still be reworded or cut down:
/// only a change that makes it longer and takes the notes past the limit
/// is refused (`not`: "Not updated"...; `done`: "Updated note"...).
fn replace_note(
    c: &mut Conversation,
    id: u64,
    change: impl FnOnce(&str) -> Result<String, String>,
    not: &str,
    done: &str,
) -> Result<String, String> {
    let Some(n) = c.notes.iter().find(|n| n.id == id) else { return Err(format!("No note {id}.")) };
    let old = n.text.len();
    let text = change(&n.text)?;
    let limit = notes_limit();
    let (before, after) = notes_change(&c.notes, old, text.len());
    if after > limit && text.len() > old {
        return Err(format!(
            "{not}: it would make note {id} longer and take your notes to about {after} tokens, past their limit of {} (they \
             take about {before} now). Keep the note at most as long as it is, or first shorten or delete other notes.",
            with_commas(limit)
        ));
    }
    let n = c.notes.iter_mut().find(|n| n.id == id).expect("found above");
    n.text = text;
    n.rewrite = None;
    Ok(format!("{done} {id} ({}).{}", sizes(c, Some(id)), notes_alert(c)))
}

/// A count with thousands separators: 12345 → "12,345".
pub fn with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// Whether the model gets that reminder at all (the "Notes reminder"
// setting); on by default.
switch!(NOTES_ALERT, notes_alert_on, set_notes_alert, true);

#[cfg(test)]
thread_local! {
    /// A test's own reminder switch, for this thread only.
    static TEST_NOTES_ALERT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The reminder switch for this thread's test (None: the setting's).
#[cfg(test)]
pub(crate) fn set_test_notes_alert(on: Option<bool>) {
    TEST_NOTES_ALERT.with(|a| a.set(on));
}

/// Whether the model gets told to tidy its notes (the setting, or a test's).
fn reminder_on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_NOTES_ALERT.with(|a| a.get()) {
        return on;
    }
    notes_alert_on()
}

/// For NOTE results (and the memory block), only with the reminder on:
/// with the notes past their limit (it was lowered), what the model must
/// do before adding to them; past `notes_alert_tokens`, a reminder to tidy
/// them up; empty otherwise. With the reminder off the model is never told
/// to shrink its notes unprompted (NOTE's own refusals still say why).
fn notes_alert(c: &Conversation) -> String {
    if !reminder_on() {
        return String::new();
    }
    let (tokens, limit) = (notes_tokens(&c.notes), notes_limit());
    if tokens > limit {
        return format!(
            "\n\nYour notes take about {tokens} tokens, over their limit of {}: Oak refuses new notes and updates that make a note longer \
             until they are under it. Shorten or merge notes with NOTE action=update and delete what is done or stale with NOTE action=delete.",
            with_commas(limit)
        );
    }
    if tokens <= notes_alert_tokens() {
        return String::new();
    }
    format!(
        "\n\nYour notes are getting big: {} notes, about {tokens} tokens (the limit is {}), and they are sent with \
         every request. Tidy them up now: merge notes about the same thing into one with NOTE action=update, cut progress notes down \
         to the current state, and delete what is done or stale with NOTE action=delete.",
        c.notes.len(),
        with_commas(limit)
    )
}

/// The goal, notes, heartbeats and meters, as a block for the system prompt.
pub(crate) fn render_memory(c: &Conversation) -> String {
    if c.goal.is_empty() && c.notes.is_empty() && c.heartbeats.is_empty() && c.meters.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n# Session memory\n");
    if c.goal_endless {
        s += &format!("\nGoal, set by the user as endless (GOAL justify and finish are disabled: keep working on it until the user clears it):\n{}\n", c.goal);
    } else if !c.goal.is_empty() {
        s += &format!("\nGoal, set by the user (when it is done: GOAL action=justify, then action=finish):\n{}\n", c.goal);
    }
    if !c.notes.is_empty() {
        s += &format!(
            "\nYour notes, about {} of {} tokens in all (NOTE; action=update keeps one current, action=list shows them and their sizes as they are now):\n",
            notes_tokens(&c.notes),
            with_commas(notes_limit())
        );
        for n in &c.notes {
            s += &note_line(n);
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
    s += "\nThis block is brought up to date only when earlier context is summarized: goals, notes, heartbeats and progress meters changed in the conversation since then are the current ones.";
    s
}

/// The session memory for the next request. It is part of the prefix, so
/// like the rest of it (`crate::prefix`) it is brought up to date only when
/// the session starts and after each summary: never because the cache
/// expired, another model or account, or a restart. In between, changes
/// are visible to the model as the tool calls and goal notes that made
/// them: a new note isn't added twice. (The call keeps its text after the
/// block has it.) The instruction files and memory
/// index (the context block) are read at those moments too. The transcript
/// gets a line when the files loaded change.
pub(crate) fn memory_for_request(c: &mut Conversation, now: i64) -> String {
    let base = c.rolling.base();
    if c.memory.as_ref().is_none_or(|m| m.base != base) {
        let old = c.memory.take();
        let mut skipped = vec![];
        // A chat has no folder: no instruction files or memory.
        let files = if c.chat { vec![] } else { instructions::load(&c.folder, &instructions::home(), &mut skipped) };
        let context = if c.chat { String::new() } else { instructions::render(&files, &instructions::memory_index(&c.folder)) };
        let paths: Vec<String> = files.iter().map(|f| f.path.display().to_string()).collect();
        let before = old.map(|m| m.files).unwrap_or_default();
        if paths != before && (!paths.is_empty() || !before.is_empty()) {
            let line = loaded_line(&paths, &skipped, &c.folder);
            add_mark(c, MarkKind::Instructions, line, context.clone(), now);
        }
        c.memory = Some(Memory { text: render_memory(c), context, files: paths, base, used_at: now });
    }
    let m = c.memory.as_mut().unwrap();
    m.used_at = now;
    m.text.clone()
}
