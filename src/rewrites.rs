// Rewrites: what a plugin's rewriter (`oak_sdk::Rewriter`, Unslop's) shows
// in a conversation's transcript in place of the model's own words: its
// replies' text, its thinking and its notes, rewritten for the user. What
// the model is sent never changes: the rewrite lives in the text block's
// key (the rewriter's, `_oak_unslop` for Unslop's), which (like every
// `_oak_` key) is dropped before sending. The plugin does the rewriting
// (`Rewriting::rewrite`) and says what is rewritten now; Oak keeps the
// state, starts the work and shows it.
//
// The marker goes through three states:
//   {"id": "..."}                    being rewritten (the text streams into JOBS)
//   {"id": "...", "text": "..."}     rewritten
//   {"id": "...", "error": "..."}    the rewrite failed; the original shows
// Only replies finished while the rewriter rewrites replies are marked;
// switching it off leaves marked ones as they are. With No drafts, the
// transcript leaves out the "Drafted a message" rows too: a marked block
// shows only its rewrite (or the original, when the rewrite failed).
//
// Thinking is rewritten in sections while it streams in (`mark_thinking`),
// each as soon as the rewriter says it is complete (Unslop's: a paragraph,
// a bold header going with the one after it, short ones running together).
// Its marker lists them by byte range:
//   {"parts": [{"id", "start", "end", "text"?, "error"?}, ...]}
// A section's rewrite can finish while its block is still in the reply
// streaming in: it waits in JOBS until the reply joins the conversation
// (`settle`).
//
// Notes: each note is rewritten for the notes box (`Note::rewrite`,
// dropped when the note changes); the notes the model sees stay as they
// are. The rewriter's epoch (`Rewriting::epoch`) moving on drops the
// rewrites made under an earlier one: they are made again, in each
// conversation as it next shows them.

use crate::store::{Conversation, Note, Rewrite};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use oak_sdk::abi::{OAK_REWRITE_NO_DRAFTS, OAK_REWRITE_NOTES, OAK_REWRITE_REPLIES, OAK_REWRITE_THINKING};

/// What does the rewriting: a plugin's (`plugins::parts`), or a test's.
pub(crate) trait Rewriting: Send + Sync {
    /// The key its rewrites are kept under in a block.
    fn key(&self) -> &'static str;
    /// What it rewrites now: `OAK_REWRITE_*`.
    fn bits(&self) -> u32;
    /// The notes' rewrites from another epoch are made again.
    fn epoch(&self) -> u32;
    /// The complete sections of thinking `text` to rewrite now (all of it,
    /// `done`), as byte ranges of it. None: it has no say.
    fn sections(&self, text: &str, done: bool) -> Option<Vec<(usize, usize)>>;
    /// Rewrites `text`, telling `progress` the rewrite so far: the rewrite
    /// or why it failed, and what it cost (dollars).
    fn rewrite(&self, text: &str, progress: &mut dyn FnMut(&str)) -> (Result<String, String>, f64);
}

#[cfg(not(test))]
fn rewriter() -> Option<&'static dyn Rewriting> {
    crate::plugins::parts().rewriters.first().map(|r| &**r as &dyn Rewriting)
}

/// The rewriter tests use: Unslop's key, and switches per test thread (so
/// a test turning them on doesn't reach sessions that other tests run
/// meanwhile). Its sections are paragraphs; it doesn't rewrite.
#[cfg(test)]
pub(crate) struct TestRewriter;

