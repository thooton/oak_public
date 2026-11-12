// Unslop: the transcript shows the model's messages to the user rewritten by
// Gemma 4 31B (through OpenRouter) in concise, neutral prose. What
// the model is sent never changes: the rewrite lives in the text block's
// `_oak_unslop` key, which (like every `_oak_` key) is dropped before sending.
//
// The marker goes through three states:
//   {"id": "..."}                    being rewritten (the text streams into JOBS)
//   {"id": "...", "text": "..."}     rewritten
//   {"id": "...", "error": "..."}    the rewrite failed; the original shows
// Only replies finished while the switch is on are marked; turning it off
// leaves marked ones as they are. With No drafts on, the transcript leaves
// out the "Drafted a message" rows too: a marked block shows only its
// rewrite (or the original, when the rewrite failed).
//
// Rewrite thinking: a thinking block is rewritten in sections while it
// streams in (`mark_thinking`), each as soon as it is complete: a paragraph
// (a bold header paragraph goes with the one after it, and short ones run
// together up to `SECTION` bytes). Its marker lists them by byte range:
//   {"parts": [{"id", "start", "end", "text"?, "error"?}, ...]}
// A section's rewrite can finish while its block is still in the reply
// streaming in: it waits in JOBS until the reply joins the conversation
// (`settle`).
//
// Rewrite notes: each note is rewritten for the notes box
// (`Note::rewrite`, dropped when the note changes); the notes the model
// sees stay as they are.

use crate::api::Sse;
use crate::oak::Event;
use crate::store::{Conversation, Note, Rewrite};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

pub const KEY: &str = "_oak_unslop";
pub const MODEL: &str = "google/gemma-4-31b-it";

pub const PROMPT: &str = r#"Rewrite the text in `<input></input>` as concise, direct prose. Return only the rewrite inside `<output></output>`.

Treat the reader as a competent adult. Never condescend or overexplain. Remove redundant help text, unsolicited tutorials, praise, reassurance, ceremonial apologies, and deferential filler. Explain non-obvious consequences and risks, not obvious actions. Do not add advice or explanations.

Preserve the meaning, uncertainty, and completion status. Keep every stated cause, reason, alternative, suggestion, name, number, command, path, link, phone number, and external resource exactly as given. Preserve code verbatim. Do not invent facts, reasons, restrictions, or claims of success. An error, missing tool, missing permission, or failing build is not a policy refusal.

Use neutral language and short sentences. Never use bullets, middle dots, or em dashes as separators between labels, metadata, or sentences; use commas, parentheses, or spacing instead. Actual lists are fine."#;

/// The switches: Unslop itself, then No drafts, Rewrite thinking and
/// Rewrite notes, which apply only with it on.
const ON: usize = 0;
const NO_DRAFTS: usize = 1;
const THINKING: usize = 2;
const NOTES: usize = 3;
const SWITCHES: usize = 4;

#[cfg(not(test))]
static FLAGS: [AtomicBool; SWITCHES] = [const { AtomicBool::new(false) }; SWITCHES];
// Per test thread, so a test turning them on doesn't reach sessions that
// other tests run meanwhile.
#[cfg(test)]
thread_local! {
    static FLAGS: [AtomicBool; SWITCHES] = const { [const { AtomicBool::new(false) }; SWITCHES] };
}

fn set_flag(i: usize, on: bool) {
    #[cfg(not(test))]
    FLAGS[i].store(on, Ordering::Relaxed);
    #[cfg(test)]
    FLAGS.with(|f| f[i].store(on, Ordering::Relaxed));
}

fn flag(i: usize) -> bool {
    #[cfg(not(test))]
    return FLAGS[i].load(Ordering::Relaxed);
    #[cfg(test)]
    FLAGS.with(|f| f[i].load(Ordering::Relaxed))
}

pub fn set(on: bool) {
    set_flag(ON, on)
}

pub fn on() -> bool {
    flag(ON)
}

/// No drafts: the transcript shows a marked block's rewrite alone, without
/// the "Drafted a message" row over it.
pub fn no_drafts() -> bool {
    flag(NO_DRAFTS)
}

