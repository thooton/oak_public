//! The transcript: a conversation's messages, tool rows, marks and notices as
//! they are drawn, and the cards at its end (ASK, REQUEST_ASK, the queue).

use super::*;
pub(crate) use oak_sdk::ui::{ROW_H, size_text};
use crate::widgets::{chevron, clip_width, draw_pencil, draw_x, icon_button, paint_pause};
use crate::theme::{hex, mix};

/// Every tool call's result, by call id (`items_at`), hashed with
/// `QuickHasher`: the map is built every frame.
pub(crate) type Results<'a> = HashMap<&'a str, ToolOut<'a>, QuickState>;

/// A tool result as the transcript shows it.
pub(crate) struct ToolOut<'a> {
    /// Borrowed from the conversation, unless its blocks had to be joined.
    text: std::borrow::Cow<'a, str>,
    images: Vec<&'a Value>,
    error: bool,
    /// The files a DISPLAY call shows (see `tools::display`).
    display: &'a [Value],
    /// The model sees it cleared (`rolling::CLEARED` in its place).
    pub(crate) cleared: bool,
}

/// Bytes that start like an image file (for a dropped file without an image extension).
fn image_like(b: &[u8]) -> bool {
    image::guess_format(b).is_ok()
}

/// The largest PDF sent as a document (the API takes requests up to 32 MB).
const MAX_PDF: usize = 20_000_000;
/// The largest text file sent as a document.
const MAX_TEXT_FILE: usize = 500_000;

/// An attached file as a content block: an image as an image, a PDF or a
/// text file as a document, anything else as a note with its path (the
/// model can't take the bytes, but can open the file with its tools). The
/// file's name goes in `_oak_file` (dropped before sending) for the UI.
pub(crate) fn file_block(path: &std::path::Path, bytes: &[u8]) -> Result<Value, String> {
    use base64::Engine;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    if images::is_image_path(path) || image_like(bytes) {
        return images::from_bytes(bytes).map(|i| i.block());
    }
    if bytes.starts_with(b"%PDF") && bytes.len() <= MAX_PDF {
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        return Ok(serde_json::json!({"type": "document", "media_type": "application/pdf", "data": data, "title": name, "_oak_file": name}));
    }
    if bytes.len() <= MAX_TEXT_FILE
        && let Ok(text) = std::str::from_utf8(bytes)
        && !text.contains('\0')
    {
        let text = if text.trim().is_empty() { "(empty file)" } else { text };
        return Ok(serde_json::json!({"type": "document", "media_type": "text/plain", "data": text, "title": name, "_oak_file": name}));
    }
    // (Never empty: an empty file is text.)
    let text = format!(
        "[Attached file: {} ({}). It isn't included here (too big, or not text, an image or a PDF); open it from disk if you need it.]",
        path.display(),
        size_text(bytes.len() as u64)
    );
    // (`_oak_path`, never sent to a model: a plugin's tab may upload the
    // file itself.)
    Ok(serde_json::json!({"type": "text", "text": text, "_oak_file": name, "_oak_path": path.display().to_string()}))
}

/// An attachment as the UI shows it: an image as a thumbnail `h` tall,
/// any other file as a chip with its name.
pub(crate) fn attachment_view(ui: &mut Ui, t: &Theme, block: &Value, h: f32) -> Option<egui::Response> {
    if block["type"] == "image" {
        return images::show(ui, block, h);
    }
    Some(file_chip(ui, t, block))
}

/// A box with a file's name in it.
fn file_chip(ui: &mut Ui, t: &Theme, block: &Value) -> egui::Response {
    let name = block["_oak_file"].as_str().or(block["title"].as_str()).unwrap_or("file");
    let icons = markdown::icon_font(14.0);
    let what = match block["media_type"].as_str() {
        Some("application/pdf") => "PDF, sent as a document",
        Some(_) => "text, sent as a document",
        None => "sent as its path: the model can open it from disk",
    };
    egui::Frame::new()
        .fill(t.panel)
        .stroke(Stroke::new(1.0, t.line))
        .inner_margin(Margin { left: 8, right: 24, top: 6, bottom: 6 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(FILE_ICON).font(icons).color(t.ink3));
                ui.add(egui::Label::new(RichText::new(name).small().color(t.ink2)).truncate());
            });
        })
        .response
        .on_hover_text(format!("{name}\n{what}"))
}

/// Nerd Font: a file.
const FILE_ICON: &str = "\u{ea7b}";