#[cfg(test)]
thread_local! {
    static TEST_BITS: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// Sets the test rewriter's switches (`OAK_REWRITE_*`) and notes epoch, on
/// this thread.
#[cfg(test)]
pub(crate) fn set_test(bits: u32, epoch: u32) {
    TEST_BITS.with(|b| b.set((bits, epoch)));
}

#[cfg(test)]
impl Rewriting for TestRewriter {
    fn key(&self) -> &'static str {
        "_oak_unslop"
    }

    fn bits(&self) -> u32 {
        TEST_BITS.with(|b| b.get().0)
    }

    fn epoch(&self) -> u32 {
        TEST_BITS.with(|b| b.get().1)
    }

    fn sections(&self, text: &str, done: bool) -> Option<Vec<(usize, usize)>> {
        let mut out = vec![];
        let mut pos = 0;
        let mut ends: Vec<usize> = text.match_indices("\n\n").map(|(i, _)| i).collect();
        if done {
            ends.push(text.len());
        }
        for end in ends {
            if !text[pos..end].trim().is_empty() {
                out.push((pos, end));
            }
            pos = end + 2;
        }
        Some(out)
    }

    fn rewrite(&self, _: &str, _: &mut dyn FnMut(&str)) -> (Result<String, String>, f64) {
        (Err("Tests don't rewrite.".into()), 0.0)
    }
}

#[cfg(test)]
fn rewriter() -> Option<&'static dyn Rewriting> {
    Some(&TestRewriter)
}

/// The key rewrites are kept under in a block ("" with no rewriter: no
/// block has it).
pub fn key() -> &'static str {
    rewriter().map_or("", |r| r.key())
}

fn bit(b: u32) -> bool {
    rewriter().is_some_and(|r| r.bits() & b != 0)
}

/// Replies are rewritten as they end (and with it, what the other
/// switches turn on).
pub fn on() -> bool {
    bit(OAK_REWRITE_REPLIES)
}

/// No drafts: the transcript shows a marked block's rewrite alone, without
/// the "Drafted a message" row over it.
pub fn no_drafts() -> bool {
    bit(OAK_REWRITE_NO_DRAFTS)
}

/// Thinking is rewritten as it streams in.
pub fn thinking() -> bool {
    bit(OAK_REWRITE_THINKING)
}

/// The notes box shows the notes rewritten.
fn notes_switched_on() -> bool {
    bit(OAK_REWRITE_NOTES)
}

/// The notes epoch: a note's rewrite from another is stale.
fn epoch() -> u32 {
    rewriter().map_or(0, |r| r.epoch())
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
#[derive(Debug, PartialEq)]
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

/// The state of a block whose rewriter's field is `m`.
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
    let k = key();
    if k.is_empty() {
        return false;
    }
    let mut any = false;
    for b in blocks.iter_mut().filter(|b| b["type"] == "text" && b.get("_oak_file").is_none()) {
        if b["text"].as_str().is_some_and(|t| !t.trim().is_empty()) && b.get(k).is_none() {
            b[k] = json!({ "id": new_id() });
            any = true;
        }
    }
    any
}

/// Rewrite thinking: marks the sections of the thinking block `b` that are
/// complete and not marked yet (all the rest, `done`: the block has ended),
/// as the rewriter divides it, and returns them, as (id, text), for
/// `start_parts`.
pub fn mark_thinking(b: &mut Value, done: bool) -> Vec<(String, String)> {
    let Some(r) = rewriter() else { return vec![] };
    let k = r.key();
    let text = b["text"].as_str().unwrap_or("");
    let from = b[k]["parts"].as_array().and_then(|p| p.last()).and_then(|p| p["end"].as_u64()).unwrap_or(0) as usize;
    let new: Vec<(String, usize, usize, String)> =
        sections(r, text, from, done).into_iter().map(|(s, e)| (new_id(), s, e, text[s..e].to_string())).collect();
    if new.is_empty() {
        return vec![];
    }
    let parts: Vec<Value> = new.iter().map(|(id, s, e, _)| json!({"id": id, "start": s, "end": e})).collect();
    let mut parts = Some(parts);
    if let Some(a) = b[k]["parts"].as_array_mut() {
        a.extend(parts.take().unwrap_or_default());
    }
    if let Some(parts) = parts {
        b[k] = json!({ "parts": parts });
    }
    new.into_iter().map(|(id, _, _, t)| (id, t)).collect()
}