/// Rewrite thinking: thinking blocks are rewritten as they stream in.
pub fn thinking() -> bool {
    flag(THINKING)
}

/// Rewrite notes: the notes box shows the notes rewritten.
fn notes_switched_on() -> bool {
    flag(NOTES)
}

/// Sets the four switches as `settings` has them.
pub fn set_all(settings: &crate::store::Settings) {
    set(settings.unslop);
    set_flag(NO_DRAFTS, settings.unslop_no_drafts);
    set_flag(THINKING, settings.unslop_thinking);
    set_flag(NOTES, settings.unslop_notes);
}

/// A rewrite under way: the output so far, and once it has ended, how (kept
/// until it lands in its block: `settle`).
#[derive(Clone, Default)]
struct Job {
    text: String,
    result: Option<Result<String, String>>,
}

/// Rewrites under way, by marker id. Locked after a conversation, never
/// before one.
static JOBS: LazyLock<Mutex<HashMap<String, Job>>> = LazyLock::new(Default::default);

/// Sets what the rewrite `id` has written so far, as its stream does.
#[cfg(test)]
pub fn set_job_text(id: &str, text: &str) {
    JOBS.lock().unwrap().insert(id.into(), Job { text: text.into(), result: None });
}

/// A note's rewrite ids start with this. Notes are never in a reply
/// streaming in, so a note's rewrite that finds no note (it changed or
/// went) is dropped.
const NOTE: &str = "note-";

fn new_id() -> String {
    crate::api::hex(&rand::random::<[u8; 8]>())
}

/// How a marked text block (or a note) stands, for the transcript.
pub enum State<'a> {
    /// The model is still writing it (the reply streaming in).
    Drafting,
    /// Being rewritten: the output so far (empty before the first words).
    Streaming(String),
    Done(&'a str),
    Failed(&'a str),
}

/// What the rewrite `id` has written so far.
fn job_text(id: &str) -> String {
    JOBS.lock()
        .unwrap()
        .get(id)
        .map(|j| match &j.result {
            Some(Ok(t)) => t.clone(),
            _ => j.text.clone(),
        })
        .unwrap_or_default()
}

/// The state of a block whose `KEY` field is `m`.
pub fn state_of(m: &Value) -> State<'_> {
    match (m["text"].as_str(), m["error"].as_str()) {
        (Some(t), _) => State::Done(t),
        (None, Some(e)) => State::Failed(e),
        (None, None) => State::Streaming(job_text(m["id"].as_str().unwrap_or(""))),
    }
}

/// Whether a session's replies get rewritten: the switch is on and the
/// session is the user's own (a subagent's messages go to the model that
/// started it, not to the user).
pub fn applies(conv: &Conversation) -> bool {
    on() && conv.parent.is_none()
}

/// Marks a finished reply's text blocks for rewriting. True if any were.
pub fn mark(blocks: &mut [Value]) -> bool {
    let mut any = false;
    for b in blocks.iter_mut().filter(|b| b["type"] == "text" && b.get("_oak_file").is_none()) {
        if b["text"].as_str().is_some_and(|t| !t.trim().is_empty()) && b.get(KEY).is_none() {
            b[KEY] = json!({ "id": new_id() });
            any = true;
        }
    }
    any
}

/// Thinking goes to be rewritten in sections of at least this many bytes
/// (once their last paragraph is complete); a bold header paragraph starts
/// a new one.
const SECTION: usize = 600;