pub(crate) fn tool_summary(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_string();
    match name {
        // The model's summary of the command, as it gave it (the tense
        // stays); the command itself for calls from before summaries
        // (and BASH, EXECUTE's old name).
        // Cloud sessions' Bash calls (Claude Code's names) give theirs as
        // the description.
        "EXECUTE" | "TASK_NEW" | "BASH" | "Bash" => {
            let summary = if name == "Bash" { s("description") } else { s("summary") };
            if summary.trim().is_empty() { s("command") } else { crate::tools::one_line(&summary) }
        }
        "READ" | "WRITE" | "EDIT" => s("path"),
        // Claude Code's tools, in cloud sessions.
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => if input["file_path"].is_string() { s("file_path") } else { s("notebook_path") },
        "Grep" | "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Agent" | "Task" => s("description"),
        "TodoWrite" => format!("{} to-dos", input["todos"].as_array().map_or(0, Vec::len)),
        "SEARCH" => s("objective"),
        "TASK_GET_NEW_OUTPUT" | "TASK_DELETE" => format!("task {}", input["id"]),
        "TASK_WAIT" => format!("task {}, up to {}s", input["id"], input["timeout"]),
        "NOTE_NEW" => s("text"),
        "NOTE_UPDATE" => format!("note {}: {}", input["id"], s("text")),
        "SET_TITLE" => s("title"),
        "NOTE_DELETE" => format!("note {}", input["id"]),
        "HEARTBEAT_START" => format!("every {}: {}", agent::every(input["interval_minutes"].as_u64().unwrap_or(0)), s("message")),
        "HEARTBEAT_STOP" => format!("heartbeat {}", input["id"]),
        "PROGRESS_SET" => format!("{}, every {}", s("label"), agent::every(input["interval_minutes"].as_u64().unwrap_or(0))),
        "PROGRESS_REMOVE" => format!("meter {}", input["id"]),
        "FETCH" | "SCREENSHOT" | "BROWSER_GOTO" => s("url"),
        "BROWSER_CLICK" => match input["ref"].as_u64() {
            Some(n) => format!("[{n}]"),
            None => format!("{}, {}", input["x"], input["y"]),
        },
        "BROWSER_TYPE" => match input["ref"].as_u64() {
            Some(n) => format!("[{n}] {}", s("text")),
            None => s("text"),
        },
        "BROWSER_KEY" => s("key"),
        "BROWSER_EVAL" => crate::tools::one_line(&s("script")),
        "BROWSER_SCROLL" if input["dy"].is_number() => format!("{} px", input["dy"]),
        "BROWSER_UPLOAD" => input["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).collect::<Vec<_>>().join(", "),
        "AGENT_NEW" => s("name"),
        "AGENT_SEND" => {
            // A subagent's names it `id`; an Oak tab agent's names another `agent`.
            let to = match input["id"].as_u64().or_else(|| input["agent"].as_u64()) {
                Some(0) => agent::MAIN_LABEL.to_string(),
                Some(n) => format!("agent {n}"),
                None => "?".into(),
            };
            format!("to {to}: {}", crate::tools::one_line(&s("message")))
        }
        "AGENT_WAIT" => format!("up to {}s", input["timeout"]),
        "AGENT_STOP" | "AGENT_DELETE" => format!("agent {}", input["id"]),
        "REQUEST_ASK" => crate::tools::one_line(&s("about")),
        "ASK" => {
            let qs = input["questions"].as_array().map(Vec::as_slice).unwrap_or_default();
            let first = qs.first().and_then(|q| q["question"].as_str()).unwrap_or("");
            if qs.len() > 1 { format!("{first} (+{} more)", qs.len() - 1) } else { first.to_string() }
        }
        _ => String::new(),
    }
}

/// A "copy" button at the top right of a tool box (`inner`: its content),
/// shown while the pointer is over the box, that copies `text`. It reads
/// "copied" for a moment after. Drawn over the box, so selection stays.
fn copy_corner(ui: &mut Ui, t: &Theme, inner: Rect, text: &str) {
    let id = ui.scope_id().with("box_copy");
    let at = ui.data(|d| d.get_temp::<f64>(id));
    let now = ui.input(|i| i.time);
    let copied = at.is_some_and(|a| now - a < 1.5);
    let area = inner.expand(10.0);
    let r = Rect::from_min_size(egui::pos2(area.right() - 52.0, area.top() + 4.0), vec2(48.0, 18.0));
    let resp = ui.interact(r, id, Sense::click());
    if !copied && !ui.rect_contains_pointer(area) {
        return;
    }
    let p = ui.painter();
    p.rect_filled(r, 3.0, t.panel2);
    p.rect_stroke(r, 3.0, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
    let color = if copied { t.accent } else if resp.hovered() { t.ink } else { t.ink3 };
    p.text(r.center(), egui::Align2::CENTER_CENTER, if copied { "copied" } else { "copy" }, egui::FontId::proportional(11.0), color);
    if resp.on_hover_text("Copy to clipboard").clicked() {
        ui.ctx().copy_text(text.trim_end().to_string());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if copied {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
}

/// A tool's input or output, set into the tool block in the page color.
fn code_block(ui: &mut Ui, t: &Theme, text: &str, color: Color32) {
    crate::widgets::inset(ui, t, t.bg, 10, |ui| {
        ui.label(RichText::new(text.trim_end()).font(markdown::code_font(12.5)).color(color));
        copy_corner(ui, t, ui.min_rect(), text);
    });
}

/// A SEARCH's results as a search engine page: each title a link, its
/// site under it, then its summary.
fn search_view(ui: &mut Ui, t: &Theme, hits: &[(String, String, String)]) {
    crate::widgets::inset(ui, t, t.bg, 10, |ui| {
        if hits.is_empty() {
            ui.label(RichText::new("No results").color(t.ink3));
        }
        for (i, (title, url, summary)) in hits.iter().enumerate() {
            if i > 0 {
                ui.add_space(8.0);
            }
            ui.hyperlink_to(RichText::new(title).strong(), url).on_hover_text(url);
            let site = url.split_once("://").map_or(url.as_str(), |(_, r)| r).trim_end_matches('/');
            caption(ui, site, t.ink3);
            if !summary.is_empty() {
                ui.label(RichText::new(summary).color(t.ink2));
            }
        }
    });
}

/// Removed and added lines' backgrounds in a diff, and their signs.
const DIFF_RED: u32 = 0xe5534b;
const DIFF_GREEN: u32 = 0x57ab5a;

/// The language of a command's output, when it is clearly code: a file
/// printed whole (`cat`, `head`, `tail`, `type`, `sed -n` of one file,
/// whose name tells), or output that reads as code.
fn output_syntax(command: &str, out: &str) -> Option<highlight::Syntax> {
    let simple = !command.contains(['|', ';', '>', '<']) && !command.contains("&&");
    let words: Vec<&str> = command.split_whitespace().collect();
    if simple
        && let (Some(first), Some(last)) = (words.first(), words.last())
        && matches!(*first, "cat" | "head" | "tail" | "type" | "sed" | "bat" | "less" | "more")
        && words.len() >= 2
        && let Some(s) = highlight::for_path(last.trim_matches(['"', '\'']))
    {
        return Some(s);
    }
    highlight::detect(out)
}

/// READ's output: the line numbers in a faint gutter, the lines colored
/// as code when they are. False if the output isn't numbered lines (a
/// note, an image's description), to be shown as text instead.
fn read_view(ui: &mut Ui, t: &Theme, path: &str, out: &str) -> bool {
    let body = out.trim_end();
    // "[N more lines; use offset …]" goes under the code, as a note.
    let (body, note) = match body.rfind("\n\n[") {
        Some(i) if body.ends_with(']') => (&body[..i], Some(body[i + 2..].trim())),
        _ => (body, None),
    };
    let mut gutter = vec![];
    let mut code = String::new();
    let lines: Vec<&str> = body.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        let numbered = line.split_once('\t').map(|(n, rest)| (n.trim_start(), rest)).filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        let Some((n, rest)) = numbered else {
            // (The last line may be cut short by the length shown.)
            if i + 1 == lines.len() && i > 0 {
                break;
            }
            return false;
        };
        gutter.push(n);
        if gutter.len() > 1 {
            code.push('\n');
        }
        code.push_str(rest);
    }
    if gutter.is_empty() {
        return false;
    }
    let syntax = highlight::for_file(path, &code);
    let font = markdown::code_font(12.5);
    crate::widgets::inset(ui, t, t.bg, 10, |ui| {
        let mut job = highlight::numbered_job(&gutter, &code, syntax.as_ref(), &font, &highlight::Palette::for_theme(t), t.ink4);
        job.wrap.max_width = ui.available_width();
        ui.add(egui::Label::new(job).selectable(true));
        if let Some(note) = note {
            caption(ui, note, t.ink3);
        }
        copy_corner(ui, t, ui.min_rect(), &code);
    });
    true
}

/// Code colored for `syntax` (None: plain), in a block like `code_block`;
/// its copy button copies `copy`, else the shown text. Diff parts (an
/// EDIT's old and new text) get a red or green background across the
/// block.
fn code_view(ui: &mut Ui, t: &Theme, syntax: Option<highlight::Syntax>, parts: &[highlight::Part], copy: Option<&str>) {
    let font = markdown::code_font(12.5);
    crate::widgets::inset(ui, t, t.bg, 10, |ui| {
        let (mut job, kinds) = highlight::job(parts, syntax.as_ref(), &font, &highlight::Palette::for_theme(t));
        job.wrap.max_width = ui.available_width();
        let galley = ui.painter().layout_job(job);
        // The backgrounds go under the text, so their slot comes first.
        let bg = ui.painter().add(egui::Shape::Noop);
        let resp = ui.add(egui::Label::new(galley.clone()));
        let (red, green) = (mix(t.bg, hex(DIFF_RED), 0.16), mix(t.bg, hex(DIFF_GREEN), 0.16));
        let (left, right) = (ui.min_rect().left() - 10.0, ui.min_rect().right() + 10.0);
        let mut shapes = vec![];
        let mut line = 0;
        for row in &galley.rows {
            if let Some(Some(added)) = kinds.get(line) {
                let r = row.rect().translate(resp.rect.min.to_vec2());
                let r = Rect::from_x_y_ranges(left..=right, r.top()..=r.bottom());
                shapes.push(egui::Shape::rect_filled(r, 0.0, if *added { green } else { red }));
            }
            if row.ends_with_newline {
                line += 1;
            }
        }
        ui.painter().set(bg, egui::Shape::Vec(shapes));
        let text: String = parts.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n");
        copy_corner(ui, t, ui.min_rect(), copy.unwrap_or(&text));
    });
}

thread_local! {
    /// The conversation whose transcript is being drawn (hashed), set by
    /// `for_conversation`: ids made from a reply's place in it (`key_id`)
    /// include it, so a side chat, whose replies keep their source's
    /// places, never shares a widget with its source beside it.
    static SALT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Draws with ids keyed to the conversation `id` (see `SALT`).
pub(crate) fn for_conversation<R>(id: &str, f: impl FnOnce() -> R) -> R {
    let salt = Id::unique(id).value();
    let before = SALT.with(|c| c.replace(salt));
    let r = f();
    SALT.with(|c| c.set(before));
    r
}

/// The id named `what` for the item at `key` in this conversation.
fn key_id(what: &str, key: (usize, usize)) -> Id {
    Id::unique((what, key, SALT.with(|c| c.get())))
}

/// Draws a reply's text as markdown.
pub(crate) fn markdown(ui: &mut Ui, t: &Theme, text: &str, key: (usize, usize)) {
    let id = key_id("markdown", key);
    crate::select::message(ui, id, || markdown::plain_text(text), |ui| markdown::show(ui, t, text, id));
}

/// Draws a reply's text as markdown, its code blocks colored for their
/// language (the Duck.ai page's replies).
pub(crate) fn markdown_highlighted(ui: &mut Ui, t: &Theme, text: &str, key: (usize, usize)) {
    let id = key_id("markdown", key);
    crate::select::message(ui, id, || markdown::plain_text(text), |ui| markdown::show_highlighted(ui, t, text, id));
}

/// One thing the transcript shows, in order.
pub(crate) enum Item<'a> {
    /// Smart context summarized the conversation here: the summary, the
    /// message it stands in for everything before (the cut), and when it
    /// was made (which tells banners apart).
    Summary { text: &'a str, cut: usize, time: i64 },
    /// A task notification and a heartbeat, as sent to the model.
    Note(&'a str),
    Beat(&'a str),
    /// The user's answer to a REQUEST_ASK, as sent to the model.
    AskReply(&'a str),
    /// A message from another session (a subagent, or for a subagent the
    /// main session or another agent); `last` marks a final reply.
    AgentMessage { from: &'a str, last: bool, body: &'a str, key: (usize, usize) },
    /// A /goal or /effort taking effect, or a usage limit; never sent.
    Mark(&'a Mark),
    /// What the user wrote; the last item of each message the user sent
    /// carries where a revert to before that message goes back to. True
    /// when it ends the user's turn (`mark_turn_ends`).
    UserText(&'a str, Option<RevertAt<'a>>, Option<i64>, bool),
    Image(&'a Value, Option<RevertAt<'a>>),
    /// An attached file that isn't an image.
    File(&'a Value, Option<RevertAt<'a>>),
    /// A reply's text, when it came (none while it streams in, or from
    /// before OAK kept the time), the reply's output speed in tokens per
    /// second (when the provider reported its output token count); true
    /// with Unslop, how its rewrite stands; true when it ends the model's
    /// turn.
    Text(&'a str, (usize, usize), Option<i64>, Option<f64>, Option<unslop::State<'a>>, bool),
    /// A thinking block; true while it is still streaming; with Rewrite
    /// thinking, how its rewrite stands.
    Thinking(&'a Value, (usize, usize), bool, Option<unslop::Thoughts>),
    /// A tool call, and its message and block.
    Tool(&'a Value, (usize, usize)),
    /// A DISPLAY call: the files it shows, in the chat itself.
    Display(&'a Value, (usize, usize)),
}

/// Where reverting to before a user's message goes back to: its first
/// block (message, block) and that block, which must still be there.
#[derive(Clone, Copy)]
pub(crate) struct RevertAt<'a> {
    pub(crate) msg: usize,
    pub(crate) block: usize,
    first: &'a Value,
}

/// `draft`: the reply streaming in will be rewritten (Unslop): its text
/// once done, its thinking (with Rewrite thinking) as it comes.
fn push_assistant<'a>(out: &mut Vec<Item<'a>>, b: &'a Value, key: (usize, usize), live: bool, draft: bool) {
    let [kind, text, name, at, tps, ms, rewrite] = crate::fields(b, ["type", "text", "name", "_oak_at", "_oak_tps", "_oak_ms", unslop::KEY]);
    let text = text.and_then(Value::as_str);
    match kind.and_then(Value::as_str).unwrap_or("") {
        "text" => {
            if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                let rewrite = rewrite.map(unslop::state_of).or(if draft { Some(unslop::State::Drafting) } else { None });
                out.push(Item::Text(text, key, at.and_then(Value::as_i64), tps.and_then(Value::as_f64), rewrite, false));
            }
        }
        "reasoning" => {
            // Thinking streaming in that will be rewritten shows none of
            // itself until its rewrite does.
            let rewrite = rewrite.and_then(|_| unslop::thoughts(b, live)).or_else(|| {
                (draft && unslop::thinking() && text.is_some_and(|t| !t.trim().is_empty()))
                    .then(|| unslop::Thoughts { pending: true, ..Default::default() })
            });
            out.push(Item::Thinking(b, key, live && ms.is_none(), rewrite))
        }
        "tool_call" if name.is_some_and(|n| n == "DISPLAY") => out.push(Item::Display(b, key)),
        "tool_call" => out.push(Item::Tool(b, key)),
        _ => {}
    }
}

/// What the transcript reads of a user-turn block, in one pass over it.
#[derive(Clone, Copy)]
struct UserBlock<'a> {
    kind: Option<&'a str>,
    text: Option<&'a str>,
    /// Carries `_oak_file` (an attached text file).
    file: bool,
    /// Carries `_oak_goal` or `_oak_context` (`agent::is_hidden_block`).
    hidden: bool,
    at: Option<i64>,
    call_id: Option<&'a Value>,
    content: Option<&'a Value>,
    is_error: Option<&'a Value>,
    display: Option<&'a Value>,
}

impl<'a> UserBlock<'a> {
    fn of(b: &'a Value) -> Self {
        let [kind, text, file, goal, context, at, call_id, content, is_error, display] =
            crate::fields(b, ["type", "text", "_oak_file", "_oak_goal", "_oak_context", "_oak_at", "call_id", "content", "is_error", "_oak_display"]);
        UserBlock {
            kind: kind.and_then(Value::as_str),
            text: text.and_then(Value::as_str),
            file: file.is_some(),
            hidden: goal.is_some() || context.is_some(),
            at: at.and_then(Value::as_i64),
            call_id,
            content,
            is_error,
            display,
        }
    }

    /// `agent::is_hidden_block`.
    fn hidden(&self) -> bool {
        self.hidden || self.text.is_some_and(|t| t.starts_with(agent::OLD_GOAL_TAG))
    }

    /// `user_authored`.
    fn authored(&self) -> bool {
        match self.kind {
            Some("image" | "document") => true,
            Some("text") => {
                let text = self.text.unwrap_or("");
                self.file
                    || !(self.hidden() || text.starts_with(NOTE_TAG) || text.starts_with(agent::ALERT_TAG) || text.starts_with(BEAT_TAG) || text.starts_with(agent::AGENT_TAG) || text.starts_with(agent::ASK_TAG))
            }
            _ => false,
        }
    }
}

/// Whether a user-turn block is something the user wrote or attached (not
/// a tool result, a notification or a note from OAK).
pub(crate) fn user_authored(b: &Value) -> bool {
    UserBlock::of(b).authored()
}

/// The messages the user sent in one user turn, as ranges of its blocks:
/// a run of blocks they wrote or attached, ending with their text (the
/// attachments of a message come before its text).
pub(crate) fn user_messages(content: &[Value]) -> Vec<std::ops::Range<usize>> {
    let blocks: Vec<_> = content.iter().map(UserBlock::of).collect();
    user_messages_of(&blocks)
}

fn user_messages_of(blocks: &[UserBlock]) -> Vec<std::ops::Range<usize>> {
    let mut out = vec![];
    let mut start = None;
    for (i, b) in blocks.iter().enumerate() {
        if !b.authored() {
            if let Some(s) = start.take() {
                out.push(s..i);
            }
            continue;
        }
        let s = *start.get_or_insert(i);
        if b.kind == Some("text") && !b.file {
            out.push(s..i + 1);
            start = None;
        }
    }
    if let Some(s) = start {
        out.push(s..blocks.len());
    }
    out
}

/// The transcript as a flat list (tool results are shown with their calls,
/// so user turns that only carry results add nothing): the items, for each
/// the message it came from (or is shown before: a line OAK added; the
/// transcript's length for those after the last message, usize::MAX for
/// the reply streaming in), and every tool call's result by call id (found
/// in the same pass: this is built every frame).
pub(crate) fn items_at<'a>(conv: &'a Conversation, live: &'a Live) -> (Vec<Item<'a>>, Vec<usize>, Results<'a>) {
    let (mut out, mut msgs, results) = conv_items(conv);
    push_live(&mut out, &mut msgs, conv, live);
    mark_turn_ends(&mut out, 0, live.busy);
    (out, msgs, results)
}

/// `items_at` without the reply streaming in, and without the turns' ends
/// marked (`mark_turn_ends`).
pub(crate) fn conv_items<'a>(conv: &'a Conversation) -> (Vec<Item<'a>>, Vec<usize>, Results<'a>) {
    let mut out = vec![];
    let mut msgs = vec![];
    let mut results = Results::default();
    let cleared = conv.rolling.cleared_calls(&conv.messages);
    // A summary from before summaries got their line: shown at its cut.
    let legacy = conv
        .rolling
        .active()
        .filter(|s| !conv.marks.iter().any(|m| m.kind == MarkKind::Summarized && m.done == s.cut as i64))
        .map(|s| (s.cut, s.text.as_str(), s.made_at));
    // The marks by where they go (in the order they were added there), so
    // placing them takes one pass rather than a scan per message.
    let mut placed: Vec<&Mark> = conv.marks.iter().collect();
    placed.sort_by_key(|m| m.at);
    let mut next = 0;
    let mut marks = |out: &mut Vec<Item<'a>>, at: usize| {
        while let Some(m) = placed.get(next).filter(|m| m.at <= at) {
            next += 1;
            if m.at == at {
                out.push(match m.kind {
                    MarkKind::Summarized => Item::Summary { text: &m.text, cut: m.done.max(0) as usize, time: m.time },
                    _ => Item::Mark(m),
                });
            }
        }
    };
    // The user turns' blocks, read once (kept between turns, so built
    // without a new allocation each).
    let mut blocks: Vec<UserBlock> = vec![];
    for (mi, m) in conv.messages.iter().enumerate() {
        if let Some((at, text, time)) = legacy
            && at == mi
        {
            out.push(Item::Summary { text, cut: at, time });
        }
        marks(&mut out, mi);
        let [role, content] = crate::fields(m, ["role", "content"]);
        let content = content.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        if role.is_none_or(|r| r != "user") {
            for (bi, b) in content.iter().enumerate() {
                if b["type"] == "tool_result" {
                    results.insert(b["call_id"].as_str().unwrap_or(""), tool_out(b, &cleared));
                }
                push_assistant(&mut out, b, (mi, bi), false, false);
            }
            msgs.resize(out.len(), mi);
            continue;
        }
        blocks.clear();
        blocks.extend(content.iter().map(UserBlock::of));
        // The last block of each message the user sent, with where a
        // revert to before that message goes.
        // (A list, not a map: a turn holds a message or two, and most
        // hold none, only tool results.)
        let ends: Vec<(usize, RevertAt)> = user_messages_of(&blocks).into_iter().map(|r| (r.end - 1, RevertAt { msg: mi, block: r.start, first: &content[r.start] })).collect();
        for (bi, (b, f)) in content.iter().zip(&blocks).enumerate() {
            if f.kind == Some("tool_result") {
                results.insert(f.call_id.and_then(Value::as_str).unwrap_or(""), tool_out_of(f, &cleared));
            }
            let revert = ends.iter().find(|e| e.0 == bi).map(|e| e.1);
            match f.kind {
                Some("image") => out.push(Item::Image(b, revert)),
                Some("document") => out.push(Item::File(b, revert)),
                Some("text") if f.file => out.push(Item::File(b, revert)),
                Some("text") => {
                    let text = f.text.unwrap_or("");
                    if f.hidden() {
                        continue; // shown as its mark
                    }
                    out.push(if text.starts_with(NOTE_TAG) || text.starts_with(agent::ALERT_TAG) {
                        Item::Note(text)
                    } else if text.starts_with(BEAT_TAG) {
                        Item::Beat(text)
                    } else if text.starts_with(agent::ASK_TAG) {
                        Item::AskReply(text)
                    } else if let Some((from, last, body)) = agent::parse_agent_message(text) {
                        Item::AgentMessage { from, last, body, key: (mi, bi) }
                    } else {
                        Item::UserText(text, revert, f.at, false)
                    });
                }
                _ => {}
            }
        }
        msgs.resize(out.len(), mi);
    }
    marks(&mut out, conv.messages.len());
    msgs.resize(out.len(), conv.messages.len());
    (out, msgs, results)
}

/// The reply streaming in (`live`), added to the items and messages
/// `conv_items` gave.
pub(crate) fn push_live<'a>(out: &mut Vec<Item<'a>>, msgs: &mut Vec<usize>, conv: &Conversation, live: &'a Live) {
    let draft = unslop::applies(conv);
    for (bi, b) in live.blocks.iter().enumerate() {
        push_assistant(out, b, (usize::MAX, bi), true, draft);
    }
    msgs.resize(out.len(), usize::MAX);
}

/// Whether an item shows what can change while the conversation doesn't: a
/// rewrite (Unslop), which comes in by itself, or a call with no result
/// yet, whose running command's output grows (`unit_sig`).
pub(crate) fn volatile(x: &Item, results: &Results) -> bool {
    match x {
        Item::Text(.., Some(_), _) | Item::Thinking(.., Some(_)) => true,
        Item::Tool(b, _) | Item::Display(b, _) => !results.contains_key(b["id"].as_str().unwrap_or("")),
        _ => false,
    }
}

/// A conversation's item with its rewrite as it stands now (`volatile`).
pub(crate) fn refresh<'a>(x: &mut Item<'a>, conv: &'a Conversation) {
    match x {
        Item::Text(_, (mi, bi), .., rewrite, _) => {
            if let Some(b) = conv.messages.get(*mi).and_then(|m| m["content"].get(*bi)) {
                *rewrite = b.get(unslop::KEY).map(unslop::state_of);
            }
        }
        Item::Thinking(b, _, _, rewrite) => *rewrite = unslop::thoughts(b, false),
        _ => {}
    }
}

/// Whether an item is the model's (its text, thinking or tool calls).
fn models(x: &Item) -> bool {
    matches!(x, Item::Text(..) | Item::Thinking(..) | Item::Tool(..) | Item::Display(..))
}
/// Whether an item is something the user sent.
fn users(x: &Item) -> bool {
    matches!(x, Item::UserText(..) | Item::Image(..) | Item::File(..))
}

/// Marks the text that ends a turn: the model's last text before anything
/// that isn't the model's (not while it is still at it), and the user's
/// last message before anything they didn't send. Only those get a bar
/// (copy, time), the way a chat shows it once per reply.
/// Only the items from `from` on are marked: those before keep their marks.
pub(crate) fn mark_turn_ends(items: &mut [Item], from: usize, busy: bool) {
    for i in from..items.len() {
        let next = items[i + 1..].iter().find(|x| !matches!(x, Item::Mark(_)));
        let x = &items[i];
        let end = match (x, next) {
            (Item::Text(..), None) => !busy,
            (Item::Text(..), Some(x)) => !models(x),
            (Item::UserText(..), x) => !x.is_some_and(users),
            _ => continue,
        };
        if let Item::Text(.., e) | Item::UserText(.., e) = &mut items[i] {
            *e = end;
        }
    }
}

/// What a turn's copy button copies, for its last text at `end`: all its
/// text (the model's, between its tool calls; or the user's messages).
fn turn_text(items: &[Item], end: usize) -> String {
    let model = models(&items[end]);
    let mut parts = vec![];
    for x in items[..=end].iter().rev() {
        match x {
            Item::Mark(_) => {}
            x if model && !models(x) => break,
            x if !model && !users(x) => break,
            Item::Text(s, .., Some(r), _) => parts.push(shown_text(s, r).trim()),
            Item::Text(s, ..) | Item::UserText(s, ..) => parts.push(s.trim()),
            _ => {}
        }
    }
    parts.reverse();
    parts.join("\n\n")
}

/// An angular check mark, drawn in a `size` square at `at` (its top left).
fn paint_check(p: &egui::Painter, at: egui::Pos2, size: f32, color: Color32) {
    let pt = |x: f32, y: f32| at + vec2(x * size, y * size);
    p.add(egui::Shape::line(vec![pt(0.06, 0.55), pt(0.38, 0.86), pt(0.96, 0.14)], Stroke::new(size * 0.17, color)));
}

/// A transcript line for the user only: what a command changed.
pub(crate) fn mark_row(ui: &mut Ui, t: &Theme, m: &Mark) {
    if m.kind == MarkKind::Limit {
        return limit_box(ui, t, m);
    }
    if m.kind == MarkKind::GoalFinished {
        return goal_finished_row(ui, t, m);
    }
    if m.kind == MarkKind::Failed {
        return crate::widgets::error_note(ui, t, &m.text);
    }
    // The line's text and icon (None: the check mark). GoalFinished, Failed
    // and Limit are unreachable here (mark_row takes them above); the match
    // must be exhaustive.
    let (text, icon): (String, Option<&'static str>) = match m.kind {
        MarkKind::GoalContinued => ("goal not finished yet: asked to continue".to_string(), Some(GOAL_ICON)),
        MarkKind::GoalFinished => ("goal finished".to_string(), Some(GOAL_ICON)),
        MarkKind::Goal if m.text.is_empty() => ("goal cleared".to_string(), Some(GOAL_ICON)),
        MarkKind::Goal => (format!("goal set to {}", crate::tools::one_line(&m.text)), Some(GOAL_ICON)),
        MarkKind::Effort => (format!("effort set to {}", m.text), Some(EFFORT_ICON)),
        MarkKind::Model => (format!("model set to {}", api::model_name(&m.text)), Some(MODEL_ICON)),
        MarkKind::Rotated => (format!("account rotated to {}", m.text), Some(ROTATED_ICON)),
        MarkKind::FilesChanged => (format!("changed on disk, not by Oak: {}", m.text), Some(FILES_CHANGED_ICON)),
        // Material Design's timer-sand paused / complete.
        MarkKind::FreebuffHour => (m.text.clone(), Some("\u{f051b}")),
        MarkKind::FreebuffGrace => ("Freebuff grace limit was reached.".to_string(), Some("\u{f0516}")),
        MarkKind::Heartbeat | MarkKind::Limit | MarkKind::Summarized | MarkKind::Failed => (m.text.clone(), None),
        MarkKind::Instructions => (m.text.clone(), Some(INSTRUCTIONS_ICON)),
        MarkKind::Identity => (m.text.clone(), Some(IDENTITY_ICON)),
        MarkKind::Tools => (m.text.clone(), Some(TOOLS_ICON)),
    };
    let hover = (matches!(m.kind, MarkKind::Goal | MarkKind::Instructions | MarkKind::FilesChanged) && !m.text.is_empty()).then_some(m.text.as_str());
    let id = ui.scope_id().with(("mark-open", m.at, m.time, m.kind as u8));
    notice_row(ui, t, id, &text, hover, &m.sent, |p, rect| {
        if let Some(icon) = icon {
            // Sized so the glyph's ink is about the checkmark's (11 px),
            // level with the text.
            glyph(p, rect, icon, t);
        } else {
            paint_check(p, egui::pos2(rect.left() + 2.0, rect.center().y - 6.0), 12.0, t.accent);
        }
    });
}

/// A Nerd Font glyph, centered in `rect`, in the transcript's icon size.
fn glyph(p: &egui::Painter, rect: Rect, g: &str, t: &Theme) {
    p.text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        Align2::CENTER_CENTER,
        g,
        markdown::icon_font(MARK_ICON_SIZE),
        t.accent,
    );
}

/// Where a text the conversation holds sits in memory: with its length, it
/// tells the transcript's lines apart (and keeps their state) frame to frame.
fn addr(s: &str) -> usize {
    s.as_ptr() as usize
}

/// A transcript line OAK added (`raw`): its tag stripped and its closing
/// tag trimmed, shown with `glyph`. `first`'s tag goes first; when it is
/// not there, `second`'s (a task notification, not an alert).
fn notice(ui: &mut Ui, t: &Theme, raw: &str, key: &str, g: &str, first: (&str, &str), second: Option<(&str, &str)>) {
    let (tag, end) = if raw.starts_with(first.0) { first } else { second.expect("a tagged line") };
    let body = raw.strip_prefix(tag).unwrap_or(raw).trim_end_matches(end).trim();
    let id = Id::unique((key, addr(raw), raw.len()));
    notice_row(ui, t, id, body, None, raw, |p, r| glyph(p, r, g, t));
}

/// A transcript line that opens on click to a body below it: an icon in
/// the row's left, `label` (cut to `room(rect)`), and, when `active`, a
/// chevron, the pointing hand and the tooltip (`tip`, given the open
/// state) after it. `clickable` makes the row a click target; `active`
/// makes it expand (a click toggles it, and the open state starts at
/// `default_open`). `open_body` paints the body in the 22-of-left frame.
#[allow(clippy::too_many_arguments)]
fn collapse_row(
    ui: &mut Ui,
    t: &Theme,
    id: Id,
    label: &str,
    room: impl FnOnce(Rect) -> f32,
    clickable: bool,
    active: bool,
    default_open: bool,
    icon: impl FnOnce(&egui::Painter, Rect),
    tip: impl Fn(bool, bool) -> Option<String>,
    open_body: impl FnOnce(&mut Ui, &Theme),
) {
    let open = active && ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(default_open);
    ui.add_space(4.0);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), if clickable { Sense::click() } else { Sense::hover() });
    let lit = clickable && resp.hovered();
    icon(ui.painter(), rect);
    let color = if lit { t.ink } else { t.ink2 };
    let font = egui::TextStyle::Body.resolve(ui.style());
    let shown = clip_width(ui, label, &font, room(rect));
    let cut = shown != label || label.contains('\n');
    let g = ui.painter().text(egui::pos2(rect.left() + 22.0, mid_y(ui, rect)), Align2::LEFT_CENTER, shown, font, color);
    if active {
        chevron(ui.painter(), egui::pos2(g.right() + 12.0, mid_y(ui, rect)), open, color);
    }
    if lit && active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if let Some(tip) = tip(open, cut) {
        resp = resp.on_hover_text(tip);
    }
    if active && resp.clicked() {
        ui.data_mut(|d| d.insert_temp(id, !open));
    }
    if open {
        egui::Frame::new().inner_margin(Margin { left: 22, right: 0, top: 2, bottom: 2 }).show(ui, |ui| open_body(ui, t));
    }
    ui.add_space(4.0);
}