/// The sections of `text` from byte `from` on to rewrite now, as byte
/// ranges of `text`: the rewriter's, or (when it has no say) the rest,
/// trimmed, once it is `done`.
fn sections(r: &dyn Rewriting, text: &str, from: usize, done: bool) -> Vec<(usize, usize)> {
    let Some(rest) = text.get(from..) else { return vec![] };
    let mut spans = r.sections(rest, done).unwrap_or_else(|| {
        let lead = rest.len() - rest.trim_start().len();
        if done && !rest.trim().is_empty() { vec![(lead, rest.trim_end().len())] } else { vec![] }
    });
    // In order, apart, each with something in it: what the rewriter got
    // wrong goes.
    spans.sort();
    let mut end = 0;
    spans.retain(|&(a, b)| {
        let keep = a >= end && rest.get(a..b).is_some_and(|t| !t.trim().is_empty());
        if keep {
            end = b;
        }
        keep
    });
    spans.into_iter().map(|(a, b)| (from + a, from + b)).collect()
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
    let parts = b.get(key()).filter(|_| !key().is_empty())?.get("parts")?.as_array()?;
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
        // Marked (again) on the next frame (`start_notes`).
        None => State::Streaming(String::new()),
        Some(r) if r.epoch != epoch() => State::Streaming(String::new()),
        Some(Rewrite { text: Some(t), .. }) => State::Done(t),
        Some(Rewrite { error: Some(e), .. }) => State::Failed(e),
        Some(r) => State::Streaming(job_text(&r.id)),
    })
}

/// Rewrite notes: starts rewrites of the notes of `conv` that have none
/// (new, or changed since) or only one from an earlier epoch, and again of
/// any left unfinished when Oak closed. Cheap with nothing to do, so it
/// runs every frame.
pub fn start_notes(conv: &Arc<crate::store::Shared>, ctx: &eframe::egui::Context) {
    let todo = {
        let mut c = conv.lock().unwrap();
        if !(notes_switched_on() && applies(&c)) {
            return;
        }
        let (todo, marked) = notes_to_do(&mut c);
        if marked {
            c.save();
        }
        todo
    };
    start_parts(todo, conv, ctx);
}