/// Rewrite thinking: marks the sections of the thinking block `b` that are
/// complete and not marked yet (all the rest, `done`: the block has ended),
/// and returns them, as (id, text), for `start_parts`.
pub fn mark_thinking(b: &mut Value, done: bool) -> Vec<(String, String)> {
    let text = b["text"].as_str().unwrap_or("");
    let from = b[KEY]["parts"].as_array().and_then(|p| p.last()).and_then(|p| p["end"].as_u64()).unwrap_or(0) as usize;
    let new: Vec<(String, usize, usize, String)> =
        sections(text, from, done).into_iter().map(|(s, e)| (new_id(), s, e, text[s..e].to_string())).collect();
    if new.is_empty() {
        return vec![];
    }
    let parts: Vec<Value> = new.iter().map(|(id, s, e, _)| json!({"id": id, "start": s, "end": e})).collect();
    let mut parts = Some(parts);
    if let Some(a) = b[KEY]["parts"].as_array_mut() {
        a.extend(parts.take().unwrap_or_default());
    }
    if let Some(parts) = parts {
        b[KEY] = json!({ "parts": parts });
    }
    new.into_iter().map(|(id, _, _, t)| (id, t)).collect()
}

/// The complete sections of `text` from byte `from` on (`done`: the text is
/// all there), as byte ranges: paragraphs (split at blank lines, trimmed), a
/// bold header paragraph going with what follows it, and short paragraphs
/// running together up to `SECTION` bytes.
fn sections(text: &str, from: usize, done: bool) -> Vec<(usize, usize)> {
    let Some(rest) = text.get(from..) else { return vec![] };
    // The complete paragraphs: all but the last are followed by a blank line.
    let mut paras = vec![];
    let mut pos = 0;
    let mut seps: Vec<usize> = rest.match_indices("\n\n").map(|(i, _)| i).collect();
    if done {
        seps.push(rest.len());
    }
    for sep in seps {
        let seg = &rest[pos..sep];
        if !seg.trim().is_empty() {
            let lead = seg.len() - seg.trim_start().len();
            paras.push((from + pos + lead, from + pos + seg.trim_end().len()));
        }
        pos = sep + 2;
    }
    let header = |(s, e): (usize, usize)| !text[s..e].contains('\n') && is_bold_header(&text[s..e]);
    let mut out = vec![];
    // The section being gathered, and whether its last paragraph is a header.
    let mut cur: Option<(usize, usize, bool)> = None;
    for p in paras {
        let h = header(p);
        cur = Some(match cur {
            Some((s, e, false)) if h || e - s >= SECTION => {
                out.push((s, e));
                (p.0, p.1, h)
            }
            Some((s, _, _)) => (s, p.1, h),
            None => (p.0, p.1, h),
        });
    }
    // Long enough, it can go: whatever comes next starts a section of its own.
    if let Some((s, e, last_header)) = cur
        && (done || (!last_header && e - s >= SECTION))
    {
        out.push((s, e));
    }
    out
}

/// How a thinking block's rewrite stands, for the transcript.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Thoughts {
    /// The rewrites so far, in order (a failed section's own text in its place).
    pub text: String,
    /// More is coming: a section being rewritten, or thinking still streaming in.
    pub pending: bool,
    /// Why sections failed.
    pub errors: Vec<String>,
}

/// A thinking section's byte range in its block.
fn span(part: &Value) -> (usize, usize) {
    let at = |k: &str| part[k].as_u64().unwrap_or(0) as usize;
    (at("start"), at("end"))
}

/// The rewrite of the thinking block `b`, when it is marked. `live`: it is
/// in the reply streaming in, so thinking past the marked sections is still
/// to be rewritten; otherwise that shows as it is.
pub fn thoughts(b: &Value, live: bool) -> Option<Thoughts> {
    let parts = b.get(KEY)?.get("parts")?.as_array()?;
    let text = b["text"].as_str().unwrap_or("");
    let mut out = Thoughts::default();
    let mut pieces: Vec<String> = vec![];
    let mut end = 0;
    for p in parts {
        let (s, e) = span(p);
        let own = text.get(s..e).unwrap_or("");
        end = e;
        if let Some(t) = p["text"].as_str() {
            pieces.push(t.to_string());
        } else if let Some(e) = p["error"].as_str() {
            pieces.push(own.to_string());
            out.errors.push(e.to_string());
        } else {
            match JOBS.lock().unwrap().get(p["id"].as_str().unwrap_or("")) {
                Some(Job { result: Some(Ok(t)), .. }) => pieces.push(t.clone()),
                Some(Job { result: Some(Err(e)), .. }) => {
                    pieces.push(own.to_string());
                    out.errors.push(e.clone());
                }
                Some(j) => {
                    pieces.push(j.text.clone());
                    out.pending = true;
                }
                // Left unfinished when Oak closed, not started again yet.
                None => out.pending = true,
            }
        }
    }
    let tail = text.get(end..).unwrap_or("").trim();
    if !tail.is_empty() {
        if live {
            out.pending = true;
        } else {
            pieces.push(tail.to_string());
        }
    }
    out.text = join_pieces(&pieces);
    Some(out)
}