/// A transcript line from OAK: an icon (`icon` paints it in the row's
/// rect) and `label`. When the model was told something with it
/// (`verbatim`), a click opens what it was sent, word for word.
fn notice_row(ui: &mut Ui, t: &Theme, id: Id, label: &str, hover: Option<&str>, verbatim: &str, icon: impl FnOnce(&egui::Painter, Rect)) {
    let opens = !verbatim.trim().is_empty();
    collapse_row(ui, t, id, label, |r| r.width() - 22.0 - if opens { 24.0 } else { 0.0 }, opens, opens, false, icon, |open, cut| {
        match (hover, opens) {
            (Some(h), true) => Some(format!("{h}\n\nClick for the model view")),
            (Some(h), false) => Some(h.to_string()),
            (None, true) if cut => Some(format!("{label}\n\nClick for the model view")),
            (None, true) => Some("Click for the model view".to_string()),
            (None, false) if cut => Some(label.to_string()),
            (None, false) => None,
        }
        .filter(|_| !open)
    }, |ui, t| {
        caption(ui, "model view", t.ink3);
        ui.add_space(2.0);
        for (title, body) in store::split_sent(verbatim) {
            if !title.is_empty() {
                ui.add_space(4.0);
                caption(ui, title, t.ink2);
                ui.add_space(2.0);
            }
            verbatim_block(ui, t, body);
        }
    });
}

/// Text as the model got it: in the code font, unformatted, selectable.
fn verbatim_block(ui: &mut Ui, t: &Theme, text: &str) {
    let at = text.char_indices().nth(20_000).map(|(i, _)| i).unwrap_or(text.len());
    let (shown, more) = (&text[..at], at < text.len());
    crate::widgets::inset(ui, t, t.bg, 10, |ui| {
        ui.add(egui::Label::new(RichText::new(shown.trim_end()).font(markdown::code_font(12.0)).color(t.ink2)).wrap().selectable(true));
        if more {
            caption(ui, "… (truncated)", t.ink3);
        }
    });
}

/// Where the summary banner asks the transcript to go: the message the
/// summary stands in for everything before.
pub(crate) const JUMP_REQUEST: &str = "oak-transcript-jump";
/// The open summary banner: when its summary was made, and its cut (the
/// minimap marks the cut).
pub(crate) const SUMMARY_OPEN: &str = "oak-summary-open";