/// The notes of `c` to rewrite (marker id, text), the ones with no rewrite
/// of this epoch marked first; and whether any were.
fn notes_to_do(c: &mut Conversation) -> (Vec<(String, String)>, bool) {
    let mut todo = vec![];
    let mut marked = false;
    let epoch = epoch();
    for n in c.notes.iter_mut() {
        match &n.rewrite {
            // (A stale one's rewrite under way, if any, then finds no place
            // to land and is dropped: `run`.)
            r if r.as_ref().is_none_or(|r| r.epoch != epoch) => {
                let id = format!("{NOTE}{}", new_id());
                n.rewrite = Some(Rewrite { id: id.clone(), epoch, ..Default::default() });
                todo.push((id, n.text.clone()));
                marked = true;
            }
            Some(r) if r.text.is_none() && r.error.is_none() && !JOBS.lock().unwrap().contains_key(&r.id) => {
                todo.push((r.id.clone(), n.text.clone()));
            }
            _ => {}
        }
    }
    (todo, marked)
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
    let k = key();
    if k.is_empty() {
        return false;
    }
    let set = |m: &mut Value| match result {
        Ok(t) => m["text"] = json!(t),
        Err(e) => m["error"] = json!(e),
    };
    // Found first, so only the message with it counts as changed (its
    // saved JSON is redone, `store::Messages`).
    let has = |m: &Value| {
        m["content"].as_array().into_iter().flatten().filter_map(|b| b.get(k)).any(|m| m["id"] == id || m["parts"].as_array().is_some_and(|ps| ps.iter().any(|p| p["id"] == id)))
    };
    // (In what is held, or else in what is on disk.)
    let i = match c.messages.iter().rposition(has) {
        Some(i) => i,
        None if c.messages.base() > 0 => {
            c.messages.load_all();
            let Some(i) = c.messages.iter().rposition(has) else { return false };
            i
        }
        None => return false,
    };
    for b in c.messages[i]["content"].as_array_mut().into_iter().flatten() {
        let Some(m) = b.get_mut(k) else { continue };
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

/// Looks for rewrites still to do in a conversation as it is read from its
/// file (`raw`, its messages' JSON): only messages with a marker are parsed.
/// (Inside a JSON string a quote is escaped: only a key, or a string of
/// just the key, matches.)
/// With no rewriter there is nothing to look for (and `start` then reads
/// nothing): the index keeps what was found under the rewriter's key
/// (`store::INDEX`), so files are looked at again when it changes.
pub fn look(c: &mut crate::store::Conversation, raw: &[&[u8]]) {
    if key().is_empty() {
        c.messages.rewrites_from = None;
        return;
    }
    let key = format!("\"{}\"", key());
    let finder = memchr::memmem::Finder::new(key.as_bytes());
    c.messages.rewrites_from = raw.iter().position(|m| finder.find(m).is_some() && serde_json::from_slice::<Value>(m).map_or(true, |m| has_marker(&m)));
}

/// Whether message `m` has a rewrite still to do (`start` would start it):
/// a marker without its text or an error, in a reply.
pub fn has_marker(m: &Value) -> bool {
    let k = key();
    if k.is_empty() {
        return false;
    }
    let open = |m: &Value| m.get("text").is_none() && m.get("error").is_none();
    m["role"] == "assistant"
        && m["content"].as_array().is_some_and(|c| {
            c.iter().any(|b| {
                b.get(k).filter(|m| open(m)).is_some_and(|m| match m["parts"].as_array() {
                    Some(parts) => parts.iter().any(open),
                    None => true,
                })
            })
        })
}

/// Settles finished rewrites, then starts one for each marked block of
/// `conv` (text, or a thinking section) that has none yet and none under
/// way (just marked, or left unfinished when Oak closed).
pub fn start(conv: &Arc<crate::store::Shared>, ctx: &eframe::egui::Context) {
    // Not yet read from its file, and with no marker in it: nothing to do,
    // and nothing read (every conversation starts here, at every start).
    // Otherwise the messages from the first marker are read.
    {
        let mut c = conv.lock().unwrap();
        if c.messages.rewrites_from.is_none() && !c.messages.loaded() && JOBS.lock().unwrap().is_empty() {
            return;
        }
        if let Some(i) = c.messages.rewrites_from.take() {
            c.messages.load_from(i);
        }
    }
    settle(conv);
    let k = key();
    if k.is_empty() {
        return;
    }
    // (The markers without a text or an error, with what to rewrite.)
    let pending: Vec<(String, String)> = {
        let c = conv.lock().unwrap();
        let open = |m: &Value| m.get("text").is_none() && m.get("error").is_none();
        let mut out = vec![];
        for b in c.messages.iter().filter(|m| m["role"] == "assistant").flat_map(|m| m["content"].as_array().into_iter().flatten()) {
            let Some(m) = b.get(k).filter(|m| open(m)) else { continue };
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

/// Rewrites `id`'s `text` with the rewriter, and puts the rewrite in its
/// place.
fn run(id: String, text: String, conv: Arc<crate::store::Shared>, ctx: eframe::egui::Context) {
    let (result, cost) = match rewriter() {
        Some(r) => r.rewrite(&text, &mut |so_far: &str| {
            if let Some(j) = JOBS.lock().unwrap().get_mut(&id) {
                j.text = so_far.to_string();
            }
            ctx.request_repaint();
        }),
        None => (Err("The plugin that rewrote it is gone.".into()), 0.0),
    };
    let result = result.and_then(|t| if t.trim().is_empty() { Err("The rewrite came back empty.".into()) } else { Ok(t) });
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

    const KEY: &str = "_oak_unslop";

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

    /// Thinking goes in the rewriter's sections, from where the marked
    /// ones end; without its say, whole once it ends. Sections that overlap
    /// or hold nothing go.
    #[test]
    fn thinking_sections() {
        struct Says(Option<Vec<(usize, usize)>>);
        impl Rewriting for Says {
            fn key(&self) -> &'static str {
                KEY
            }
            fn bits(&self) -> u32 {
                0
            }
            fn epoch(&self) -> u32 {
                0
            }
            fn sections(&self, _: &str, _: bool) -> Option<Vec<(usize, usize)>> {
                self.0.clone()
            }
            fn rewrite(&self, _: &str, _: &mut dyn FnMut(&str)) -> (Result<String, String>, f64) {
                (Err(String::new()), 0.0)
            }
        }
        let text = "  é ü\n\nmore  ";
        assert!(sections(&Says(None), text, 0, false).is_empty(), "no say: waits for the end");
        assert_eq!(sections(&Says(None), text, 0, true), [(2, 13)]);
        assert_eq!(sections(&Says(None), text, 9, true), [(9, 13)]);
        assert!(sections(&Says(None), text, 99, true).is_empty());
        let says = Says(Some(vec![(4, 6), (0, 3), (2, 4), (5, 5), (6, 7), (7, 99)]));
        assert_eq!(sections(&says, "abcdefghi", 1, false), [(1, 4), (5, 7), (7, 8)]);
    }

    /// Streamed in pieces, a thinking block is marked section by section
    /// (the test rewriter's: paragraphs), each once, in order.
    #[test]
    fn thinking_marked_as_it_streams() {
        let full = "**One**\n\nFirst part.\n\n**Two**\n\nSecond part, longer.\n\n**Three**\n\nLast.";
        let mut b = json!({"type": "reasoning", "text": ""});
        let mut sent = vec![];
        for chunk in full.as_bytes().chunks(5) {
            b["text"] = json!(b["text"].as_str().unwrap().to_string() + std::str::from_utf8(chunk).unwrap());
            sent.extend(mark_thinking(&mut b, false));
        }
        assert_eq!(sent.len(), 5, "the last section waits for the block's end");
        sent.extend(mark_thinking(&mut b, true));
        assert!(mark_thinking(&mut b, true).is_empty(), "marked once");
        let texts: Vec<&str> = sent.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["**One**", "First part.", "**Two**", "Second part, longer.", "**Three**", "Last."]);
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

    /// A new epoch: the notes' rewrites made before stop showing and are
    /// made again; failed ones too.
    #[test]
    fn note_rewrites_clear_with_a_new_epoch() {
        set_test(OAK_REWRITE_REPLIES | OAK_REWRITE_NOTES, 0);
        let mut c = Conversation { deleted: true, ..Default::default() };
        c.notes.push(Note { id: 1, text: "a".into(), rewrite: Some(Rewrite { id: "note-old1".into(), text: Some("A.".into()), ..Default::default() }) });
        c.notes.push(Note { id: 2, text: "b".into(), rewrite: Some(Rewrite { id: "note-old2".into(), error: Some("HTTP 500".into()), ..Default::default() }) });
        assert_eq!(notes_to_do(&mut c), (vec![], false), "this epoch's, done or failed, stay");
        assert_eq!(note_state(&c.notes[0], true), Some(State::Done("A.")));

        set_test(OAK_REWRITE_REPLIES | OAK_REWRITE_NOTES, 1);
        assert_eq!(note_state(&c.notes[0], true), Some(State::Streaming(String::new())), "stale: shows as being rewritten");
        let (todo, marked) = notes_to_do(&mut c);
        assert!(marked);
        assert_eq!(todo.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        for (n, (id, _)) in c.notes.iter().zip(&todo) {
            let r = n.rewrite.as_ref().unwrap();
            assert_eq!((&r.id, r.epoch, &r.text, &r.error), (id, 1, &None, &None));
        }
        // An old rewrite finishing late finds no place.
        assert!(!apply(&mut c, "note-old1", &Ok("A.".into())));
        set_test(0, 0);
    }
}