/// Rewritten pieces one after another, spaced as paragraphs, empty ones gone.
fn join_pieces(pieces: &[String]) -> String {
    pieces.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// What the notes box shows for the note `n` with Rewrite notes on (`own`:
/// it is the user's own session's; a subagent's aren't rewritten). None:
/// the note as it is.
pub fn note_state(n: &Note, own: bool) -> Option<State<'_>> {
    if !(own && on() && notes_switched_on()) {
        return None;
    }
    Some(match &n.rewrite {
        // Marked on the next frame (`start_notes`).
        None => State::Streaming(String::new()),
        Some(Rewrite { text: Some(t), .. }) => State::Done(t),
        Some(Rewrite { error: Some(e), .. }) => State::Failed(e),
        Some(r) => State::Streaming(job_text(&r.id)),
    })
}

/// Rewrite notes: starts rewrites of the notes of `conv` that have none
/// (new, or changed since), and again of any left unfinished when Oak
/// closed. Cheap with nothing to do, so it runs every frame.
pub fn start_notes(conv: &Arc<crate::store::Shared>, ctx: &eframe::egui::Context) {
    let todo: Vec<(String, String)> = {
        let mut c = conv.lock().unwrap();
        if !(notes_switched_on() && applies(&c)) {
            return;
        }
        let mut todo = vec![];
        let mut marked = false;
        for n in c.notes.iter_mut() {
            match &n.rewrite {
                None => {
                    let id = format!("{NOTE}{}", new_id());
                    n.rewrite = Some(Rewrite { id: id.clone(), ..Default::default() });
                    todo.push((id, n.text.clone()));
                    marked = true;
                }
                Some(r) if r.text.is_none() && r.error.is_none() && !JOBS.lock().unwrap().contains_key(&r.id) => {
                    todo.push((r.id.clone(), n.text.clone()));
                }
                _ => {}
            }
        }
        if marked {
            c.save();
        }
        todo
    };
    start_parts(todo, conv, ctx);
}

/// Puts a finished rewrite in its place in `c`: a text block's marker, a
/// thinking section or a note. False when it isn't there (yet).
fn apply(c: &mut Conversation, id: &str, result: &Result<String, String>) -> bool {
    if id.starts_with(NOTE) {
        let Some(r) = c.notes.iter_mut().filter_map(|n| n.rewrite.as_mut()).find(|r| r.id == id) else { return false };
        match result {
            Ok(t) => r.text = Some(t.clone()),
            Err(e) => r.error = Some(e.clone()),
        }
        return true;
    }
    let set = |m: &mut Value| match result {
        Ok(t) => m["text"] = json!(t),
        Err(e) => m["error"] = json!(e),
    };
    // Found first, so only the message with it counts as changed (its
    // saved JSON is redone, `store::Messages`).
    let has = |m: &Value| {
        m["content"].as_array().into_iter().flatten().filter_map(|b| b.get(KEY)).any(|m| m["id"] == id || m["parts"].as_array().is_some_and(|ps| ps.iter().any(|p| p["id"] == id)))
    };
    let Some(i) = c.messages.iter().rposition(has) else { return false };
    for b in c.messages[i]["content"].as_array_mut().into_iter().flatten() {
        let Some(m) = b.get_mut(KEY) else { continue };
        if m["id"] == id {
            set(m);
            return true;
        }
        if let Some(p) = m.get_mut("parts").and_then(Value::as_array_mut).and_then(|ps| ps.iter_mut().find(|p| p["id"] == id)) {
            set(p);
            return true;
        }
    }
    false
}