/// Smart context summarized the conversation here: a banner across the
/// transcript. Clicked, it opens to the summary (what the model sees in
/// place of the messages before `cut`), with a link to that point, which
/// the minimap marks meanwhile.
fn summary_banner(ui: &mut Ui, t: &Theme, text: &str, cut: usize, time: i64) {
    let open = ui.data(|d| d.get_temp::<(i64, usize)>(ui.scope_id().with(SUMMARY_OPEN))).is_some_and(|o| o.0 == time);
    ui.add_space(8.0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
    let lit = resp.hovered() || open;
    let color = if lit { t.accent } else { t.ink3 };
    let font = FontId::proportional(12.5);
    let label = "Context summarized";
    let p = ui.painter();
    let g = p.layout_no_wrap(label.into(), font, color);
    // The cube, the same mark the session box uses for a compacted session.
    let cube = p.layout_no_wrap(
        "\u{f1b2}".into(),
        markdown::icon_font(MARK_ICON_SIZE),
        color,
    );
    let total = cube.size().x + 4.0 + g.size().x;
    let (c, half) = (rect.center(), total / 2.0 + 22.0);
    let y = c.y.round() + 0.5;
    let line = Stroke::new(1.0, if lit { t.accent.gamma_multiply(0.7) } else { t.line });
    p.hline(rect.left()..=c.x - half, y, line);
    p.hline(c.x + half..=rect.right(), y, line);
    let left = c.x - total / 2.0;
    let (cw, ch) = (cube.size().x, cube.size().y);
    p.galley(egui::pos2(left, c.y - ch / 2.0), cube, color);
    p.galley(egui::pos2(left + cw + 4.0, c.y - g.size().y / 2.0), g, color);
    chevron(p, egui::pos2(c.x + half - 12.0, c.y), open, color);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let resp = if open { resp } else { resp.on_hover_text("Context summary") };
    if resp.clicked() {
        ui.data_mut(|d| {
            if open {
                d.remove_temp::<(i64, usize)>(ui.scope_id().with(SUMMARY_OPEN));
            } else {
                d.insert_temp(ui.scope_id().with(SUMMARY_OPEN), (time, cut));
            }
        });
    }
    if open {
        ui.add_space(2.0);
        egui::Frame::new().stroke(Stroke::new(1.0, t.accent.gamma_multiply(0.5))).inner_margin(Margin::same(12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let link = ui
                .add(egui::Label::new(RichText::new("↑ Jump to summary boundary").small().color(t.accent)).sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Earlier messages are replaced by this summary in model context; later messages remain verbatim.");
            if link.clicked() {
                ui.data_mut(|d| d.insert_temp(Id::unique(JUMP_REQUEST), cut));
            }
            ui.add_space(6.0);
            markdown::show(ui, t, text, ui.scope_id().with(("rolling-summary-text", time)));
        });
    }
    ui.add_space(8.0);
}

/// A message from another session: a line naming the sender (and whether
/// it is a subagent's final reply) that opens on click to the message.
/// Short ones start open.
fn agent_message_row(ui: &mut Ui, t: &Theme, from: &str, last: bool, body: &str, key: (usize, usize)) {
    let id = key_id("agent-message", key);
    let short = body.lines().count() <= AGENT_MESSAGE_OPEN_LINES && body.len() <= 800;
    let head = if last { format!("final reply from {from}") } else { format!("message from {from}") };
    collapse_row(ui, t, id, &head, |r| r.right() - r.left() - 60.0, true, true, short, |p, rect| glyph(p, rect, AGENT_ICON, t), |open, _| {
        Some(if open { "Hide the message" } else { "Show the message" }.to_string())
    }, |ui, t| {
        crate::widgets::inset(ui, t, Color32::TRANSPARENT, 10, |ui| {
            crate::select::message(ui, id.with("text"), || markdown::plain_text(body), |ui| markdown::show(ui, t, body, id.with("text")));
        });
    });
}

/// Agent messages up to this many lines start open in the transcript.
const AGENT_MESSAGE_OPEN_LINES: usize = 8;

/// The goal was finished: a line that opens on click to the model's
/// justification.
fn goal_finished_row(ui: &mut Ui, t: &Theme, m: &Mark) {
    let id = ui.scope_id().with(("goal-finished", m.at, m.time));
    let active = !m.text.is_empty();
    collapse_row(ui, t, id, "goal finished", |_| f32::INFINITY, true, active, false, |p, rect| {
        paint_check(p, egui::pos2(rect.left() + 2.0, rect.center().y - 6.0), 12.0, t.accent);
    }, |open, _| active.then(|| (if open { "Hide completion report" } else { "Show completion report" }).to_string()), |ui, t| {
        markdown::show(ui, t, &m.text, id.with("text"));
    });
}

/// The Nerd Font icon of an account rotation line.
pub(crate) const ROTATED_ICON: &str = "\u{f0d53}";
/// The font size of a transcript line's icon.
const MARK_ICON_SIZE: f32 = 11.0;
/// Instructions (the system prompt, CLAUDE.md, AGENTS.md) loaded.
const INSTRUCTIONS_ICON: &str = "\u{f0477}";
/// The effort changed (nf-fa-dove).
const EFFORT_ICON: &str = "\u{ed99}";
/// The model changed (nf-md-swap_horizontal).
const MODEL_ICON: &str = "\u{f04e1}";
/// Files changed on disk, not by OAK (nf-md-file_edit).
pub(crate) const FILES_CHANGED_ICON: &str = "\u{f11e7}";
/// The Claude Code identity and billing header (nf-md-card_account_details_outline).
const IDENTITY_ICON: &str = "\u{f0dab}";
/// The tool definitions (nf-md-tools).
const TOOLS_ICON: &str = "\u{f1064}";
/// A heartbeat came due (nf-fa-heartbeat).
const HEARTBEAT_ICON: &str = "\u{f21e}";
/// A task notification: a background task finished (nf-md-rice).
const TASK_NOTE_ICON: &str = "\u{f07ea}";
/// The goal set, cleared or continued.
const GOAL_ICON: &str = "\u{f4de}";
/// A message from another session (nf-md-robot).
const AGENT_ICON: &str = "\u{f06a9}";

/// A usage limit stopped the conversation: when it resets, and whether OAK
/// carried on (it does so on its own; the model isn't told). With several
/// accounts signed in (and rotation on), it only stops once all of them are used up.
fn limit_box(ui: &mut Ui, t: &Theme, m: &Mark) {
    let waiting = m.text.is_empty();
    let all = api::rotation_on() && api::accounts().len() > 1;
    let detail = match (m.text.as_str(), all) {
        ("", false) => format!("Resumes at reset: {} (in {}).", clock(m.time), until(m.time)),
        ("", true) => format!("Resumes at first account reset: {} (in {}).", clock(m.time), until(m.time)),
        ("continued", false) => format!("Resumed after reset at {}.", clock(m.done)),
        ("continued", true) => format!("Resumed after account reset at {}.", clock(m.done)),
        (_, false) => format!("Oak stopped waiting at {}. The limit resets at {}.", clock(m.done), clock(m.time)),
        (_, true) => format!("Oak stopped waiting at {}. The first account resets at {}.", clock(m.done), clock(m.time)),
    };
    let title = if all { "All accounts at usage limit" } else { "Usage limit reached" };
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(t.panel)
        .stroke(Stroke::new(1.0, if waiting { t.accent } else { t.line }))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                paint_clock(ui.painter(), r, if waiting { t.accent } else { t.ink3 });
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(title).strong().color(if waiting { t.ink } else { t.ink2 }));
                    caption(ui, &detail, t.ink3);
                });
            });
        });
    ui.add_space(6.0);
}

/// A clock face in `r`: a ring and two hands.
fn paint_clock(p: &egui::Painter, r: Rect, color: Color32) {
    let (c, rad) = (r.center(), r.width() * 0.45);
    let s = Stroke::new(1.5, color);
    p.circle_stroke(c, rad, s);
    p.line_segment([c, c + vec2(0.0, -rad * 0.62)], s);
    p.line_segment([c, c + vec2(rad * 0.48, rad * 0.2)], s);
}

/// A tool result block as the transcript shows it; `cleared`: the calls
/// the model sees cleared. Borrowed from the conversation (built every
/// frame) rather than copying each output.
fn tool_out<'a>(b: &'a Value, cleared: &rolling::Ids) -> ToolOut<'a> {
    tool_out_of(&UserBlock::of(b), cleared)
}

fn tool_out_of<'a>(b: &UserBlock<'a>, cleared: &rolling::Ids) -> ToolOut<'a> {
    let error = b.is_error.is_some_and(|e| e == true);
    let display = b.display.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let cleared = !cleared.is_empty() && b.call_id.and_then(Value::as_str).is_some_and(|id| cleared.contains(id));
    match b.content.unwrap_or(&Value::Null) {
        Value::Array(blocks) => ToolOut {
            text: match blocks.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>()[..] {
                [] => "".into(),
                [one] => one.into(),
                ref many => many.join("\n").into(),
            },
            images: blocks.iter().filter(|x| x["type"] == "image").collect(),
            error,
            display,
            cleared,
        },
        c => ToolOut { text: c.as_str().unwrap_or("").into(), images: vec![], error, display, cleared },
    }
}

/// The tool results among `blocks` (those of calls that ran while their
/// reply still streams in, none of them cleared), added to `results`.
pub(crate) fn add_results<'a>(results: &mut Results<'a>, blocks: &'a [Value]) {
    let none = rolling::Ids::default();
    for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
        results.insert(b["call_id"].as_str().unwrap_or(""), tool_out(b, &none));
    }
}

// Tool and thinking rows: one height, and fixed columns for the chevron,
// the status dot and the name, so every row lines up with the others.
const CHEVRON_X: f32 = 14.0;
const DOT_X: f32 = 30.0;
const NAME_X: f32 = 42.0;
/// The widest the name column gets, in characters.
const NAME_COLUMN: usize = 11;

/// The middle of a row, on the pixel grid: texts of different heights
/// centered on it (a tool's name and its summary in the code font) then
/// keep their places as the row moves by fractions of a pixel (as rows
/// above open and close), rather than each rounding its own way.
pub(crate) fn mid_y(ui: &Ui, rect: Rect) -> f32 {
    use egui::emath::GuiRounding;
    rect.center().y.round_to_pixels(ui.pixels_per_point())
}

/// A row that opens and closes on click; returns whether it is open.
pub(crate) fn row(ui: &mut Ui, t: &Theme, id: Id, clickable: bool, in_block: bool) -> (Rect, bool, bool) {
    let mut open = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let sense = if clickable { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), sense);
    let hovered = clickable && resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 0.0, if in_block { t.tool_hover } else { t.tool_bg });
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if clickable && resp.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    (rect, open, hovered)
}

/// What opens under a row, lined up with the names.
fn row_details(ui: &mut Ui, indent: f32, f: impl FnOnce(&mut Ui)) {
    ui.horizontal_top(|ui| {
        ui.add_space(indent);
        ui.vertical(|ui| {
            ui.set_width(ui.available_width() - 10.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            f(ui);
        });
    });
    ui.add_space(6.0);
}

/// A tool block's name as the chat shows it: the name the call came with
/// from another agent (Freebuff's `read_files`), else OAK's own for it.
pub(crate) fn shown(b: &Value) -> String {
    match b["_oak_shown"].as_str() {
        Some(n) => n.to_string(),
        None => tools::shown_name(&tools::canonical_cow(b["name"].as_str().unwrap_or(""), &b["input"]).0),
    }
}

/// Each tool's color, for its dot, name and summary. One lightness and
/// one soft saturation for all (OKLCH L 0.78, chroma 0.05 to 0.09), so
/// no tool shouts; only the hue differs, by kind of tool: files in blue,
/// green and teal, commands and tasks in violets, the web in cyans, the
/// model's own memory in sage and rose. Oranges and yellows are left to
/// the accent, warnings and errors.
fn tool_color(name: &str) -> Color32 {
    hex(match name {
        // Cloud sessions' tools (Claude Code's names) take the colors of
        // OAK's that do the same.
        "READ" | "Read" => 0x8bbcef,
        "WRITE" | "Write" => 0x90c897,
        "EDIT" | "Edit" | "MultiEdit" | "NotebookEdit" => 0x7ac9bb,
        "EXECUTE" | "BASH" | "Bash" => 0xb8aded,
        "BashOutput" | "KillShell" | "KillBash" => 0xcea7db,
        "Grep" | "Glob" => 0xd8b38a,
        "WebSearch" => 0x77c4de,
        "WebFetch" => 0x76c7ce,
        "AskUserQuestion" => 0xe5a1b7,
        "TodoWrite" => 0xb8bc88,
        "Agent" | "Task" => 0xa9b2f1,
        "TASK_NEW" | "TASK_GET_NEW_OUTPUT" | "TASK_WAIT" | "TASK_LIST" => 0xcea7db,
        "TASK_DELETE" => 0xdba4c8,
        "SEARCH" => 0x77c4de,
        "FETCH" => 0x76c7ce,
        "SCREENSHOT" => 0xa3b6e8,
        n if n.starts_with("BROWSER") => 0x8fc3d9,
        "ASK" | "REQUEST_ASK" => 0xe5a1b7,
        "DISPLAY" => 0x99bdd3,
        n if n.starts_with("NOTE_") => 0xb8bc88,
        n if n.starts_with("HEARTBEAT_") => 0xe6a3a7,
        n if n.starts_with("PROGRESS_") => 0x9dc8c0,
        n if n.starts_with("GOAL_") => 0xa3c38b,
        n if n.starts_with("AGENT_") => 0xa9b2f1,
        _ => 0xb0b8c1,
    })
}

/// The characters of a tool's output its row shows (the model gets it all).
pub(crate) const OUTPUT_SHOWN: usize = 6000;

/// A tool's output as its row shows it: up to `OUTPUT_SHOWN` characters,
/// ending with a whole line when there is one, and how many lines are left.
pub(crate) fn cut_output(text: &str) -> (String, usize) {
    let Some((i, _)) = text.char_indices().nth(OUTPUT_SHOWN) else {
        return (text.to_string(), 0);
    };
    let end = text[..i].rfind('\n').filter(|&n| n > 0).unwrap_or(i);
    let rest = &text[end..];
    (text[..end].to_string(), rest.strip_prefix('\n').unwrap_or(rest).lines().count())
}

/// Whether a call of `name` runs a shell command.
fn is_command(name: &str) -> bool {
    matches!(name, "EXECUTE" | "TASK_NEW" | "BASH" | "Bash")
}

fn tool_row(ui: &mut Ui, t: &Theme, b: &Value, results: &Results, name_w: f32) {
    let id = b["id"].as_str().unwrap_or("");
    // A call still streaming in: its input so far.
    let streaming = b["_oak_partial"].as_str();
    let partial = streaming.map(|raw| Value::Object(partial_input(raw)));
    let input = partial.as_ref().unwrap_or(&b["input"]);
    // A family's call (`TASK` with an action) is shown as its action's tool.
    let (name, input) = tools::canonical_cow(b["name"].as_str().unwrap_or(""), input);
    let (name, input) = (name.as_ref(), input.as_ref());
    let shown_name = shown(b);
    let result = results.get(id);
    let (rect, open, hovered) = row(ui, t, ui.scope_id().with(("tool-open", id)), true, true);
    let y = mid_y(ui, rect);
    let p = ui.painter();
    chevron(p, egui::pos2(rect.left() + CHEVRON_X, y), open, if hovered { t.ink } else { t.ink3 });
    let dot = egui::pos2(rect.left() + DOT_X, y);
    // The tool's color: full on the dot, less on the name, less again on
    // the summary.
    let swatch = tool_color(name);
    match result {
        None => p.circle_stroke(dot, 3.5, Stroke::new(1.2, swatch)),
        Some(r) if r.error => p.circle_filled(dot, 3.5, t.error),
        Some(_) => p.circle_filled(dot, 3.5, swatch),
    };
    let (name_color, summary_color) = (mix(t.ink2, swatch, 0.75), mix(t.ink2, swatch, 0.40));
    let font = FontId::proportional(13.0);
    let shown = p.text(egui::pos2(rect.left() + NAME_X, y), Align2::LEFT_CENTER, &shown_name, font.clone(), name_color);
    let x = rect.left() + NAME_X + name_w.max(shown.width()) + 18.0;
    // A command shown as it is (no summary) is code: the code font.
    let raw_command = is_command(name) && input[if name == "Bash" { "description" } else { "summary" }].as_str().is_none_or(|s| s.trim().is_empty());
    let font = if raw_command { markdown::code_font(12.5) } else { font };
    // After the summary: an EDIT's lines added and removed and READ's lines
    // (the summary is cut short first when there is too little room).
    let lines = line_counts(ui, t, name, input, result);
    // At the right end: EXECUTE's timeout.
    let timeout = (name == "EXECUTE")
        .then(|| input["timeout"].as_u64())
        .flatten()
        .map(|s| ui.painter().layout_no_wrap(format!("timeout {s}s"), FontId::proportional(12.0), t.ink3));
    let end = rect.right() - 10.0 - timeout.as_ref().map_or(0.0, |g| g.size().x + 12.0);
    let room = end - x - lines.as_ref().map_or(0.0, |g| g.size().x + 8.0);
    let summary = clip_width(ui, &tool_summary(name, input), &font, room.max(40.0));
    let drawn = ui.painter().text(egui::pos2(x, y), Align2::LEFT_CENTER, summary, font, summary_color);
    if let Some(g) = lines
        && drawn.right() + 8.0 + g.size().x <= end
    {
        ui.painter().galley(egui::pos2(drawn.right() + 8.0, y - g.size().y / 2.0), g, t.ink3);
    }
    if let Some(g) = timeout
        && drawn.right() + 12.0 <= rect.right() - 10.0 - g.size().x
    {
        ui.painter().galley(egui::pos2(rect.right() - 10.0 - g.size().x, y - g.size().y / 2.0), g, t.ink3);
    }
    if !open {
        // A WRITE or EDIT coming in shows its progress under its row (a
        // running command's output shows only in its open row).
        if streaming.is_some() {
            write_progress(ui, t, name, input);
        }
        return;
    }
    row_details(ui, NAME_X, |ui| {
        tool_input_view(ui, t, name, input);
        match result {
            Some(r) => tool_output_view(ui, t, name, input, r),
            None if streaming.is_some() => {
                ui.label(RichText::new("writing…").color(t.ink3));
            }
            None if name == "EXECUTE" && crate::tasks::streaming_len(id).is_some() => {
                command_output(ui, t, id);
            }
            None => {
                ui.label(RichText::new("running…").color(t.ink3));
            }
        }
    });
}

/// An EDIT's lines added and removed (in green and red) and READ's lines,
/// laid out for after a tool row's summary; None for other calls.
fn line_counts(ui: &Ui, t: &Theme, name: &str, input: &Value, result: Option<&ToolOut>) -> Option<Arc<egui::Galley>> {
    let mut parts: Vec<(String, Color32)> = vec![];
    if let Some((added, removed)) = edit_stats(name, input, result) {
        parts.push((format!("+{added}"), mix(t.ink2, hex(DIFF_GREEN), 0.85)));
        parts.push((format!("−{removed}"), mix(t.ink2, hex(DIFF_RED), 0.85)));
    }
    if let Some(l) = line_span(name, input, result) {
        parts.push((l, t.ink3));
    }
    if parts.is_empty() {
        return None;
    }
    let mut job = egui::text::LayoutJob::default();
    for (i, (s, color)) in parts.into_iter().enumerate() {
        job.append(&s, if i > 0 { 5.0 } else { 0.0 }, egui::TextFormat { font_id: FontId::proportional(12.0), color, ..Default::default() });
    }
    Some(ui.painter().layout_job(job))
}

/// An open tool row's input: a WRITE's new file, an EDIT's change, a
/// command (as shell code), SEARCH's queries, an agent's prompt or message,
/// ASK's questions.
fn tool_input_view(ui: &mut Ui, t: &Theme, name: &str, input: &Value) {
    let path = input["path"].as_str().unwrap_or("");
    let cut = |k: &str| -> String { input[k].as_str().unwrap_or("").head(6000) };
    match name {
        "WRITE" => {
            // The new file as it is (no diff), colored as code when it is.
            let content: String = input["content"].as_str().unwrap_or("").head(4000);
            code_view(ui, t, highlight::for_file(path, &content), &[(None, content.trim_end())], None);
        }
        "EDIT" => {
            let (old, new) = (cut("old_string"), cut("new_string"));
            let parts: Vec<highlight::Part> = [(Some(false), old.as_str()), (Some(true), new.as_str())].into_iter().filter(|(_, s)| !s.is_empty()).collect();
            code_view(ui, t, highlight::for_path(path), &parts, None);
        }
        _ => {}
    }
    let command = is_command(name);
    let detail = match name {
        _ if command => format!("$ {}", input["command"].as_str().unwrap_or("")),
        "SEARCH" => input["queries"].as_array().into_iter().flatten().filter_map(|q| q.as_str()).collect::<Vec<_>>().join("\n"),
        "AGENT_NEW" => cut("prompt"),
        "AGENT_SEND" => cut("message"),
        "ASK" => input["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|q| {
                let opts: Vec<&str> = q["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str()).collect();
                format!("{}\n  {}", q["question"].as_str().unwrap_or(""), opts.join(", "))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if command && !detail.is_empty() {
        // The command is shell code.
        code_view(ui, t, Some(highlight::shell()), &[(None, &detail)], Some(input["command"].as_str().unwrap_or("")));
    } else if !detail.is_empty() {
        code_block(ui, t, &detail, t.ink2);
    }
}

/// An open tool row's result `r`: SEARCH's hits, READ's lines, a command's
/// output colored when it is code, or the text; then how much was left out
/// and the images.
fn tool_output_view(ui: &mut Ui, t: &Theme, name: &str, input: &Value, r: &ToolOut) {
    // Cleared from the model's context: what it sees in its place, then
    // the output itself.
    if r.cleared {
        caption(ui, "model view", t.ink3);
        verbatim_block(ui, t, rolling::CLEARED);
        caption(ui, "actual output", t.ink3);
    }
    let (out, more) = cut_output(&r.text);
    let hits = (!r.error && name == "SEARCH").then(|| crate::tools::search_results(&r.text)).flatten();
    if let Some(hits) = hits {
        search_view(ui, t, &hits);
    } else if !r.error && name == "READ" && read_view(ui, t, input["path"].as_str().unwrap_or(""), &out) {
        // Shown with its line numbers, colored as code when it is.
    } else if !r.error && is_command(name) && let Some(syntax) = output_syntax(input["command"].as_str().unwrap_or(""), &out) {
        code_view(ui, t, Some(syntax), &[(None, out.trim_end())], None);
    } else {
        code_block(ui, t, &out, if r.error { t.error } else { t.ink });
    }
    if more > 0 {
        caption(ui, &format!("{more} more line{}", crate::plural(more)), t.ink3);
    }
    for img in &r.images {
        images::show(ui, img, 240.0);
    }
}

/// An EDIT's lines added and removed, for its row: old_string's and
/// new_string's lines past the ones they start and end with alike, times
/// the replacements its result counts. None for a failed edit.
pub(crate) fn edit_stats(name: &str, input: &Value, result: Option<&ToolOut>) -> Option<(usize, usize)> {
    if !matches!(name, "EDIT" | "Edit") || result.is_some_and(|r| r.error) {
        return None;
    }
    let lines = |k: &str| input[k].as_str().map(|s| s.lines().collect::<Vec<_>>());
    let (old, new) = (lines("old_string")?, lines("new_string").unwrap_or_default());
    let same_start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - same_start;
    let same_end = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    // "(3 replacements; …)" with replace_all.
    let times = result
        .and_then(|r| r.text.split_once(" replacements;"))
        .and_then(|(before, _)| before.rsplit('(').next()?.parse::<usize>().ok())
        .unwrap_or(1);
    Some(((new.len() - same_start - same_end) * times, (old.len() - same_start - same_end) * times))
}

/// The lines a READ covers, for its row: from its result (the numbered
/// lines it returned), or before that the offset and limit asked for.
pub(crate) fn line_span(name: &str, input: &Value, result: Option<&ToolOut>) -> Option<String> {
    let ok = result.filter(|r| !r.error);
    match name {
        "READ" => {
            let number = |l: &str| l.split_once('\t').and_then(|(n, _)| n.trim().parse::<u64>().ok());
            if let Some(r) = ok {
                let first = r.text.lines().find_map(number)?;
                // The whole file read: nothing to add to its path.
                if first == 1 && !r.text.contains(" more lines; use offset ") {
                    return None;
                }
                let last = r.text.lines().rev().find_map(number)?;
                return Some(if first == last { format!("line {first}") } else { format!("lines {first}–{last}") });
            }
            let offset = input["offset"].as_u64();
            match (offset, input["limit"].as_u64()) {
                (_, Some(0)) => None,
                (o, Some(n)) => {
                    let a = o.unwrap_or(1).max(1);
                    Some(format!("lines {a}–{}", a + n - 1))
                }
                (Some(o), None) => Some(format!("from line {o}")),
                (None, None) => None,
            }
        }
        // How many results a SEARCH found.
        "SEARCH" => {
            let n = crate::tools::search_results(&ok?.text)?.len();
            Some(format!("{n} result{}", crate::plural(n)))
        }
        _ => None,
    }
}

/// Lines of a WRITE or EDIT shown while it comes in.
const PROGRESS_LINES: usize = 8;
/// The most lines of a running command's output its open row shows.
const OPEN_COMMAND_LINES: usize = 200;

/// Under a WRITE or EDIT that is still streaming in: how much of it has
/// come (lines and size) and its last few lines.
fn write_progress(ui: &mut Ui, t: &Theme, name: &str, input: &Value) {
    let (verb, text) = match name {
        "WRITE" => ("Writing", input["content"].as_str()),
        "EDIT" => match input["new_string"].as_str() {
            Some(s) => ("Writing the replacement", Some(s)),
            None => ("Finding the text to replace", input["old_string"].as_str()),
        },
        _ => return,
    };
    let text = text.unwrap_or("");
    let lines = text.lines().count();
    row_details(ui, NAME_X, |ui| {
        caption(ui, &format!("{verb}… {lines} line{}, {}", crate::plural(lines), size_text(text.len() as u64)), t.ink3);
        if !text.is_empty() {
            let all: Vec<&str> = text.lines().collect();
            let tail = all[all.len().saturating_sub(PROGRESS_LINES)..].join("\n");
            code_view(ui, t, highlight::for_path(input["path"].as_str().unwrap_or("")), &[(None, &tail)], None);
        }
    });
}

/// A running EXECUTE's output so far, in its open row: how long it has run
/// and how many lines it printed, then its last lines.
fn command_output(ui: &mut Ui, t: &Theme, id: &str) {
    let Some((lines, total, ran)) = crate::tasks::streaming(id, OPEN_COMMAND_LINES) else { return };
    let count = if total == 0 { "no output yet".to_string() } else { format!("{total} line{}", crate::plural(total)) };
    caption(ui, &format!("Running… {}, {count}", crate::tasks::fmt_duration(ran)), t.ink3);
    if !lines.is_empty() {
        let text = lines.join("\n");
        // The end of it, at most OUTPUT_SHOWN characters from a line start.
        let n = text.chars().count();
        let text = match text.char_indices().nth(n.saturating_sub(OUTPUT_SHOWN)).map(|(i, _)| i).filter(|&i| i > 0) {
            Some(i) => text[i..].split_once('\n').map_or(&text[i..], |(_, rest)| rest),
            None => text.as_str(),
        };
        code_block(ui, t, text.trim_end(), t.ink2);
    }
    // The time ticks on even while no output comes.
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
}

/// The fields of a tool input still streaming in, from its JSON so far:
/// string values as far as they came (the last one cut off). Other values
/// are skipped.
pub(crate) fn partial_input(raw: &str) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let mut it = raw.chars().peekable();
    let skip_ws = |it: &mut std::iter::Peekable<std::str::Chars>| {
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
    };
    // A JSON string after its opening quote; true when it ended.
    let string = |it: &mut std::iter::Peekable<std::str::Chars>| -> (String, bool) {
        let mut s = String::new();
        while let Some(c) = it.next() {
            match c {
                '"' => return (s, true),
                '\\' => match it.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some('b') => s.push('\u{8}'),
                    Some('f') => s.push('\u{c}'),
                    Some('u') => {
                        let hex: String = it.by_ref().take(4).collect();
                        let Ok(mut code) = u32::from_str_radix(&hex, 16) else { return (s, false) };
                        if hex.len() < 4 {
                            return (s, false);
                        }
                        if (0xD800..0xDC00).contains(&code) {
                            // A surrogate pair: the low half follows as \uXXXX.
                            let next: String = it.by_ref().take(6).collect();
                            match next.strip_prefix("\\u").and_then(|h| u32::from_str_radix(h, 16).ok()).filter(|_| next.len() == 6) {
                                Some(low) => code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF),
                                None => return (s, false),
                            }
                        }
                        s.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    }
                    Some(c) => s.push(c),
                    None => return (s, false),
                },
                c => s.push(c),
            }
        }
        (s, false)
    };
    skip_ws(&mut it);
    if it.next() != Some('{') {
        return out;
    }
    loop {
        skip_ws(&mut it);
        match it.next() {
            Some(',') => continue,
            Some('"') => {}
            _ => return out,
        }
        let (key, done) = string(&mut it);
        if !done {
            return out;
        }
        skip_ws(&mut it);
        if it.next() != Some(':') {
            return out;
        }
        skip_ws(&mut it);
        if it.peek() == Some(&'"') {
            it.next();
            let (value, done) = string(&mut it);
            out.insert(key, Value::String(value));
            if !done {
                return out;
            }
            continue;
        }
        // Not a string: skip to the next field.
        let (mut depth, mut quoted, mut escaped) = (0i32, false, false);
        loop {
            let Some(c) = it.peek().copied() else { return out };
            if quoted {
                it.next();
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => quoted = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                '[' | '{' => depth += 1,
                ']' | '}' if depth > 0 => depth -= 1,
                ',' | '}' if depth == 0 => break,
                _ => {}
            }
            it.next();
        }
    }
}

/// "Thinking…" while it streams, then "Thought for N tokens" (or, where the
/// API gave no count, "Thought for Ns"); clicking shows the (summarized)
/// thinking when there is any, or with Rewrite thinking its rewrite
/// (sections streaming in, "Rewriting…" while more is to come).
pub(crate) fn thinking_row(ui: &mut Ui, t: &Theme, b: &Value, key: (usize, usize), streaming: bool, rewrite: Option<&unslop::Thoughts>, in_block: bool) {
    let label = if streaming {
        "Thinking…".to_string()
    } else if let Some(n) = b["_oak_tokens"].as_u64().filter(|&n| n > 0) {
        format!("Thought for {} token{}", with_commas(n), crate::plural(n))
    } else if let Some(ms) = b["_oak_ms"].as_u64() {
        format!("Thought for {}s", ms.div_ceil(1000).max(1))
    } else {
        "Thought".to_string()
    };
    let text = b["text"].as_str().unwrap_or("").trim();
    let x = if in_block { NAME_X } else { 2.0 };
    let (rect, open, hovered) = row(ui, t, key_id("thinking-open", key), !text.is_empty(), in_block);
    let color = if hovered { t.ink2 } else { t.ink3 };
    ui.painter().text(egui::pos2(rect.left() + x, mid_y(ui, rect)), Align2::LEFT_CENTER, label, FontId::proportional(13.0), color);
    if open && !text.is_empty() {
        row_details(ui, x, |ui| match rewrite {
            None => {
                ui.label(RichText::new(text).color(t.ink3));
            }
            Some(r) => {
                if !r.text.is_empty() {
                    ui.label(RichText::new(&r.text).color(t.ink3));
                }
                if r.pending {
                    caption(ui, "Rewriting…", t.ink3);
                }
                for e in &r.errors {
                    caption(ui, &format!("Rewrite failed: {e}"), t.error);
                }
            }
        });
    }
}

/// A count with thousands separators: 12345 → "12,345".
fn with_commas(n: u64) -> String {
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

/// Tool calls from outside a conversation (the Duck.ai page), drawn as a
/// conversation's are: one block, the thinking before them first.
/// `results` are the calls' outputs by id, as text (true: failed).
pub(crate) fn tool_block(ui: &mut Ui, t: &Theme, thinking: Option<(&Value, (usize, usize), bool)>, calls: &[Value], results: &HashMap<String, (String, bool)>) {
    let results: Results = results
        .iter()
        .map(|(id, (text, error))| (id.as_str(), ToolOut { text: text.as_str().into(), images: vec![], error: *error, display: &[], cleared: false }))
        .collect();
    let mut items: Vec<Item> = vec![];
    if let Some((b, key, streaming)) = thinking {
        items.push(Item::Thinking(b, key, streaming, None));
    }
    items.extend(calls.iter().enumerate().map(|(i, b)| Item::Tool(b, (usize::MAX, i))));
    work_block(ui, t, &items, &results, None);
}

/// A run of tool calls (and the thinking between them) on one shared
/// background, so consecutive calls read as one block. With the
/// conversation's messages (`revert`), each row has the revert menu.
fn work_block(ui: &mut Ui, t: &Theme, run: &[Item], results: &Results, revert: Option<&[Value]>) {
    let font = FontId::proportional(13.0);
    let width = |n: &str| ui.fonts_mut(|f| f.layout_no_wrap(n.to_string(), font.clone(), Color32::WHITE).size().x);
    let longest = run
        .iter()
        .filter_map(|x| if let Item::Tool(b, _) = x { Some(shown(b)) } else { None })
        .map(|n| width(&n))
        .fold(0.0, f32::max)
        // The summaries line up after the names, but one long name (such as
        // TASK_GET_NEW_OUTPUT) shouldn't push them all far to the right.
        .min(width(&"M".repeat(NAME_COLUMN)) * 0.8);
    ui.add_space(2.0);
    egui::Frame::new().fill(t.tool_bg).inner_margin(Margin::symmetric(0, 3)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        for x in run {
            let (rect, key) = match x {
                Item::Tool(b, key) => (ui.scope(|ui| tool_row(ui, t, b, results, longest)).response.rect, *key),
                Item::Thinking(b, key, streaming, rewrite) => (ui.scope(|ui| thinking_row(ui, t, b, *key, *streaming, rewrite.as_ref(), true)).response.rect, *key),
                _ => continue,
            };
            item_menu(ui, rect, key_id("item-menu", key), None, revert.map(|m| (m, key)));
        }
    });
    ui.add_space(2.0);
}

/// The whole transcript, laid out at once (tests; the app shows it as a
/// list, `vlist`).
#[cfg(test)]
pub(crate) fn transcript_view(ui: &mut Ui, t: &Theme, conv: &Conversation, live: &Live) {
    let (items, _, results) = items_at(conv, live);
    ui.add_space(20.0);
    for u in units(&items) {
        transcript_unit(ui, t, &items, u, &results, Some(&conv.messages));
    }
    transcript_status(ui, t, conv, live, false);
}

/// The transcript in units: a run of tool calls and thinking (shown as one
/// block), or any other single item.
pub(crate) fn units(items: &[Item]) -> Vec<std::ops::Range<usize>> {
    let work = |x: &Item| matches!(x, Item::Thinking(..) | Item::Tool(..));
    let mut out = vec![];
    let mut i = 0;
    while i < items.len() {
        let end = if work(&items[i]) { (i..items.len()).find(|&j| !work(&items[j])).unwrap_or(items.len()) } else { i + 1 };
        out.push(i..end);
        i = end;
    }
    out
}

/// What a unit's height depends on, hashed: when it changes, the unit is
/// laid out again (opened rows are on screen, so they are anyway).
pub(crate) fn unit_sig(run: &[Item], results: &Results) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = QuickHasher::default();
    for x in run {
        std::mem::discriminant(x).hash(&mut h);
        match x {
            Item::Summary { text, cut, time } => (text.len(), cut, time).hash(&mut h),
            Item::Note(s) | Item::Beat(s) | Item::AskReply(s) => (addr(s), s.len()).hash(&mut h),
            Item::AgentMessage { body, key, last, .. } => (body.len(), key, last).hash(&mut h),
            Item::Mark(m) => (m.at, m.kind as u8, m.text.len(), m.time, m.done, m.sent.len()).hash(&mut h),
            Item::UserText(s, r, _, end) => (s.len(), r.map(|r| (r.msg, r.block)), end).hash(&mut h),
            Item::Image(b, r) | Item::File(b, r) => (*b as *const Value as usize, r.map(|r| (r.msg, r.block))).hash(&mut h),
            Item::Text(s, key, _, tps, rewrite, end) => {
                (s.len(), key, tps.map(f64::to_bits), end).hash(&mut h);
                rewrite.as_ref().map(|r| (unslop::no_drafts(), match r {
                    unslop::State::Drafting => (0, 0),
                    unslop::State::Streaming(t) => (1, t.len()),
                    unslop::State::Done(t) => (2, t.len()),
                    unslop::State::Failed(e) => (3, e.len()),
                })).hash(&mut h);
            }
            Item::Thinking(b, key, streaming, rewrite) => {
                let [text, ms, tokens] = crate::fields(b, ["text", "_oak_ms", "_oak_tokens"]);
                (key, streaming, text.and_then(Value::as_str).map_or(0, str::len), ms.and_then(Value::as_u64), tokens.and_then(Value::as_u64)).hash(&mut h);
                rewrite.as_ref().map(|r| (r.text.len(), r.pending, r.errors.len())).hash(&mut h);
            }
            Item::Tool(b, key) | Item::Display(b, key) => {
                let [id, partial] = crate::fields(b, ["id", "_oak_partial"]);
                let id = id.and_then(Value::as_str).unwrap_or("");
                (id, key).hash(&mut h);
                partial.and_then(Value::as_str).map(str::len).hash(&mut h);
                let result = results.get(id);
                result.map(|r| (r.error, r.text.len(), r.images.len(), r.display.len())).hash(&mut h);
                // A running command's output, in its open row, grows.
                if result.is_none() {
                    crate::tasks::streaming_len(id).hash(&mut h);
                }
            }
        }
    }
    h.finish()
}