/// Puts the rewrites that finished while their blocks were in a reply
/// streaming in into `conv`, now that the reply is part of it.
pub fn settle(conv: &Arc<crate::store::Shared>) {
    let mut c = conv.lock().unwrap();
    let done: Vec<(String, Result<String, String>)> =
        JOBS.lock().unwrap().iter().filter_map(|(id, j)| j.result.clone().map(|r| (id.clone(), r))).collect();
    let mut changed = false;
    for (id, r) in done {
        if apply(&mut c, &id, &r) {
            JOBS.lock().unwrap().remove(&id);
            changed = true;
        }
    }
    if changed {
        c.save();
    }
}

/// Looks for rewrite markers in a conversation not yet read from its file,
/// without parsing it. (Inside a JSON string a quote is escaped: only a
/// key, or a string of just the key, matches.)
pub fn look(c: &mut crate::store::Conversation) {
    if c.messages.no_rewrites {
        return;
    }
    if let Some(raw) = c.messages.unread() {
        let key = format!("\"{KEY}\"");
        let finder = memchr::memmem::Finder::new(key.as_bytes());
        c.messages.no_rewrites = !raw.iter().any(|m| finder.find(m).is_some());
    }
}

/// Settles finished rewrites, then starts one for each marked block of
/// `conv` (text, or a thinking section) that has none yet and none under
/// way (just marked, or left unfinished when Oak closed).
pub fn start(conv: &Arc<crate::store::Shared>, ctx: &eframe::egui::Context) {
    // Not yet read from its file, and with no marker in it: nothing to do,
    // and nothing parsed (every conversation starts here, at every start).
    {
        let mut c = conv.lock().unwrap();
        look(&mut c);
        if c.messages.no_rewrites && c.messages.unread().is_some() && JOBS.lock().unwrap().is_empty() {
            return;
        }
    }
    settle(conv);
    // (The markers without a text or an error, with what to rewrite.)
    let pending: Vec<(String, String)> = {
        let c = conv.lock().unwrap();
        let open = |m: &Value| m.get("text").is_none() && m.get("error").is_none();
        let mut out = vec![];
        for b in c.messages.iter().filter(|m| m["role"] == "assistant").flat_map(|m| m["content"].as_array().into_iter().flatten()) {
            let Some(m) = b.get(KEY).filter(|m| open(m)) else { continue };
            match m["parts"].as_array() {
                Some(parts) => {
                    let text = b["text"].as_str().unwrap_or("");
                    out.extend(
                        parts
                            .iter()
                            .filter(|p| open(p))
                            .filter_map(|p| Some((p["id"].as_str()?, text.get(p["start"].as_u64()? as usize..p["end"].as_u64()? as usize)?)))
                            .map(|(id, own)| (id.to_string(), own.to_string())),
                    );
                }
                None => {
                    if let (Some(id), Some(text)) = (m["id"].as_str(), b["text"].as_str()) {
                        out.push((id.to_string(), text.to_string()));
                    }
                }
            }
        }
        out
    };
    start_parts(pending, conv, ctx);
}

/// Starts rewrites of `parts` (id, text), but not of those under way.
pub fn start_parts(parts: Vec<(String, String)>, conv: &Arc<crate::store::Shared>, ctx: &eframe::egui::Context) {
    for (id, text) in parts {
        {
            let mut jobs = JOBS.lock().unwrap();
            if jobs.contains_key(&id) {
                continue;
            }
            jobs.insert(id.clone(), Job::default());
        }
        let (conv, ctx) = (conv.clone(), ctx.clone());
        std::thread::spawn(move || run(id, text, conv, ctx));
    }
}

/// The rewrite request for `text`.
fn body(text: &str) -> crate::oak::Request {
    crate::oak::Request {
        provider: crate::providers::Kind::OpenRouter,
        model: MODEL.into(),
        max_tokens: 16000,
        purpose: crate::oak::Purpose::Unslop,
        ttl: crate::oak::Ttl::Off,
        messages: vec![json!({"role": "user", "content": [crate::oak::text(&format!("{PROMPT}\n\n<input>\n{text}\n</input>"))]})],
        ..Default::default()
    }
}