/// The transcript's end: what OAK is doing, its questions and the queue.
pub(crate) fn tail_sig(conv: &Conversation, live: &Live, editing: &Option<Editing>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    (conv.paused, live.pause_queued, live.pause_settled(conv.paused), live.busy, &live.status, live.ask.is_some(), &conv.ask_request, live.queued.len()).hash(&mut h);
    for q in &live.queued {
        (q.id, q.text.len(), q.images.len()).hash(&mut h);
    }
    editing.as_ref().map(|e| (e.id, e.text.len())).hash(&mut h);
    h.finish()
}

/// Draws one unit (`units`). Its items' text has a right-click menu to copy
/// it (`item_menu`); with the conversation's messages (`revert`), the
/// model's items and the messages from other sessions also revert to
/// before them there.
pub(crate) fn transcript_unit(ui: &mut Ui, t: &Theme, items: &[Item], u: std::ops::Range<usize>, results: &Results, revert: Option<&[Value]>) {
    let run = &items[u.clone()];
    let menu = |ui: &mut Ui, rect: Rect, key: (usize, usize), message: Option<&dyn Fn() -> String>| {
        item_menu(ui, rect, key_id("item-menu", key), message, revert.map(|m| (m, key)));
    };
    if matches!(run[0], Item::Thinking(..) | Item::Tool(..)) {
        if run.iter().any(|x| matches!(x, Item::Tool(..))) {
            work_block(ui, t, run, results, revert);
        } else {
            for x in run {
                if let Item::Thinking(b, key, streaming, rewrite) = x {
                    let r = ui.scope(|ui| thinking_row(ui, t, b, *key, *streaming, rewrite.as_ref(), false)).response.rect;
                    menu(ui, r, *key, None);
                }
            }
        }
        return;
    }
    {
        match &run[0] {
            Item::Summary { text, cut, time } => summary_banner(ui, t, text, *cut, *time),
            Item::Note(note) => notice(ui, t, note, "note-open", TASK_NOTE_ICON, (agent::ALERT_TAG, "</usage-alert>"), Some((NOTE_TAG, "</task-notification>"))),
            Item::Beat(beat) => notice(ui, t, beat, "beat-open", HEARTBEAT_ICON, (BEAT_TAG, "</heartbeat>"), None),
            Item::AskReply(text) => {
                let body = text.strip_prefix(agent::ASK_TAG).unwrap_or(text).trim_end_matches("</ask-request>").trim();
                let label = if body.starts_with("The user accepted") { "you accepted Oak's request to ask you" } else { "you declined Oak's request to ask you" };
                let id = Id::unique(("ask-reply-open", addr(text), text.len()));
                notice_row(ui, t, id, label, Some(body), text, |p, r| {
                    p.circle_filled(egui::pos2(r.left() + 8.0, r.center().y), 3.5, tool_color("ASK"));
                });
            }
            Item::Mark(m) => mark_row(ui, t, m),
            Item::AgentMessage { from, last, body, key } => {
                let r = ui.scope(|ui| agent_message_row(ui, t, from, *last, body, *key)).response.rect;
                menu(ui, r, *key, Some(&|| body.to_string()));
            }
            Item::UserText(text, revert, at, end) => {
                ui.add_space(6.0);
                let right = if revert.is_some() { 36 } else { 12 };
                let select_id = Id::unique(("user-text", addr(text)));
                let frame = crate::select::message(ui, select_id, || text.to_string(), |ui| egui::Frame::new()
                    .fill(t.panel)
                    .stroke(Stroke::new(1.0, t.line))
                    .inner_margin(Margin { left: 12, right, top: 12, bottom: 12 })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if crate::find::active() {
                            // Matches marked (Ctrl+F).
                            let job = egui::text::LayoutJob::simple(text.to_string(), egui::TextStyle::Body.resolve(ui.style()), ui.visuals().text_color(), ui.available_width());
                            markdown::marked_label(ui, job);
                        } else {
                            ui.label(*text);
                        }
                    }));
                let r = frame.response.rect;
                let id = Id::unique(("user-menu", addr(text)));
                item_menu(ui, r, id, Some(&|| text.to_string()), None);
                if let Some(at) = revert {
                    revert_control(ui, t, Rect::from_min_size(egui::pos2(r.right() - 30.0, r.top() + 8.0), vec2(22.0, 22.0)), *at);
                }
                if *end {
                    message_bar(ui, t, r, Id::unique(("user-bar", addr(text))), || turn_text(items, u.start), *at, None);
                }
            }
            Item::Image(b, revert) => {
                ui.add_space(4.0);
                if let Some(r) = images::show(ui, b, 240.0)
                    && let Some(at) = revert
                {
                    revert_control(ui, t, Rect::from_min_size(egui::pos2(r.rect.right() + 6.0, r.rect.top()), vec2(22.0, 22.0)), *at);
                }
            }
            Item::File(b, revert) => {
                ui.add_space(4.0);
                let r = file_chip(ui, t, b);
                if let Some(at) = revert {
                    revert_control(ui, t, Rect::from_min_size(egui::pos2(r.rect.right() + 6.0, r.rect.center().y - 11.0), vec2(22.0, 22.0)), *at);
                }
            }
            Item::Text(text, key, at, tps, rewrite, end) => {
                // (The menu takes the whole item: with Unslop, its draft's row too.)
                let shown = ui.scope(|ui| match rewrite {
                    Some(r) => unslop_view(ui, t, text, *key, r),
                    None => ui.scope(|ui| markdown(ui, t, text, *key)).response.rect,
                });
                let r = shown.inner;
                let message = || match rewrite {
                    Some(r) => shown_text(text, r).to_string(),
                    None => text.to_string(),
                };
                menu(ui, shown.response.rect, *key, Some(&message));
                if *end {
                    message_bar(ui, t, r, key_id("reply-bar", *key), || turn_text(items, u.start), *at, *tps);
                }
            }
            Item::Display(b, key) => {
                let r = ui.scope(|ui| display_view(ui, t, b, results)).response.rect;
                menu(ui, r, *key, None);
            }
            Item::Thinking(..) | Item::Tool(..) => {}
        }
    }
}

/// What a text block shows with Unslop: the rewrite once there is one,
/// the model's own text if it failed (empty while either is coming).
pub(crate) fn shown_text<'a>(text: &'a str, r: &'a unslop::State) -> &'a str {
    match r {
        unslop::State::Drafting => "",
        unslop::State::Streaming(s) => s,
        unslop::State::Done(s) => s,
        unslop::State::Failed(_) => text,
    }
}

/// A text block with Unslop: a "Drafted a message" row that opens on the
/// model's own text, then the rewrite as the reply (streaming in while it
/// comes). With No drafts, just the rewrite, after "Writing…" while the
/// model drafts and the rewrite starts. Returns the reply's rect, for its
/// message bar.
fn unslop_view(ui: &mut Ui, t: &Theme, text: &str, key: (usize, usize), r: &unslop::State) -> Rect {
    let no_drafts = unslop::no_drafts();
    if !no_drafts {
        let drafting = matches!(r, unslop::State::Drafting);
        let (rect, open, hovered) = row(ui, t, key_id("unslop-open", key), true, false);
        let color = if hovered { t.ink2 } else { t.ink3 };
        let p = ui.painter();
        chevron(p, egui::pos2(rect.left() + 7.0, mid_y(ui, rect)), open, color);
        let label = if drafting { "Drafting a message…" } else { "Drafted a message" };
        p.text(egui::pos2(rect.left() + 18.0, mid_y(ui, rect)), Align2::LEFT_CENTER, label, FontId::proportional(13.0), color);
        if open {
            row_details(ui, 18.0, |ui| {
                crate::widgets::inset(ui, t, Color32::TRANSPARENT, 10, |ui| {
                    markdown::show(ui, t, text, key_id("unslop-original", key));
                });
            });
        }
        if drafting {
            return rect;
        }
    }
    match r {
        unslop::State::Drafting | unslop::State::Streaming(_) if shown_text(text, r).is_empty() => {
            ui.label(RichText::new(if no_drafts { "Writing…" } else { "Rewriting…" }).color(t.ink3)).rect
        }
        unslop::State::Failed(e) => {
            let r = ui.scope(|ui| markdown(ui, t, text, key)).response.rect;
            caption(ui, &format!("Rewrite failed: {e}"), t.error);
            r
        }
        _ => ui.scope(|ui| markdown(ui, t, shown_text(text, r), key)).response.rect,
    }
}

/// Where the transcript asks the app to revert: (message, block, that block).
pub(crate) const REVERT_REQUEST: &str = "oak-revert-request";

/// The revert button of a message the user sent, at `rect`. A click asks
/// first, under the message; confirming leaves the request for the app
/// (`REVERT_REQUEST`).
fn revert_control(ui: &mut Ui, t: &Theme, rect: Rect, at: RevertAt) {
    let id = ui.scope_id().with(("revert", at.msg, at.block));
    let mut asking = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let resp = ui.interact(rect, id.with("button"), Sense::click());
    let lit = resp.hovered() || asking;
    if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, t.panel2);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    draw_revert(ui.painter(), rect, if lit { t.ink } else { t.ink3 });
    if resp
        .on_hover_text("Removes this message and all later messages; restores this message as a draft. Files are unchanged.")
        .clicked()
    {
        asking = !asking;
    }
    if asking {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            caption(ui, "Remove this message and all later messages? Files are unchanged.", t.ink2);
            if ui.add(Button::new(RichText::new("Revert").color(t.accent))).clicked() {
                ui.data_mut(|d| d.insert_temp(Id::unique(REVERT_REQUEST), (at.msg, at.block, at.first.clone())));
                asking = false;
            }
            if ui.button("Cancel").clicked() {
                asking = false;
            }
        });
    }
    ui.data_mut(|d| d.insert_temp(id, asking));
}

/// Where the transcript asks the app to revert to before one of the
/// model's items (or a message from another session): an `agent::RevertTo`
/// (in an `Option`, which egui can take out).
pub(crate) const REWIND_REQUEST: &str = "oak-rewind-request";

/// The right-click menu on one of the transcript's items (`rect`): copying
/// (the selection, the item's text with `message`, a link), and with the
/// conversation's messages (`revert`) and the item's `key`, Revert to
/// before this (`key`: its message and block, the message `usize::MAX` for
/// the reply streaming in; choosing it leaves the request for the app,
/// `REWIND_REQUEST`).
fn item_menu(ui: &mut Ui, rect: Rect, id: Id, message: Option<&dyn Fn() -> String>, revert: Option<(&[Value], (usize, usize))>) {
    let entry = |ui: &mut Ui| {
        let Some((messages, key)) = revert else { return };
        let resp = ui
            .button("Revert to before this")
            .on_hover_text("Removes this and everything after it from the conversation; whatever Oak is doing stops. Files are unchanged.");
        if resp.clicked()
            && let Some(to) = revert_target(key, messages)
        {
            ui.data_mut(|d| d.insert_temp(Id::unique(REWIND_REQUEST), Some(to)));
        }
    };
    crate::copy_menu::show(ui, id, rect, message, revert.is_some().then_some(&entry as &dyn Fn(&mut Ui)));
}

/// Where reverting to before the item at `key` goes back to: before its
/// block, or for the reply streaming in, to the messages there are now.
fn revert_target(key: (usize, usize), messages: &[Value]) -> Option<agent::RevertTo> {
    if key.0 == usize::MAX {
        return Some(agent::RevertTo::End(messages.len()));
    }
    let block = messages.get(key.0)?["content"].get(key.1)?;
    Some(agent::RevertTo::Block(key.0, key.1, block.clone()))
}

/// The height of a message's bar (`message_bar`).
const BAR_H: f32 = 22.0;

/// Under the message that ends a turn (`msg`, its rect): buttons (copy,
/// for now; `copy_text` gives what it copies) and when it was sent, shown while the
/// pointer is over the message or the bar. The
/// room is kept either way, so nothing moves when it shows. The time is in
/// words ("3 days ago"); hovering it gives the full date and time. A
/// reply also shows its output speed (`tps`), only here, never pinned.
pub(crate) fn message_bar(ui: &mut Ui, t: &Theme, msg: Rect, id: Id, copy_text: impl FnOnce() -> String, at: Option<i64>, tps: Option<f64>) {
    // Tucked right under the message, not a full item gap below it.
    ui.add_space(2.0 - ui.spacing().item_spacing.y);
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
    let now = ui.input(|i| i.time);
    // A copy shows its check mark for a moment, pointer or not.
    let copied = ui.data(|d| d.get_temp::<f64>(id.with("copied"))).is_some_and(|c| now - c < COPIED_FOR);
    let over = ui.rect_contains_pointer(msg.union(bar));
    let shown = ui.ctx().animate_bool_with_time(id.with("shown"), over || copied, 0.12);
    if shown <= 0.0 {
        return;
    }
    let fade = |c: Color32| c.gamma_multiply(shown);
    let p = ui.painter();
    let copy = Rect::from_min_size(egui::pos2(bar.left(), bar.center().y - 11.0), vec2(22.0, 22.0));
    let resp = ui.interact(copy, id.with("copy"), Sense::click());
    if resp.hovered() {
        p.rect_filled(copy, 0.0, fade(t.panel2));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if copied {
        paint_check(p, copy.center() - vec2(5.0, 5.0), 10.0, fade(t.accent));
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(COPIED_FOR));
    } else {
        draw_copy(p, copy, fade(if resp.hovered() { t.ink } else { t.ink3 }));
    }
    if resp.on_hover_text(if copied { "Copied" } else { "Copy" }).clicked() {
        ui.ctx().copy_text(copy_text().trim_end().to_string());
        ui.data_mut(|d| d.insert_temp(id.with("copied"), now));
    }
    let mut x = copy.right() + 8.0;
    let mut label = |text: String, key: &str, tip: String| {
        let galley = p.layout_no_wrap(text, FontId::proportional(12.0), fade(t.ink3));
        let pos = egui::pos2(x, bar.center().y - galley.size().y / 2.0);
        let r = Rect::from_min_size(pos, galley.size());
        x = r.right() + 12.0;
        p.galley(pos, galley, fade(t.ink3));
        ui.interact(r, id.with(key), Sense::hover()).on_hover_text(tip);
    };
    if let Some(at) = at {
        let (now_ms, off) = (api::now_ms(), local_offset_ms());
        label(time_ago(at, now_ms, off), "time", full_time(at, off));
    }
    if let Some(tps) = tps {
        label(
            format!("{tps:.1} tok/s"),
            "speed",
            "Provider-reported output tokens divided by output duration.".into(),
        );
    }
}

/// How long a message's copy button shows its check mark, in seconds.
const COPIED_FOR: f64 = 1.5;

/// Two sheets, one over the other: copy.
fn draw_copy(p: &egui::Painter, r: Rect, c: Color32) {
    let a = (r.width() * 0.36).round();
    let front = Rect::from_min_size(r.center() + vec2(-a / 2.0 + 1.5, -a / 2.0 + 1.5), vec2(a, a));
    let back = front.translate(vec2(-3.0, -3.0));
    let stroke = Stroke::new(1.3, c);
    p.rect_stroke(front, 1.5, stroke, egui::StrokeKind::Middle);
    // Only what the front sheet leaves showing of the back one.
    let pts = vec![
        egui::pos2(front.left(), back.bottom()),
        back.left_bottom(),
        back.left_top(),
        back.right_top(),
        egui::pos2(back.right(), front.top()),
    ];
    p.add(egui::Shape::line(pts, stroke));
}