/// What to show of the reply `raw`: what is inside `<output>`, without the
/// closing tag. While it streams (`done` false), nothing before `<output>`
/// comes, and the end is held back while it could be the start of
/// `</output>`. A finished reply without the tags is taken whole.
pub fn output(raw: &str, done: bool) -> String {
    const OPEN: &str = "<output>";
    const CLOSE: &str = "</output>";
    let body = match raw.find(OPEN) {
        Some(i) => &raw[i + OPEN.len()..],
        None if done => raw,
        None => return String::new(),
    };
    let mut body = match body.find(CLOSE) {
        Some(i) => &body[..i],
        None => body,
    };
    if !done {
        for n in (1..CLOSE.len()).rev() {
            if body.ends_with(&CLOSE[..n]) {
                body = &body[..body.len() - n];
                break;
            }
        }
    }
    split_bold_headers(body.trim())
}

/// A line that is all bold (`**Header**`, maybe with a colon after) with
/// text right under it gets a blank line after it, so it shows as a header
/// of its own instead of running into the paragraph (the rewrite model
/// writes them that way). Not inside code blocks.
fn split_bold_headers(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut fence = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if crate::instructions::is_fence(line) {
            fence = !fence;
        }
        out += line;
        if lines.peek().is_some() {
            out.push('\n');
            if !fence && is_bold_header(line) && lines.peek().is_some_and(|n| !n.trim().is_empty()) {
                out.push('\n');
            }
        }
    }
    out
}

/// A line that is all bold, maybe with a colon after: a header.
fn is_bold_header(l: &str) -> bool {
    let l = l.trim();
    let l = l.strip_suffix(':').unwrap_or(l);
    ["**", "__"].iter().any(|m| l.len() > 2 * m.len() && l.starts_with(m) && l.ends_with(m) && !l[m.len()..l.len() - m.len()].contains(m))
}