/// An arrow curving back over the top to the left: undo.
fn draw_revert(p: &egui::Painter, r: Rect, c: Color32) {
    let (m, rad) = (r.center() + vec2(0.5, 1.0), r.width() * 0.24);
    let n = 14;
    let (from, to) = (std::f32::consts::PI, -std::f32::consts::FRAC_PI_2);
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            m + vec2(a.cos(), -a.sin()) * rad
        })
        .collect();
    let stroke = Stroke::new(1.4, c);
    p.add(egui::Shape::line(pts.clone(), stroke));
    // The head, at the left end, pointing down.
    let tip = pts[0] + vec2(0.0, 2.5);
    p.line_segment([tip, tip + vec2(-3.0, -3.0)], stroke);
    p.line_segment([tip, tip + vec2(3.0, -3.0)], stroke);
}

/// A DISPLAY call: the files it shows, right in the chat. Images as
/// pictures, text files as the start of their text, anything else as its
/// name; each name opens the file.
fn display_view(ui: &mut Ui, t: &Theme, b: &Value, results: &Results) {
    ui.add_space(4.0);
    let Some(out) = results.get(b["id"].as_str().unwrap_or("")) else {
        caption(ui, "Showing files…", t.ink3);
        return;
    };
    if out.error {
        caption(ui, &format!("Couldn't show the files: {}", out.text), t.error);
        return;
    }
    for f in out.display {
        let path = f["path"].as_str().unwrap_or("");
        let name = f["name"].as_str().unwrap_or(path);
        if f.get("image").is_some() {
            images::show(ui, &f["image"], 420.0);
            file_link(ui, t, name, path, "");
        } else if let Some(text) = f["text"].as_str() {
            let lines = f["lines"].as_u64().unwrap_or(0);
            crate::widgets::inset(ui, t, t.panel, 10, |ui| {
                file_link(ui, t, name, path, &format!("{lines} line{}", crate::plural(lines)));
                ui.add_space(2.0);
                let shown = text.lines().count() as u64;
                ui.label(RichText::new(if text.trim().is_empty() { "(empty file)" } else { text.trim_end() }).font(markdown::code_font(12.5)).color(t.ink2));
                if lines > shown {
                    caption(ui, &format!("… {} more line{}", lines - shown, crate::plural(lines - shown)), t.ink3);
                }
            });
        } else {
            egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                file_link(ui, t, name, path, &size_text(f["bytes"].as_u64().unwrap_or(0)));
            });
        }
        ui.add_space(6.0);
    }
}

/// A file's name, which opens it with the system's default app; `note`
/// goes after it, faint.
fn file_link(ui: &mut Ui, t: &Theme, name: &str, path: &str, note: &str) {
    let icons = markdown::icon_font(13.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(FILE_ICON).font(icons).color(t.ink3));
        let resp = ui.add(egui::Label::new(RichText::new(name).small().color(t.ink2)).sense(Sense::click()));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.on_hover_text(path).clicked() {
            let _ = open::that(path);
        }
        if !note.is_empty() {
            caption(ui, note, t.ink3);
        }
    });
}

/// Under the transcript's end: paused, or what OAK is doing. "Paused" shows
/// once the pause has taken hold, and `agents_pausing` (its subagents still
/// pausing) holds it back too; until then the pause button spins and this
/// says what OAK is still doing.
pub(crate) fn transcript_status(ui: &mut Ui, t: &Theme, conv: &Conversation, live: &Live, agents_pausing: bool) {
    if live.pause_settled(conv.paused) && !agents_pausing {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            paint_pause(ui.painter(), r, t.accent);
            let what = if live.ultra { "Stopped (ultrapause)" } else { "Paused" };
            ui.label(RichText::new(what).color(t.ink2));
        });
    } else if live.busy || conv.paused || live.pause_queued {
        let status = if live.pause_settled(conv.paused) {
            "Pausing subagents"
        } else if live.status.is_empty() || live.status == "Paused" {
            "Pausing"
        } else {
            live.status.as_str()
        };
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(status).color(t.ink3));
        });
    }
}

/// ASK's questions: options to click and an Other field for each. True
/// when the user sends the answers.
pub(crate) fn ask_view(ui: &mut Ui, t: &Theme, a: &mut agent::Ask) -> bool {
    let mut send = false;
    ask_card(ui, t, "Questions", |ui| {
        for i in 0..a.questions.len() {
            if i > 0 {
                ui.add_space(10.0);
            }
            let multiple = a.questions[i].multiple;
            ui.label(RichText::new(&a.questions[i].text).strong().color(t.ink));
            ui.add_space(2.0);
            let choice = |ui: &mut Ui, on: bool, text: &str| {
                if multiple {
                    let mut v = on;
                    ui.checkbox(&mut v, text).clicked()
                } else {
                    ui.radio(on, text).clicked()
                }
            };
            for o in 0..a.questions[i].options.len() {
                if choice(ui, a.picked[i][o], &a.questions[i].options[o].clone()) {
                    a.pick(i, Some(o));
                }
            }
            ui.horizontal(|ui| {
                if choice(ui, a.other[i].1, "Other:") {
                    a.pick(i, None);
                }
                let edit = crate::text_menu::add(
                    ui,
                    TextEdit::singleline(&mut a.other[i].0)
                        .id(ui.scope_id().with(("ask-other", i)))
                        .hint_text("Answer")
                        .desired_width(ui.available_width().min(460.0)),
                );
                // Writing an answer chooses it.
                if edit.changed() && !a.other[i].0.trim().is_empty() && !a.other[i].1 {
                    a.pick(i, None);
                }
            });
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let ready = a.ready();
            if ui.add_enabled(ready, Button::new(RichText::new("Send answers").color(if ready { t.accent } else { t.ink3 }))).clicked() {
                send = true;
            }
            if ui
                .button(RichText::new("Skip").color(t.ink2))
                .on_hover_text("Continue without answers; queued messages are sent.")
                .clicked()
            {
                a.skipped = true;
            }
            let left = (0..a.questions.len()).filter(|&i| !a.answered(i)).count();
            let wait = "messages queued until answers are sent or skipped";
            let hint = if left > 0 { format!("{left} unanswered, {wait}") } else { wait.into() };
            caption(ui, &hint, t.ink3);
        });
    });
    send
}

/// A card the model's questions (or its request to ask some) wait in for
/// the user: `title` over what `f` adds.
fn ask_card(ui: &mut Ui, t: &Theme, title: &str, f: impl FnOnce(&mut Ui)) {
    ui.add_space(6.0);
    egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.accent)).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        caption(ui, title, t.accent);
        ui.add_space(4.0);
        f(ui);
    });
    ui.add_space(6.0);
}

/// A REQUEST_ASK waiting for the user: what the model wants to ask them
/// about, to accept or decline. Some(true) when accepted, Some(false)
/// when declined.
pub(crate) fn ask_request_view(ui: &mut Ui, t: &Theme, about: &str) -> Option<bool> {
    let mut reply = None;
    ask_card(ui, t, "Question request", |ui| {
        ui.label(RichText::new(about).strong().color(t.ink));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .button(RichText::new("Accept").color(t.accent))
                .on_hover_text("Allow one set of questions; pauses for answers.")
                .clicked()
            {
                reply = Some(true);
            }
            if ui.button(RichText::new("Decline").color(t.ink2)).on_hover_text("Continue without questions").clicked() {
                reply = Some(false);
            }
        });
    });
    reply
}

/// Queued messages, dimmed, below the transcript. Hovering one shows edit
/// and remove buttons; the one being edited becomes a text box.
pub(crate) fn queue_view(ui: &mut Ui, t: &Theme, conv: &str, queued: &[agent::Queued], editing: &mut Option<Editing>) -> Option<QueueAction> {
    let mut action = None;
    for q in queued {
        if let Some(e) = editing.as_mut().filter(|e| e.conv == conv && e.id == q.id) {
            egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.accent)).inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let id = Id::unique(("queue-edit", conv, q.id));
                let enter = ui.memory(|m| m.has_focus(id))
                    && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter));
                let resp = crate::text_menu::add(
                    ui,
                    TextEdit::multiline(&mut e.text)
                        .id(id)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
                );
                if std::mem::take(&mut e.focus) {
                    resp.request_focus();
                }
                let esc = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape));
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() || enter {
                        action = Some(QueueAction::Save);
                    }
                    if ui.button("Cancel").clicked() || esc {
                        action = Some(QueueAction::Cancel);
                    }
                });
            });
            ui.add_space(4.0);
            continue;
        }
        let card = egui::Frame::new()
            .stroke(Stroke::new(1.0, t.line))
            .inner_margin(Margin { left: 12, right: 72, top: 10, bottom: 10 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if !q.images.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for img in &q.images {
                            attachment_view(ui, t, img, 48.0);
                        }
                    });
                }
                if !q.text.trim().is_empty() {
                    ui.label(RichText::new(&q.text).color(t.ink3));
                }
                ui.label(RichText::new("queued").small().color(t.ink3.gamma_multiply(0.7)));
            });
        let rect = card.response.rect;
        if ui.rect_contains_pointer(rect) {
            let size = vec2(24.0, 24.0);
            let remove = Rect::from_min_size(egui::pos2(rect.right() - 8.0 - size.x, rect.top() + 8.0), size);
            let edit = remove.translate(vec2(-size.x - 4.0, 0.0));
            if icon_button(ui, t, edit, Id::unique(("queue-pencil", conv, q.id)), "Edit", draw_pencil) {
                action = Some(QueueAction::Edit(q.id, q.text.clone()));
            }
            if icon_button(ui, t, remove, Id::unique(("queue-x", conv, q.id)), "Remove from queue", draw_x) {
                action = Some(QueueAction::Remove(q.id));
            }
        }
        ui.add_space(4.0);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{run_frame, test_ctx};

    /// A reply's widgets (its copy bar, text, menu) are its conversation's
    /// own: a side chat's copy of a reply never shares them with its source.
    #[test]
    fn reply_ids_belong_to_their_conversation() {
        let key = (3, 1);
        let source = for_conversation("source", || key_id("reply-bar", key));
        assert_ne!(source, for_conversation("side", || key_id("reply-bar", key)));
        assert_eq!(source, for_conversation("source", || key_id("reply-bar", key)));
        assert_ne!(source, key_id("reply-bar", key), "the conversation is let go after drawing");
    }

    /// Long tool outputs are cut after a whole line, counting the lines left.
    #[test]
    fn long_outputs_cut_at_a_line() {
        let short = "a\nb";
        assert_eq!(cut_output(short), (short.to_string(), 0));
        let line = "x".repeat(99);
        let text: String = (0..100).map(|_| format!("{line}\n")).collect();
        let (out, more) = cut_output(&text);
        assert_eq!((out.lines().count(), more), (60, 40));
        assert!(out.lines().all(|l| l == line));
        // One huge line: cut where it must be, the rest counts as a line.
        let (out, more) = cut_output(&"y".repeat(OUTPUT_SHOWN + 5));
        assert_eq!((out.len(), more), (OUTPUT_SHOWN, 1));
    }

    /// A tool row's name and summary (in fonts of different heights) keep
    /// their places relative to each other wherever the row lands, to a
    /// fraction of a pixel: they don't jiggle as rows above open and close.
    #[test]
    fn tool_row_text_keeps_its_line() {
        let (ctx, t) = test_ctx(true, false);
        let b = serde_json::json!({"type": "tool_use", "id": "c1", "name": "EXECUTE", "input": {"command": "ls -la", "timeout": 5}});
        let results = Results::default();
        let mut gaps = std::collections::BTreeSet::new();
        let ppp = 1.25;
        ctx.set_pixels_per_point(ppp);
        let mut time = 0.0;
        for i in 0..12 {
            let out = run_frame(&ctx, vec2(700.0, 200.0), &mut time, 1.0 / 60.0, vec![], |ui| {
                ui.add_space(10.0 + i as f32 * 0.37);
                tool_row(ui, &t, &b, &results, 60.0);
            });
            // Where the tessellator puts each text: on whole pixels.
            let tops: Vec<f32> = out
                .shapes
                .iter()
                .filter_map(|s| if let egui::Shape::Text(x) = &s.shape { Some((x.pos.y * ppp).round() / ppp + x.galley.rows[0].pos.y) } else { None })
                .collect();
            assert!(tops.len() >= 2, "{tops:?}");
            gaps.insert(((tops[1] - tops[0]) * 1000.0).round() as i32);
        }
        assert_eq!(gaps.len(), 1, "name and summary moved apart: {gaps:?}");
    }

    /// A WRITE streaming in: its fields as far as they came, the last one
    /// cut off, escapes decoded; other values skipped.
    #[test]
    fn partial_tool_input() {
        let p = partial_input(r#"{"path": "src/a.rs", "n": [1, {"x": "}"}], "content": "fn main() {\n    println!(\"h\u00e9"#);
        assert_eq!(p["path"], "src/a.rs");
        assert_eq!(p["content"], "fn main() {\n    println!(\"hé");
        assert!(p.get("n").is_none());
        assert!(partial_input(r#"{"pa"#).is_empty());
        assert_eq!(partial_input(r#"{"content": "a\ud83d\ude00b"}"#)["content"], "a😀b");
        assert_eq!(partial_input(r#"{"content": "half \u00"#)["content"], "half ");
    }
}