fn run(id: String, text: String, conv: Arc<crate::store::Shared>, ctx: eframe::egui::Context) {
    let (tx, rx) = std::sync::mpsc::channel();
    let req = body(&text);
    std::thread::spawn(move || crate::openrouter::stream(req, tx));
    let mut raw = String::new();
    let mut cost = 0.0;
    let mut result: Result<String, String> = Err("The rewrite ended early.".into());
    for m in rx {
        match m {
            Sse::Event(Event::Cost(c)) => cost += c,
            Sse::Event(Event::Text { text, .. }) => {
                raw += &text;
                let shown = output(&raw, false);
                if let Some(j) = JOBS.lock().unwrap().get_mut(&id) {
                    j.text = shown;
                }
                ctx.request_repaint();
            }
            Sse::Done => {
                let out = output(&raw, true);
                result = if out.is_empty() { Err("The rewrite came back empty.".into()) } else { Ok(out) };
                break;
            }
            Sse::Err(e) => {
                result = Err(e);
                break;
            }
            _ => {}
        }
    }
    // Into its block; a block still in the reply streaming in gets it when
    // the reply joins the conversation (`settle`, which looks under the
    // same lock).
    let mut c = conv.lock().unwrap();
    c.spent += cost;
    let mut jobs = JOBS.lock().unwrap();
    if apply(&mut c, &id, &result) || id.starts_with(NOTE) {
        jobs.remove(&id);
    } else if let Some(j) = jobs.get_mut(&id) {
        j.result = Some(result);
    }
    drop(jobs);
    c.save();
    drop(c);
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only what is inside `<output>` shows, without a partial closing tag,
    /// and nothing at all until the opening tag has streamed in.
    #[test]
    fn output_between_the_tags() {
        assert_eq!(output("Sure.\n<output>\nHello there.\n</output>", true), "Hello there.");
        assert_eq!(output("<output>Hello</out", false), "Hello");
        assert_eq!(output("<output>Hello <", false), "Hello");
        assert_eq!(output("<outp", false), "");
        assert_eq!(output("Plain reply", true), "Plain reply");
        assert_eq!(output("Plain reply", false), "");
        assert_eq!(output("<output>a < b</output> trailing", true), "a < b");
    }

    /// A bold header line gets a blank line after it, unless it is inside a
    /// code block or not the whole line.
    #[test]
    fn bold_headers_get_their_own_paragraph() {
        let f = |s: &str| output(&format!("<output>{s}</output>"), true);
        assert_eq!(f("**Prompt Update**\nI have updated it."), "**Prompt Update**\n\nI have updated it.");
        assert_eq!(f("**Notes:**\n- one"), "**Notes:**\n\n- one");
        assert_eq!(f("**Done**\n\nAlready apart."), "**Done**\n\nAlready apart.");
        assert_eq!(f("**a** and **b**\nnext"), "**a** and **b**\nnext");
        assert_eq!(f("```\n**x**\ny\n```"), "```\n**x**\ny\n```");
    }

    /// Marked text blocks are the ones that hold text, once, and the marker
    /// never reaches the model.
    #[test]
    fn marks_text_blocks_only_and_strips_before_sending() {
        let mut blocks = vec![
            json!({"type": "reasoning", "text": "t"}),
            json!({"type": "text", "text": "Done."}),
            json!({"type": "tool_call", "id": "x", "name": "READ", "input": {}}),
            json!({"type": "text", "text": "  "}),
        ];
        assert!(mark(&mut blocks));
        assert!(blocks[1][KEY]["id"].is_string());
        assert!(blocks[0].get(KEY).is_none() && blocks[2].get(KEY).is_none() && blocks[3].get(KEY).is_none());
        let id = blocks[1][KEY]["id"].clone();
        assert!(!mark(&mut blocks), "marked once");
        assert_eq!(blocks[1][KEY]["id"], id);
        let msg = json!({"role": "assistant", "content": blocks});
        assert!(!serde_json::to_string(&crate::rolling::view(&[msg], None)).unwrap().contains(KEY), "never sent to the model");
    }

    /// Thinking goes in sections: a header with its paragraph, sent once the
    /// next header is complete; short paragraphs run together up to SECTION.
    #[test]
    fn thinking_sections() {
        let text = "**Planning**\n\nI will read the files.\n\n**Reading**\n\nLooking at x.";
        let got = |from, done| sections(text, from, done).into_iter().map(|(s, e)| &text[s..e]).collect::<Vec<_>>();
        assert_eq!(got(0, false), ["**Planning**\n\nI will read the files."]);
        assert_eq!(got(0, true), ["**Planning**\n\nI will read the files.", "**Reading**\n\nLooking at x."]);
        // A header alone, or a paragraph not yet ended, waits.
        assert!(sections("**Planning**\n\n", 0, false).is_empty());
        // No headers (DeepSeek): paragraphs run together to SECTION bytes.
        let para = |c: char| c.to_string().repeat(400);
        let long = format!("{}\n\n{}\n\n{}\n\n", para('a'), para('b'), para('c'));
        let s = sections(&long, 0, false);
        assert_eq!(&long[s[0].0..s[0].1], format!("{}\n\n{}", para('a'), para('b')));
        // Multibyte text and odd spacing don't trip it.
        let odd = "  é ü\n\n\n\n**Ö**\n\nñ  \n\n";
        let s = sections(odd, 0, true);
        assert_eq!(s.iter().map(|&(a, b)| &odd[a..b]).collect::<Vec<_>>(), ["é ü", "**Ö**\n\nñ"]);
        assert!(sections(odd, 99, true).is_empty());
    }

    /// Streamed in pieces, a thinking block is marked section by section,
    /// each once, in order.
    #[test]
    fn thinking_marked_as_it_streams() {
        let full = "**One**\n\nFirst part.\n\n**Two**\n\nSecond part, longer.\n\n**Three**\n\nLast.";
        let mut b = json!({"type": "reasoning", "text": ""});
        let mut sent = vec![];
        for chunk in full.as_bytes().chunks(5) {
            b["text"] = json!(b["text"].as_str().unwrap().to_string() + std::str::from_utf8(chunk).unwrap());
            sent.extend(mark_thinking(&mut b, false));
        }
        assert_eq!(sent.len(), 2, "the last section waits for the block's end");
        sent.extend(mark_thinking(&mut b, true));
        assert!(mark_thinking(&mut b, true).is_empty(), "marked once");
        let texts: Vec<&str> = sent.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["**One**\n\nFirst part.", "**Two**\n\nSecond part, longer.", "**Three**\n\nLast."]);
        // Never sent to the model.
        let msg = json!({"role": "assistant", "content": [b]});
        assert!(!serde_json::to_string(&crate::rolling::view(&[msg], None)).unwrap().contains(KEY));
    }

    /// What a thinking row shows: the rewrites in order, a failed section's
    /// own text, and whether more is coming.
    #[test]
    fn thoughts_shown() {
        let text = "aaa\n\nbbb\n\nccc\n\nddd";
        let id_b = format!("t-{}", new_id());
        let b = json!({"type": "reasoning", "text": text, KEY: {"parts": [
            {"id": "x", "start": 0, "end": 3, "text": "A."},
            {"id": "y", "start": 5, "end": 8, "error": "HTTP 500"},
            {"id": id_b, "start": 10, "end": 13},
        ]}});
        JOBS.lock().unwrap().insert(id_b.clone(), Job { text: "C so f".into(), result: None });
        assert_eq!(
            thoughts(&b, true).unwrap(),
            Thoughts { text: "A.\n\nbbb\n\nC so f".into(), pending: true, errors: vec!["HTTP 500".into()] }
        );
        // Done, and no longer live: the unmarked rest shows as it is.
        JOBS.lock().unwrap().insert(id_b.clone(), Job { text: String::new(), result: Some(Ok("C.".into())) });
        assert_eq!(
            thoughts(&b, false).unwrap(),
            Thoughts { text: "A.\n\nbbb\n\nC.\n\nddd".into(), pending: false, errors: vec!["HTTP 500".into()] }
        );
        JOBS.lock().unwrap().remove(&id_b);
        assert!(thoughts(&json!({"type": "reasoning", "text": "t"}), true).is_none());
    }

    /// A section rewritten while its reply still streamed in lands once the
    /// reply is in the conversation; a note's lands in the note.
    #[test]
    fn finished_rewrites_settle() {
        let id = format!("t-{}", new_id());
        let block = json!({"type": "reasoning", "text": "plan", KEY: {"parts": [{"id": id, "start": 0, "end": 4}]}});
        let conv = Arc::new(crate::store::Shared::new(Conversation { deleted: true, ..Default::default() }));
        JOBS.lock().unwrap().insert(id.clone(), Job { text: String::new(), result: Some(Ok("My plan.".into())) });
        settle(&conv);
        assert!(JOBS.lock().unwrap().contains_key(&id), "kept until its block is in the conversation");
        conv.lock().unwrap().messages.push(json!({"role": "assistant", "content": [block]}));
        settle(&conv);
        assert!(!JOBS.lock().unwrap().contains_key(&id));
        let c = conv.lock().unwrap();
        assert_eq!(c.messages[0]["content"][0][KEY]["parts"][0]["text"], "My plan.");
        drop(c);

        let nid = format!("{NOTE}{}", new_id());
        let mut c = Conversation { deleted: true, ..Default::default() };
        c.notes.push(Note { id: 1, text: "step 2 of 5".into(), rewrite: Some(Rewrite { id: nid.clone(), ..Default::default() }) });
        assert!(apply(&mut c, &nid, &Ok("I am on step two of five.".into())));
        assert_eq!(c.notes[0].rewrite.as_ref().unwrap().text.as_deref(), Some("I am on step two of five."));
        assert!(!apply(&mut c, &format!("{NOTE}gone"), &Ok("x".into())));
    }
}
