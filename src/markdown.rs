//! Markdown for assistant replies: parsed with pulldown-cmark into a small
//! tree of blocks, then drawn with egui. Paragraphs are one LayoutJob each,
//! so wrapping, selection and clickable links all work on whole paragraphs.
//!
//! A reply streaming in is drawn every frame, and is one unit of the
//! transcript's list, laid out whole. So a text that goes on from the one
//! parsed last is parsed only from its last elements on
//! (`Parsed::extended`), and a long text's blocks far off screen are only
//! the space they took when last drawn, or, laid out off screen for the
//! minimap, their ink (`draw_long`).

use crate::Theme;
pub use crate::look::{CODE, ICONS, code_font, icon_font};
use crate::highlight;
use eframe::egui::{
    self, Align, Color32, FontData, FontDefinitions, FontFamily, FontId, Id, Label, Layout, Margin, RichText, ScrollArea,
    Stroke, TextFormat, Ui, text::LayoutJob, vec2,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Hash)]
pub struct Style {
    pub strong: bool,
    pub em: bool,
    pub strike: bool,
    pub code: bool,
    /// A link titled "cite" (`[site](url "cite")`): a source an answer
    /// cites, drawn as a small chip rather than a link in the text.
    pub cite: bool,
}

#[derive(Clone, Debug, PartialEq, Hash)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Hash)]
pub struct Item {
    /// Some for task-list items: whether the box is ticked.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Hash)]
pub enum Block {
    Para(Vec<Span>),
    Heading(u8, Vec<Span>),
    Code { lang: String, text: String },
    List { start: Option<u64>, items: Vec<Item> },
    Quote(Vec<Block>),
    Rule,
    Table { head: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>> },
}

// ---------------------------------------------------------------- parsing

enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List(Option<u64>, Vec<Item>),
    Item(Option<bool>, Vec<Block>),
    Table { head: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>>, row: Vec<Vec<Span>> },
}

#[derive(Clone, Copy)]
enum InlineKind {
    Para,
    Heading(u8),
    Cell,
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    inline: Option<(InlineKind, Vec<Span>)>,
    code: Option<(String, String)>,
    strong: u32,
    em: u32,
    strike: u32,
    links: Vec<String>,
    /// Whether each open link is a citation (`Style::cite`).
    cites: Vec<bool>,
}

impl Builder {
    fn push_block(&mut self, b: Block) {
        match self.stack.last_mut() {
            Some(Frame::Blocks(bs) | Frame::Quote(bs) | Frame::Item(_, bs)) => bs.push(b),
            // A block where only items belong (not produced by pulldown-cmark): keep it anyway.
            Some(Frame::List(_, items)) => items.push(Item { task: None, blocks: vec![b] }),
            Some(Frame::Table { .. }) | None => {}
        }
    }

    fn open_frame(&mut self, f: Frame) {
        self.close_inline();
        self.stack.push(f);
    }

    fn open_inline(&mut self, kind: InlineKind) {
        self.close_inline();
        self.inline = Some((kind, vec![]));
    }

    fn close_inline(&mut self) {
        let Some((kind, mut spans)) = self.inline.take() else { return };
        // Trailing whitespace from soft breaks at the end of a paragraph.
        if let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end().len();
            last.text.truncate(trimmed);
        }
        spans.retain(|s| !s.text.is_empty());
        let spans = autolink(spans);
        match kind {
            InlineKind::Cell => {
                if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                    row.push(spans);
                }
            }
            _ if spans.is_empty() => {}
            InlineKind::Para => self.push_block(Block::Para(spans)),
            InlineKind::Heading(level) => self.push_block(Block::Heading(level, spans)),
        }
    }

    fn text(&mut self, text: &str, code: bool) {
        if let Some((_, t)) = &mut self.code {
            t.push_str(text);
            return;
        }
        if self.inline.is_none() {
            // Tight list items carry their text without a paragraph.
            self.inline = Some((InlineKind::Para, vec![]));
        }
        let style = Style { strong: self.strong > 0, em: self.em > 0, strike: self.strike > 0, code, cite: self.cites.last() == Some(&true) };
        let link = self.links.last().cloned();
        let spans = &mut self.inline.as_mut().unwrap().1;
        match spans.last_mut() {
            Some(last) if last.style == style && last.link == link => last.text.push_str(text),
            _ => spans.push(Span { text: text.to_string(), style, link }),
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph | Tag::HtmlBlock => self.open_inline(InlineKind::Para),
                Tag::Heading { level, .. } => self.open_inline(InlineKind::Heading(level as u8)),
                Tag::BlockQuote(_) => self.open_frame(Frame::Quote(vec![])),
                Tag::CodeBlock(kind) => {
                    self.close_inline();
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((lang, String::new()));
                }
                Tag::List(start) => self.open_frame(Frame::List(start, vec![])),
                Tag::Item => self.open_frame(Frame::Item(None, vec![])),
                Tag::Table(_) => self.open_frame(Frame::Table { head: vec![], rows: vec![], row: vec![] }),
                Tag::TableHead | Tag::TableRow => {
                    if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
                        row.clear();
                    }
                }
                Tag::TableCell => self.open_inline(InlineKind::Cell),
                Tag::Emphasis => self.em += 1,
                Tag::Strong => self.strong += 1,
                Tag::Strikethrough => self.strike += 1,
                Tag::Link { dest_url, title, .. } => {
                    self.links.push(dest_url.to_string());
                    self.cites.push(&*title == "cite");
                }
                Tag::Image { dest_url, .. } => {
                    self.links.push(dest_url.to_string());
                    self.cites.push(false);
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::HtmlBlock | TagEnd::TableCell => self.close_inline(),
                TagEnd::BlockQuote(_) => {
                    self.close_inline();
                    if let Some(Frame::Quote(bs)) = self.stack.pop() {
                        self.push_block(Block::Quote(bs));
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = self.code.take() {
                        self.push_block(Block::Code { lang, text });
                    }
                }
                TagEnd::List(_) => {
                    self.close_inline();
                    if let Some(Frame::List(start, items)) = self.stack.pop() {
                        self.push_block(Block::List { start, items });
                    }
                }
                TagEnd::Item => {
                    self.close_inline();
                    if let Some(Frame::Item(task, blocks)) = self.stack.pop()
                        && let Some(Frame::List(_, items)) = self.stack.last_mut()
                    {
                        items.push(Item { task, blocks });
                    }
                }
                TagEnd::TableHead => {
                    if let Some(Frame::Table { head, row, .. }) = self.stack.last_mut() {
                        *head = std::mem::take(row);
                    }
                }
                TagEnd::TableRow => {
                    if let Some(Frame::Table { rows, row, .. }) = self.stack.last_mut() {
                        rows.push(std::mem::take(row));
                    }
                }
                TagEnd::Table => {
                    if let Some(Frame::Table { head, rows, .. }) = self.stack.pop() {
                        self.push_block(Block::Table { head, rows });
                    }
                }
                TagEnd::Emphasis => self.em = self.em.saturating_sub(1),
                TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
                TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
                TagEnd::Link | TagEnd::Image => {
                    self.links.pop();
                    self.cites.pop();
                }
                _ => {}
            },
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => self.text(&t, false),
            Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, true),
            Event::FootnoteReference(t) => self.text(&format!("[{t}]"), false),
            Event::SoftBreak => self.text(" ", false),
            Event::HardBreak => self.text("\n", false),
            Event::Rule => {
                self.close_inline();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(task, _)) = self.stack.iter_mut().rev().find(|f| matches!(f, Frame::Item(..))) {
                    *task = Some(done);
                }
            }
        }
    }
}

#[cfg(test)]
pub fn parse(text: &str) -> Vec<Block> {
    parse_at(text).0
}

/// The blocks of `text`, and where in it the line starts that the
/// top-level element each one is (or is in) starts on (an indented code
/// block's element starts after its indent).
fn parse_at(text: &str) -> (Vec<Block>, Vec<usize>) {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder { stack: vec![Frame::Blocks(vec![])], ..Default::default() };
    let root = |b: &Builder| match b.stack.first() {
        Some(Frame::Blocks(bs)) => bs.len(),
        _ => 0,
    };
    let (mut offsets, mut depth, mut start) = (vec![], 0usize, 0);
    for (ev, range) in Parser::new_ext(text, opts).into_offset_iter() {
        let line = |at: usize| text[..at].rfind('\n').map_or(0, |i| i + 1);
        match ev {
            Event::Start(_) => {
                if depth == 0 {
                    start = line(range.start);
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ if depth == 0 => start = line(range.start),
            _ => {}
        }
        b.event(ev);
        offsets.resize(root(&b), start);
    }
    b.close_inline();
    if let Some((lang, text)) = b.code.take() {
        b.push_block(Block::Code { lang, text });
    }
    offsets.resize(root(&b), start);
    // pulldown-cmark closes everything it opens, so only the root is left.
    match b.stack.into_iter().next() {
        Some(Frame::Blocks(bs)) => (bs, offsets),
        _ => (vec![], vec![]),
    }
}

/// A text's blocks, and each one's signature (`crate::hash_of`), which
/// with the width it was drawn at keys its height (`Measured`).
struct Parsed {
    blocks: Vec<Block>,
    sigs: Vec<u64>,
    /// Where each block's top-level element starts in the text (`parse_at`).
    offsets: Vec<usize>,
}

impl Parsed {
    fn new(text: &str) -> Parsed {
        let (blocks, offsets) = parse_at(text);
        let sigs = blocks.iter().map(crate::hash_of).collect();
        Parsed { blocks, sigs, offsets }
    }

    /// `text`, which is the text `self` was parsed from with more after it
    /// (a reply streaming in), parsed from its last top-level elements on:
    /// only its last line can still change, and that line, once whole, can
    /// at most join the element before it (`|` after a table becomes a row
    /// of it as `| n`, `1` after a list an item of it as `10. ten`). The
    /// elements before those two were each ended by a whole line of another
    /// element, which nothing after can undo, so their blocks are kept. Not
    /// when the text has a link reference definition (`[x]: url`), which
    /// links text before it too.
    fn extended(&self, text: &str) -> Option<Parsed> {
        let mut starts = self.offsets.iter().rev().copied();
        let last = starts.next()?;
        let off = starts.find(|&o| o < last)?;
        if off == 0 || text.contains("]:") {
            return None;
        }
        let k = self.offsets.partition_point(|&o| o < off);
        let (rest, at) = parse_at(&text[off..]);
        let mut p = Parsed { blocks: Vec::with_capacity(k + rest.len()), sigs: Vec::with_capacity(k + rest.len()), offsets: Vec::with_capacity(k + rest.len()) };
        p.blocks.extend_from_slice(&self.blocks[..k]);
        p.sigs.extend_from_slice(&self.sigs[..k]);
        p.offsets.extend_from_slice(&self.offsets[..k]);
        p.sigs.extend(rest.iter().map(crate::hash_of));
        p.blocks.extend(rest);
        p.offsets.extend(at.into_iter().map(|o| o + off));
        Some(p)
    }
}

impl std::ops::Deref for Parsed {
    type Target = [Block];
    fn deref(&self) -> &[Block] {
        &self.blocks
    }
}

/// Parsed replies, so a long transcript isn't re-parsed every frame. Two
/// generations, each up to `CACHE_TEXT` bytes of text: a reply drawn lately
/// is in one of them, while the prefixes a streaming reply leaves behind
/// (one per frame) age out instead of piling up.
fn parse_cached(text: &str) -> Rc<Parsed> {
    const CACHE_TEXT: usize = 1 << 20;
    #[derive(Default)]
    struct Cache {
        hot: HashMap<u64, Rc<Parsed>>,
        hot_bytes: usize,
        cold: HashMap<u64, Rc<Parsed>>,
        /// The text parsed last and its blocks, which the next text, when
        /// it goes on from it, starts from (`Parsed::extended`).
        last: Option<(String, Rc<Parsed>)>,
    }
    thread_local! {
        static CACHE: RefCell<Cache> = RefCell::default();
    }
    let key = crate::hash_of(text);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(b) = c.hot.get(&key) {
            return b.clone();
        }
        let b = match c.cold.remove(&key) {
            Some(b) => b,
            None => {
                let from = c.last.as_ref().filter(|(prev, _)| text.len() > prev.len() && text.starts_with(prev.as_str()));
                let b = Rc::new(from.and_then(|(_, p)| p.extended(text)).unwrap_or_else(|| Parsed::new(text)));
                c.last = Some((text.to_string(), b.clone()));
                b
            }
        };
        if c.hot_bytes + text.len() > CACHE_TEXT {
            c.cold = std::mem::take(&mut c.hot);
            c.hot_bytes = 0;
        }
        c.hot_bytes += text.len();
        c.hot.insert(key, b.clone());
        b
    })
}

// ---------------------------------------------------------------- fonts

const BOLD_FAMILY: &str = "bold";
const ITALIC_FAMILY: &str = "italic";
const BOLD_ITALIC_FAMILY: &str = "bold-italic";

/// The id of the built-in face, IBM Plex Sans (the setting's default).
pub const PLEX: &str = "plex";

/// The faces Settings offers: IBM Plex Sans, then the plugins', as (id, name).
pub fn faces() -> Vec<(String, String)> {
    let mut list = vec![(PLEX.to_string(), "IBM Plex Sans".to_string())];
    list.extend(crate::plugins::parts().fonts.iter().map(|f| (f.id(), f.name())).filter(|(id, _)| id != PLEX));
    list
}

/// The plugin font with this id, when its plugin is loaded.
pub fn plugin_face(id: &str) -> Option<&'static std::sync::Arc<crate::plugins::FontDef>> {
    crate::plugins::parts().fonts.iter().find(|f| id != PLEX && f.id() == id)
}

/// The fonts, with IBM Plex Sans for text. See `install_face`.
#[cfg(test)]
pub fn install_fonts(ctx: &egui::Context) {
    install_face(ctx, PLEX);
}

/// The fonts: the face `face` names (with real bold and italics) for
/// everything, egui's monospace font (Hack) only for code: the `CODE`
/// family, and the Monospace text style. Nerd Font symbols (built in) are
/// the `ICONS` family, and a fallback for the others. False when `face` is
/// a plugin's whose files aren't on this machine yet (`FontDef::fetch`
/// gets them): Plex stands in, as it does for an unknown face.
pub fn install_face(ctx: &egui::Context, face: &str) -> bool {
    let mut defs = FontDefinitions::default();
    let plex: [&'static [u8]; 4] = [
        include_bytes!("../assets/IBMPlexSans-Regular.ttf"),
        include_bytes!("../assets/IBMPlexSans-Bold.ttf"),
        include_bytes!("../assets/IBMPlexSans-Italic.ttf"),
        include_bytes!("../assets/IBMPlexSans-BoldItalic.ttf"),
    ];
    let font = plugin_face(face);
    // A plugin's four faces, each a font egui can read.
    let files = font.and_then(|f| f.faces()).filter(|f| f.len() == 4 && f.iter().all(|b| b.starts_with(b"OTTO") || b.starts_with(&[0, 1, 0, 0]) || b.starts_with(b"true")));
    let (prefix, faces, ok): (&str, Vec<FontData>, bool) = match files {
        Some(files) => ("face", files.into_iter().map(FontData::from_owned).collect(), true),
        None => ("plex", plex.into_iter().map(FontData::from_static).collect(), font.is_none()),
    };
    let names = ["", "-bold", "-italic", "-bold-italic"].map(|s| format!("{prefix}{s}"));
    for (name, data) in names.iter().zip(faces) {
        defs.font_data.insert(name.clone(), Arc::new(data));
    }
    defs.font_data.insert("nerd-symbols".into(), Arc::new(FontData::from_static(include_bytes!("../assets/SymbolsNerdFontMono-Regular.ttf"))));
    // Egui's own fonts after these, for emoji and other symbols.
    let fallback = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let code = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let family = |first: &str| [vec![first.to_string(), "nerd-symbols".to_string()], fallback.clone()].concat();
    defs.families.insert(FontFamily::Proportional, family(&names[0]));
    // The Monospace family is the text face too, so nothing but code is monospace.
    defs.families.insert(FontFamily::Monospace, family(&names[0]));
    defs.families.insert(FontFamily::Name(BOLD_FAMILY.into()), family(&names[1]));
    defs.families.insert(FontFamily::Name(ITALIC_FAMILY.into()), family(&names[2]));
    defs.families.insert(FontFamily::Name(BOLD_ITALIC_FAMILY.into()), family(&names[3]));
    defs.families.insert(FontFamily::Name(CODE.into()), [code, vec!["nerd-symbols".to_string()]].concat());
    defs.families.insert(FontFamily::Name(ICONS.into()), vec!["nerd-symbols".to_string()]);
    ctx.set_fonts(defs);
    let set_in = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let f = d.get_temp_mut_or_default::<Fonts>(fonts_key());
        f.generation += 1;
        f.set_in = set_in;
    });
    ok
}

// ---------------------------------------------------------------- drawing

const BODY: f32 = 13.5;

struct Ctx<'a> {
    t: &'a Theme,
    id: Id,
    n: usize,
    /// Code blocks are colored for their fence's language.
    highlighted: bool,
}

impl Ctx<'_> {
    fn next_id(&mut self) -> Id {
        self.n += 1;
        self.id.with(self.n)
    }
}

/// Draws markdown text. `id` keeps code blocks' and tables' scroll state apart.
pub fn show(ui: &mut Ui, t: &Theme, text: &str, id: Id) {
    show_as(ui, t, text, id, false);
}

/// Draws markdown text, its code blocks colored for their language when
/// the fence names one (`Duck`'s replies, `duck_view::duck_transcript`).
pub fn show_highlighted(ui: &mut Ui, t: &Theme, text: &str, id: Id) {
    show_as(ui, t, text, id, true);
}

fn show_as(ui: &mut Ui, t: &Theme, text: &str, id: Id, highlighted: bool) {
    let parsed = parse_cached(text);
    let mut cx = Ctx { t, id, n: 0, highlighted };
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        if parsed.len() >= LONG {
            draw_long(ui, &mut cx, &parsed);
        } else {
            draw_blocks(ui, &mut cx, &parsed, 0, t.ink);
        }
    });
}

/// From this many blocks, a text's blocks far off screen are left out
/// (`draw_long`).
const LONG: usize = 8;

/// How many times the fonts were set on a context, and in which pass the
/// last time: the heights `Measured` keeps are for the fonts they were
/// drawn in, and fonts set during a pass are used from the next one.
#[derive(Clone, Copy, Default)]
struct Fonts {
    generation: u64,
    set_in: u64,
}

fn fonts_key() -> Id {
    Id::unique("markdown-fonts")
}

/// A block's height and right edge (from the text's left) as last drawn,
/// by the text's id, the block's place and signature, the width, the zoom
/// and the fonts (`draw_long`); and its ink as last sketched
/// (`vlist::capture`), relative to its top left, with the colors it was
/// drawn in. Each is settled once two draws in a row agree: a widget's
/// first draw can be a sizing pass that paints nothing (a table's grid).
#[derive(Clone, Default)]
struct Measured {
    h: f32,
    right: f32,
    settled: bool,
    ink: Option<(u64, Rc<[crate::vlist::Ink]>)>,
    ink_settled: bool,
}

/// Whether two sketches are the same (to a hundredth of a point).
fn same_ink(a: &[crate::vlist::Ink], b: &[crate::vlist::Ink]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.color == b.color && (a.rect.min - b.rect.min).length() < 0.01 && (a.rect.max - b.rect.max).length() < 0.01)
}

thread_local! {
    static MEASURED: RefCell<HashMap<u64, Measured>> = RefCell::default();
}

#[cfg(test)]
thread_local! {
    /// How many blocks `draw_long` drew on this thread.
    static DRAWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A long text (a long reply, often still streaming in, is one unit of the
/// transcript, so `vlist` lays it out whole): its blocks more than a view
/// away from the clip rect, as last drawn at this width, are only the space
/// they take, which is most of the frame's work saved. Off screen, where
/// `vlist` measures and sketches units (the clip is the whole unit), a
/// block sketched before in these colors is only its ink and its space.
/// Each block is drawn with ids of its own (`Ui` scope and `Ctx::id`), so
/// leaving others out moves no widget's id. Every block is drawn while
/// text is selected (a selection whose ends aren't both drawn is dropped,
/// and a message selected whole is lit as drawn) or find is marking
/// matches (it counts them in drawing order), and the last block always is.
fn draw_long(ui: &mut Ui, cx: &mut Ctx, parsed: &Parsed) {
    let ctx = ui.ctx().clone();
    let clip = ui.clip_rect();
    let near = egui::Rangef::new(clip.top() - clip.height(), clip.bottom() + clip.height());
    let fonts = ctx.data(|d| d.get_temp::<Fonts>(fonts_key())).unwrap_or_default();
    // (In the pass the fonts were set in, what is drawn may be in the old ones.)
    let keep = ctx.cumulative_pass_nr() > fonts.set_in;
    let leave_out = keep && !crate::find::active() && !ctx.plugin::<egui::text_selection::LabelSelectionState>().lock().has_selection() && !crate::select::any_selected(&ctx);
    let sketching = crate::vlist::sketching();
    let (width, ppp, left) = (ui.available_width(), ctx.pixels_per_point(), ui.max_rect().left());
    let fonts = fonts.generation;
    let t = cx.t;
    let colors = crate::hash_of(&(t.bg, t.panel, t.panel2, t.ink, t.ink2, t.ink3, t.accent, t.line, ui.visuals().dark_mode, ui.visuals().selection.bg_fill));
    let gap = ui.spacing().item_spacing.y;
    let layer = ui.layer_id();
    let base = cx.id;
    let n = parsed.len();
    // The blocks left out since the last one drawn: their height together, and right edge.
    let mut skipped: Option<(f32, f32)> = None;
    let fill = |ui: &mut Ui, skipped: &mut Option<(f32, f32)>| {
        if let Some((h, right)) = skipped.take() {
            ui.allocate_exact_size(vec2(right, h), egui::Sense::hover());
        }
    };
    for (i, b) in parsed.iter().enumerate() {
        let key = crate::hash_of(&(base, i, parsed.sigs[i], width.to_bits(), ppp.to_bits(), fonts, cx.highlighted));
        let top = ui.cursor().top() + skipped.map_or(0.0, |(h, _)| h + gap);
        let kept = if leave_out && i + 1 < n { MEASURED.with(|c| c.borrow().get(&key).filter(|m| m.settled).cloned()) } else { None };
        if let Some(m) = kept {
            let ink = m.ink.as_ref().filter(|(c, _)| m.ink_settled && *c == colors).map(|(_, ink)| ink);
            let skip = match (sketching, ink) {
                (true, Some(ink)) => {
                    let o = vec2(left, top);
                    let p = ui.painter();
                    for k in ink.iter() {
                        p.add(egui::Shape::rect_filled(k.rect.translate(o), 0.0, k.color));
                    }
                    true
                }
                (true, None) => false,
                (false, _) => top + m.h < near.min || top > near.max,
            };
            if skip {
                skipped = Some(match skipped {
                    None => (m.h, m.right),
                    Some((h, right)) => (h + gap + m.h, right.max(m.right)),
                });
                continue;
            }
        }
        fill(ui, &mut skipped);
        #[cfg(test)]
        DRAWN.set(DRAWN.get() + 1);
        let id = base.with(("block", i));
        let start = crate::vlist::paint_len(&ctx, layer);
        let r = ui
            .scope_builder(egui::UiBuilder::new().scope_id(id), |ui| {
                cx.id = id;
                cx.n = 0;
                draw_block(ui, cx, b, i, 0, t.ink);
            })
            .response
            .rect;
        // (Nor from a pass egui is to run again, as a table first laid out
        // asks: its sizes aren't settled.)
        if !keep || ctx.will_discard() {
            continue;
        }
        let ink = sketching.then(|| {
            let end = crate::vlist::paint_len(&ctx, layer);
            let all = egui::Rangef::new(f32::NEG_INFINITY, f32::INFINITY);
            crate::vlist::capture(&ctx, layer, (start, end), egui::pos2(left, r.top()), all)
        });
        MEASURED.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= 50_000 {
                c.clear();
            }
            let m = c.entry(key).or_default();
            let (h, right) = (r.height(), r.right() - left);
            m.settled = m.h == h && m.right == right;
            (m.h, m.right) = (h, right);
            if let Some(ink) = ink {
                m.ink_settled = m.ink.as_ref().is_some_and(|(c, old)| *c == colors && same_ink(old, &ink));
                m.ink = Some((colors, ink.into()));
            }
        });
    }
    fill(ui, &mut skipped);
    cx.id = base;
}

fn draw_blocks(ui: &mut Ui, cx: &mut Ctx, blocks: &[Block], depth: usize, color: Color32) {
    for (i, b) in blocks.iter().enumerate() {
        draw_block(ui, cx, b, i, depth, color);
    }
}

/// Block `b`, the `i`th of its blocks.
fn draw_block(ui: &mut Ui, cx: &mut Ctx, b: &Block, i: usize, depth: usize, color: Color32) {
    match b {
        Block::Para(spans) => {
            if i > 0 {
                ui.add_space(2.0);
            }
            inline(ui, cx.t, spans, BODY, color, false, ui.available_width());
        }
        Block::Heading(level, spans) => {
            if i > 0 {
                ui.add_space(if *level <= 2 { 10.0 } else { 6.0 });
            }
            let size = match level {
                1 => 19.0,
                2 => 16.5,
                3 => 14.5,
                _ => BODY,
            };
            inline(ui, cx.t, spans, size, cx.t.ink, true, ui.available_width());
            if *level == 1 {
                ui.separator();
            }
        }
        Block::Code { lang, text } => code(ui, cx, lang, text),
        Block::List { start, items } => list(ui, cx, *start, items, depth, color),
        Block::Quote(inner) => {
            let r = egui::Frame::new()
                .inner_margin(Margin { left: 14, right: 0, top: 2, bottom: 2 })
                .show(ui, |ui| draw_blocks(ui, cx, inner, depth, cx.t.ink2))
                .response
                .rect;
            ui.painter().vline(r.left() + 1.0, r.y_range(), Stroke::new(2.0, cx.t.line));
        }
        Block::Rule => {
            ui.separator();
        }
        Block::Table { head, rows } => table(ui, cx, head, rows),
    }
}

/// Web addresses written out in plain text (not code) become links.
fn autolink(spans: Vec<Span>) -> Vec<Span> {
    let mut out = Vec::with_capacity(spans.len());
    for s in spans {
        if s.link.is_some() || s.style.code || !s.text.contains("://") {
            out.push(s);
            continue;
        }
        let mut rest = s.text.as_str();
        while let Some(start) = [rest.find("https://"), rest.find("http://")].into_iter().flatten().min() {
            let tail = &rest[start..];
            let mut end = tail.find(|c: char| c.is_whitespace() || "<>\"`".contains(c)).unwrap_or(tail.len());
            // Not the sentence's punctuation, nor a closing bracket it
            // didn't open.
            loop {
                let url = &tail[..end];
                let Some(last) = url.chars().last() else { break };
                let unopened = |open, close| last == close && url.matches(open).count() < url.matches(close).count();
                if ".,;:!?'*".contains(last) || unopened('(', ')') || unopened('[', ']') {
                    end -= last.len_utf8();
                } else {
                    break;
                }
            }
            if end <= "https://".len() {
                out.push(Span { text: rest[..start + end.max(1)].to_string(), ..s.clone() });
                rest = &rest[start + end.max(1)..];
                continue;
            }
            if start > 0 {
                out.push(Span { text: rest[..start].to_string(), ..s.clone() });
            }
            let url = &tail[..end];
            out.push(Span { text: url.to_string(), link: Some(url.to_string()), ..s.clone() });
            rest = &tail[end..];
        }
        if !rest.is_empty() {
            out.push(Span { text: rest.to_string(), ..s });
        }
    }
    out
}

fn format(t: &Theme, style: Style, link: bool, size: f32, color: Color32, heading: bool) -> TextFormat {
    let bold = style.strong || heading;
    let family = match (bold, style.em) {
        (true, true) => FontFamily::Name(BOLD_ITALIC_FAMILY.into()),
        (true, false) => FontFamily::Name(BOLD_FAMILY.into()),
        (false, true) => FontFamily::Name(ITALIC_FAMILY.into()),
        (false, false) => FontFamily::Proportional,
    };
    let mut f = TextFormat { font_id: FontId::new(size, family), color, ..Default::default() };
    if style.code {
        f.font_id = code_font(size * 0.96);
        f.background = t.panel2;
        if !heading {
            f.color = t.ink;
        }
    }
    if style.strike {
        f.strikethrough = Stroke::new(1.0, f.color);
    }
    if link && style.cite {
        f.font_id.size = size * 0.8;
        f.color = t.ink2;
        f.background = t.panel2;
    } else if link {
        f.color = t.accent;
        f.underline = Stroke::new(1.0, t.accent.gamma_multiply(0.6));
    }
    f
}

/// A run of styled text as one wrapped label; links open on click.
fn inline(ui: &mut Ui, t: &Theme, spans: &[Span], size: f32, color: Color32, heading: bool, width: f32) {
    let mut job = LayoutJob::default();
    let mut links: Vec<(usize, usize, &str)> = vec![];
    let mut chars = 0;
    for s in spans {
        let n = s.text.chars().count();
        if let Some(url) = &s.link {
            links.push((chars, chars + n, url));
        }
        job.append(&s.text, 0.0, format(t, s.style, s.link.is_some(), size, color, heading));
        chars += n;
    }
    job.wrap.max_width = width;
    let (resp, galley) = marked_label(ui, job);
    if links.is_empty() {
        return;
    }
    let link_at = |pos: egui::Pos2| {
        let rel = pos - resp.rect.min;
        // Only over the glyphs, not past the end of a short line.
        let on_text = galley.rows.iter().any(|r| r.rect().contains(rel.to_pos2()));
        // The glyph under the pointer: the cursor nearest it is after the
        // glyph when the pointer is over its right half.
        let at = galley.cursor_from_pos(rel);
        let caret_x = galley.pos_from_cursor(at).min.x;
        let at = if rel.x < caret_x { at.index.0.saturating_sub(1) } else { at.index.0 };
        links.iter().find(|(a, b, _)| on_text && at >= *a && at < *b).map(|(_, _, url)| *url)
    };
    // A right-click's menu here offers to copy the link's address.
    if let Some(p) = crate::copy_menu::opening(ui.ctx())
        && resp.rect.contains(p)
        && let Some(url) = link_at(p)
    {
        crate::copy_menu::note_link(ui.ctx(), link_target(url, link_base(ui.ctx()).as_deref()));
    }
    let Some(pos) = resp.hover_pos() else { return };
    if let Some(url) = link_at(pos) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        let target = link_target(url, link_base(ui.ctx()).as_deref());
        if resp.clicked() {
            open_link(&target);
        }
        resp.on_hover_text(target);
    }
}

/// The folder relative file links resolve against while a conversation's
/// transcript is drawn (its working folder).
pub fn set_link_base(ctx: &egui::Context, folder: Option<&std::path::Path>) {
    let id = Id::unique("markdown-link-base");
    ctx.data_mut(|d| match folder {
        Some(f) => {
            d.insert_temp(id, f.to_path_buf());
        }
        None => d.remove::<std::path::PathBuf>(id),
    });
}

fn link_base(ctx: &egui::Context) -> Option<std::path::PathBuf> {
    ctx.data(|d| d.get_temp(Id::unique("markdown-link-base")))
}

/// Where a link leads: a web address (or `mailto:`) as it is, for the
/// default browser; else a file: `file://` taken off, %-escapes decoded,
/// a line (`#L12`, `:12`, `:12:5`) dropped, and a relative path resolved
/// against `base`.
pub(crate) fn link_target(url: &str, base: Option<&std::path::Path>) -> String {
    let url = url.trim().trim_start_matches('<').trim_end_matches('>');
    let scheme = url.split_once(':').map(|(s, _)| s.to_ascii_lowercase());
    let file = match scheme.as_deref() {
        Some("file") => url[5..].trim_start_matches('/'),
        // A drive letter is no scheme.
        Some(s) if s.len() > 1 && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => return url.to_string(),
        _ => url,
    };
    // (A `file:///C:/x` is `C:/x`; a `file:///home/x` is `/home/x`.)
    let file = if scheme.as_deref() == Some("file") && !(file.len() > 1 && file.as_bytes()[1] == b':') { format!("/{file}") } else { file.to_string() };
    let mut path = decode_percent(&file);
    // A line in it: `#L12`, `#L12-L20`, `:12`, `:12:5`.
    if let Some(i) = path.rfind("#L")
        && path[i + 2..].chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'L')
    {
        path.truncate(i);
    }
    for _ in 0..2 {
        if let Some(i) = path.rfind(':')
            && i > 1
            && !path[i + 1..].is_empty()
            && path[i + 1..].chars().all(|c| c.is_ascii_digit())
        {
            path.truncate(i);
        }
    }
    let p = std::path::Path::new(&path);
    // (A drive letter makes a path absolute on every platform.)
    let drive = path.len() > 1 && path.as_bytes()[1] == b':' && path.as_bytes()[0].is_ascii_alphabetic();
    match base {
        Some(base) if p.is_relative() && !drive => base.join(p).display().to_string(),
        _ => path,
    }
}

/// `%XX` escapes decoded (a bad one stays as it is).
fn decode_percent(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Opens a link's target with the system: a web address in the default
/// browser, a file in its default app. (Under test, only noted.)
fn open_link(target: &str) {
    #[cfg(test)]
    OPENED.lock().unwrap().push(target.to_string());
    #[cfg(not(test))]
    {
        let target = target.to_string();
        // (Off the UI thread: the shell can take a while.)
        std::thread::spawn(move || {
            let _ = open::that(&target);
        });
    }
}

#[cfg(test)]
static OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(vec![]);

fn code(ui: &mut Ui, cx: &mut Ctx, lang: &str, text: &str) {
    let t = cx.t;
    let id = cx.next_id();
    let body = text.trim_end();
    crate::widgets::inset(ui, t, t.panel, 10, |ui| {
        // The language and the copy button aren't the code: not selected
        // with it, nor copied.
        let head = ui.painter().add(egui::Shape::Noop);
        ui.horizontal(|ui| {
            ui.add(Label::new(RichText::new(if lang.is_empty() { "code" } else { lang }).small().color(t.ink3)).selectable(false));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let copy = ui.add(egui::Button::new(RichText::new("copy").small().color(t.ink3)).frame(false));
                if copy.clicked() {
                    ui.ctx().copy_text(body.to_string());
                }
                copy.on_hover_text("Copy to clipboard");
            });
        });
        crate::select::not_text(ui, head);
        // Code keeps its lines: it scrolls sideways instead of wrapping.
        ScrollArea::horizontal().id_salt(id).auto_shrink([false, true]).show(ui, |ui| {
            let syntax = cx.highlighted.then(|| highlight::for_fence(lang)).flatten();
            let Some(syntax) = syntax else {
                if crate::find::active() {
                    // Matches marked (Ctrl+F).
                    let mut job = LayoutJob::single_section(body.to_string(), TextFormat { font_id: FontId::monospace(12.5), color: t.ink, ..Default::default() });
                    job.wrap.max_width = f32::INFINITY;
                    marked_label(ui, job);
                } else {
                    ui.add(Label::new(RichText::new(body).monospace().size(12.5).color(t.ink)).extend());
                }
                return;
            };
            let font = code_font(12.5);
            let pal = highlight::Palette::for_theme(t);
            // Plain, unwrapped lines of colored runs; only an empty block
            // needs a label to hold its place.
            let (mut job, _) = highlight::job(&[(None, body)], Some(&syntax), &font, &pal);
            if job.text.is_empty() {
                ui.add(Label::new(RichText::new(body).font(font).color(t.ink)).extend());
                return;
            }
            job.wrap.max_width = f32::INFINITY;
            if crate::find::active() {
                marked_label(ui, job);
            } else {
                ui.add(Label::new(job).selectable(true));
            }
        });
    });
}

/// A label of `job` with the matches of find (Ctrl+F) marked, and its
/// galley.
pub(crate) fn marked_label(ui: &mut Ui, mut job: LayoutJob) -> (egui::Response, Arc<egui::Galley>) {
    let current = crate::find::mark(&mut job);
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let resp = ui.add(Label::new(galley.clone()));
    if let Some(at) = current {
        crate::find::found(&galley, resp.rect.min, at);
    }
    (resp, galley)
}

/// The text of a run of spans, unstyled.
fn spans_text(s: &[Span]) -> String {
    s.iter().map(|s| s.text.as_str()).collect()
}

/// The number (from `start`) or bullet (by `depth`) of a list's item `i`.
fn list_marker(start: Option<u64>, i: u64, depth: usize) -> String {
    match start {
        Some(s) => format!("{}.", s + i),
        None => (if depth.is_multiple_of(2) { "•" } else { "–" }).to_string(),
    }
}

/// The texts markdown `text` is drawn as, in the order they are drawn
/// (each paragraph, heading, code block and table cell), for find.
pub(crate) fn plain_parts(text: &str) -> Vec<String> {
    fn walk(blocks: &[Block], out: &mut Vec<String>) {
        for b in blocks {
            match b {
                Block::Para(s) | Block::Heading(_, s) => out.push(spans_text(s)),
                Block::Code { text, .. } => out.push(text.trim_end().to_string()),
                Block::List { items, .. } => {
                    for i in items {
                        walk(&i.blocks, out);
                    }
                }
                Block::Quote(inner) => walk(inner, out),
                Block::Rule => {}
                Block::Table { head, rows } => {
                    out.extend(head.iter().map(|c| spans_text(c)));
                    out.extend(rows.iter().flatten().map(|c| spans_text(c)));
                }
            }
        }
    }
    let mut out = vec![];
    walk(&parse_cached(text), &mut out);
    out
}

/// Markdown `text` as plain text, the way it reads on screen: what copying
/// a whole message copies. Blocks apart by a blank line, list items with
/// their numbers or bullets (nested ones indented), code without its fence
/// and language, table cells apart by tabs.
pub(crate) fn plain_text(text: &str) -> String {
    fn blocks(bs: &[Block], depth: usize, sep: &str) -> String {
        bs.iter().map(|b| block(b, depth)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(sep)
    }
    fn block(b: &Block, depth: usize) -> String {
        match b {
            Block::Para(s) | Block::Heading(_, s) => spans_text(s),
            Block::Code { text, .. } => text.trim_end().to_string(),
            Block::List { start, items } => items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let mut marker = list_marker(*start, i as u64, depth);
                    if let Some(done) = item.task {
                        marker.push_str(if done { " [x]" } else { " [ ]" });
                    }
                    let indent = " ".repeat(marker.chars().count() + 1);
                    let body = blocks(&item.blocks, depth + 1, "\n");
                    let body = body.lines().collect::<Vec<_>>().join(&format!("\n{indent}"));
                    format!("{marker} {body}").trim_end().to_string()
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Quote(inner) => blocks(inner, depth, "\n\n"),
            Block::Rule => String::new(),
            Block::Table { head, rows } => std::iter::once(head)
                .chain(rows)
                .map(|row| row.iter().map(|c| spans_text(c)).collect::<Vec<_>>().join("\t"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
    blocks(&parse_cached(text), 0, "\n\n")
}

fn list(ui: &mut Ui, cx: &mut Ctx, start: Option<u64>, items: &[Item], depth: usize, color: Color32) {
    let t = cx.t;
    let markers: Vec<String> = (0..items.len() as u64).map(|i| list_marker(start, i, depth)).collect();
    let width = markers.iter().map(|m| m.chars().count()).max().unwrap_or(1) as f32 * 8.2 + 8.0;
    for (item, marker) in items.iter().zip(markers) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            // The number or bullet is a label of its own, so a selection
            // over the list copies it with the item's text.
            let marker_color = if start.is_some() { t.ink2 } else { t.accent };
            ui.allocate_ui_with_layout(vec2(width, BODY + 4.0), Layout::left_to_right(Align::Min), |ui| {
                ui.set_min_size(vec2(width, BODY + 4.0));
                ui.add(Label::new(RichText::new(&marker).font(FontId::proportional(BODY)).color(marker_color)).extend());
            });
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.x = 8.0;
                if let Some(done) = item.task {
                    ui.label(RichText::new(if done { "[x]" } else { "[ ]" }).color(if done { t.accent } else { t.ink3 }));
                }
                draw_blocks(ui, cx, &item.blocks, depth + 1, if item.task == Some(true) { t.ink2 } else { color });
            });
        });
    }
}

fn table(ui: &mut Ui, cx: &mut Ctx, head: &[Vec<Span>], rows: &[Vec<Vec<Span>>]) {
    let t = cx.t;
    let id = cx.next_id();
    ScrollArea::horizontal().id_salt(id).auto_shrink([false, true]).show(ui, |ui| {
        egui::Grid::new(id.with("grid")).striped(true).spacing(vec2(18.0, 6.0)).show(ui, |ui| {
            for cell in head {
                inline(ui, t, cell, BODY, t.ink, true, 360.0);
            }
            ui.end_row();
            for row in rows {
                for cell in row {
                    inline(ui, t, cell, BODY, t.ink2, false, 360.0);
                }
                ui.end_row();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{click, run_frame, test_ctx};
    use eframe::egui::Rect;

    /// IBM Plex Sans for text (with its own bold and italics); only code
    /// gets the code font.
    #[test]
    fn plex_for_text_code_for_code() {
        let t = crate::app_hover_test::warm_theme();
        let family = |style: Style| format(&t, style, false, 13.5, t.ink, false).font_id.family;
        assert_eq!(family(Style::default()), FontFamily::Proportional);
        assert_eq!(family(Style { strong: true, ..Default::default() }), FontFamily::Name(BOLD_FAMILY.into()));
        assert_eq!(family(Style { em: true, ..Default::default() }), FontFamily::Name(ITALIC_FAMILY.into()));
        assert_eq!(family(Style { code: true, strong: true, ..Default::default() }), FontFamily::Name(CODE.into()));

        let (ctx, _) = test_ctx(false, false);
        run_frame(&ctx, vec2(600.0, 300.0), &mut 0.0, 1.0 / 60.0, vec![], |ui| {
            let fonts = ui.ctx().fonts(|f| f.definitions().clone());
            assert_eq!(fonts.families[&FontFamily::Proportional][0], "plex");
            assert_eq!(fonts.families[&FontFamily::Monospace][0], "plex", "nothing but code is monospace");
            assert_eq!(fonts.families[&FontFamily::Name(BOLD_FAMILY.into())][0], "plex-bold");
            assert_ne!(fonts.families[&FontFamily::Name(CODE.into())][0], "plex");
            // Plex is proportional; the code font isn't.
            let w = |c, f: FontId| ui.fonts_mut(|x| x.glyph_width(&f, c));
            assert!(w('i', FontId::proportional(13.0)) < w('M', FontId::proportional(13.0)) * 0.5);
            assert_eq!(w('i', code_font(13.0)), w('M', code_font(13.0)));
        });
    }

    fn span(text: &str, style: Style) -> Span {
        Span { text: text.into(), style, link: None }
    }

    #[test]
    fn inline_styles() {
        let b = parse("Some **bold**, *em*, `code` and ~~gone~~.");
        let strong = Style { strong: true, ..Default::default() };
        let Block::Para(spans) = &b[0] else { panic!("{b:?}") };
        assert_eq!(spans[0], span("Some ", Style::default()));
        assert_eq!(spans[1], span("bold", strong));
        assert!(spans.iter().any(|s| s.text == "em" && s.style.em));
        assert!(spans.iter().any(|s| s.text == "code" && s.style.code));
        assert!(spans.iter().any(|s| s.text == "gone" && s.style.strike));
    }

    #[test]
    fn headings_lists_and_tasks() {
        let b = parse("## Title\n\n- one **x**\n- two\n  1. nested\n\nBetween.\n\n- [x] done\n- [ ] todo\n");
        assert!(matches!(&b[0], Block::Heading(2, s) if s[0].text == "Title"));
        let Block::List { start: None, items } = &b[1] else { panic!("{b:?}") };
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[1].blocks[1], Block::List { start: Some(1), .. }));
        let Block::List { items: tasks, .. } = &b[3] else { panic!("{b:?}") };
        assert_eq!(tasks[0].task, Some(true));
        assert_eq!(tasks[1].task, Some(false));
        assert!(matches!(&tasks[1].blocks[0], Block::Para(s) if s[0].text == "todo"));
    }

    #[test]
    fn code_links_quotes_tables() {
        let md = "```rust\nfn main() {}\n```\n\n> quoted [site](https://x.dev)\n\n| a | b |\n|---|---|\n| 1 | **2** |\n\n---\n";
        let b = parse(md);
        assert_eq!(b[0], Block::Code { lang: "rust".into(), text: "fn main() {}\n".into() });
        let Block::Quote(q) = &b[1] else { panic!("{b:?}") };
        let Block::Para(spans) = &q[0] else { panic!() };
        assert_eq!(spans[1].link.as_deref(), Some("https://x.dev"));
        let Block::Table { head, rows } = &b[2] else { panic!("{b:?}") };
        assert_eq!(head.len(), 2);
        assert!(rows[0][1][0].style.strong);
        assert_eq!(b[3], Block::Rule);
    }

    /// Links lead to web pages as they are, and to files in the working
    /// folder: `file://`, %-escapes and lines dealt with.
    #[test]
    fn link_targets() {
        let base = std::path::Path::new("/work");
        let t = |u| link_target(u, Some(base));
        assert_eq!(t("https://x.dev/a?b=1"), "https://x.dev/a?b=1");
        assert_eq!(t("mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(t("src/main.rs#L12"), base.join("src/main.rs").display().to_string());
        assert_eq!(t("src/main.rs:12:5"), base.join("src/main.rs").display().to_string());
        assert_eq!(t("my%20notes.md"), base.join("my notes.md").display().to_string());
        assert_eq!(t("file:///C:/Users/a%20b/x.txt"), "C:/Users/a b/x.txt");
        assert_eq!(t("C:/Users/x.txt:3"), "C:/Users/x.txt");
        if cfg!(unix) {
            assert_eq!(t("file:///home/x"), "/home/x");
        }
    }

    /// A text that goes on from the last one parsed is parsed from its last
    /// top-level element on, and comes out as parsing all of it does: at
    /// every point of texts whose later lines change what earlier ones are
    /// (setext headings, lists going on after a blank line, fences, tables,
    /// quotes, HTML), going on by a few bytes at a time.
    #[test]
    fn going_on_parses_as_all_of_it() {
        let docs = [
            long_reply(3),
            "Intro line\nstill the intro\n===\n\nNext para\n---\n\n- a\n- b\n\n  more of b\n\n- c\n\nAfter the list.\n\n1. one\n2. two\n\n10. ten\n".to_string(),
            "Para\n\n```\nunclosed fence\n\n\nstill code\n\n    indented\n```\n\n    indented code\n\n    more\n\nend".to_string(),
            "| a | b |\n|---|---|\n| 1 | 2 |\nlazy row\n\n> quote\nlazy\n\n> > nested\n\nout\n\n<!-- a\n\ncomment -->\n\n<div>\nhtml\n</div>\n\n***\n\n- [ ] task\n- [x] done\n\nText with a ~~strike~~ and `code`\n\n# Heading\ntext".to_string(),
            "See [the docs][d] and more.\n\nLater text.\n\n[d]: https://example.com\n\nAfter.".to_string(),
        ];
        let mut extended = 0;
        for doc in &docs {
            let mut at = 0;
            let mut prev = Parsed::new("");
            let mut step = 1;
            while at < doc.len() {
                at = (at + step).min(doc.len());
                while !doc.is_char_boundary(at) {
                    at += 1;
                }
                step = step % 7 + 1;
                let text = &doc[..at];
                let fresh = Parsed::new(text);
                if let Some(p) = prev.extended(text) {
                    extended += 1;
                    assert_eq!(p.blocks, fresh.blocks, "at {at} of {doc:?}");
                    assert_eq!(p.offsets, fresh.offsets, "at {at} of {doc:?}");
                    assert_eq!(p.sigs, fresh.sigs);
                }
                assert_eq!(parse_cached(text).blocks, fresh.blocks, "at {at} of {doc:?}");
                prev = fresh;
            }
        }
        assert!(extended > 200, "{extended}");
    }

    /// A long reply: headings, paragraphs, lists, code and tables.
    fn long_reply(parts: usize) -> String {
        (0..parts)
            .map(|i| {
                format!(
                    "## Part {i}\n\nThe change **moves** the lexer's lookahead into the *parser*, so a token is read once, which also makes this line wrap when the column is narrow.\n\n\
                     - the first point, about `tokens`\n- the second [point](https://example.com/{i})\n\n\
                     ```rust\nfn parse_{i}(input: &str) -> Ast {{\n    todo!()\n}}\n```\n\n| case | before |\n|---|---|\n| nested | fails |\n\n"
                )
            })
            .collect()
    }

    /// A long text's blocks far from the clip rect, once drawn, only take
    /// their space: the text is as tall and wide as when every block is
    /// drawn, what shows is drawn the same, and much less is drawn.
    #[test]
    fn far_blocks_take_their_space() {
        let md = long_reply(40);
        assert!(parse(&md).len() >= LONG);
        let (ctx, t) = test_ctx(true, false);
        let mut time = 0.0;
        let view = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(500.0, 400.0));
        let mut frame = |full: bool| {
            let mut rect = Rect::NOTHING;
            let out = run_frame(&ctx, vec2(500.0, 400.0), &mut time, 0.1, vec![], |ui| {
                ui.set_clip_rect(view);
                rect = ui.scope(|ui| {
                    if full {
                        // Every block drawn, as a short text's are.
                        let mut cx = Ctx { t: &t, id: Id::unique("far-full"), n: 0, highlighted: false };
                        ui.spacing_mut().item_spacing.y = 6.0;
                        draw_blocks(ui, &mut cx, &parse_cached(&md), 0, t.ink);
                    } else {
                        show(ui, &t, &md, Id::unique("far-test"));
                    }
                })
                .response
                .rect;
            });
            let texts: Vec<(String, Rect)> = crate::app_hover_test::painted(&out);
            (rect, texts)
        };
        let n = parse(&md).len();
        let (full_rect, full_texts) = frame(true);
        DRAWN.set(0);
        let (first_rect, first_texts) = frame(false);
        assert!(DRAWN.get() >= n, "the first frame draws every block");
        // (Measured once more, to be sure of it.)
        frame(false);
        DRAWN.set(0);
        let (rect, texts) = frame(false);
        let drawn = DRAWN.get();
        assert!(full_rect.height() > 4.0 * view.height(), "{full_rect:?}");
        assert_eq!(first_rect, full_rect);
        assert_eq!(first_texts, full_texts);
        assert_eq!(rect, full_rect);
        assert!(drawn * 3 < n, "{drawn} of {n} blocks drawn");
        let shown = |v: &[(String, Rect)]| v.iter().filter(|(_, r)| r.intersects(view)).cloned().collect::<Vec<_>>();
        assert_eq!(shown(&texts), shown(&full_texts));
    }

    /// Laid out off screen for its sketch, a long text's blocks sketched
    /// before are their ink: the sketch is the same as when they are drawn.
    #[test]
    fn far_blocks_sketch_the_same() {
        let md = long_reply(30);
        let (ctx, t) = test_ctx(true, false);
        let mut time = 0.0;
        let mut frame = |full: bool| {
            let mut sketch = vec![];
            let mut rect = Rect::NOTHING;
            let out = run_frame(&ctx, vec2(500.0, 400.0), &mut time, 0.1, vec![], |ui| {
                // As `vlist` sketches a unit: clipped to all of it.
                ui.set_clip_rect(Rect::from_min_size(egui::Pos2::ZERO, vec2(500.0, 1.0e6)));
                let layer = ui.layer_id();
                let start = crate::vlist::paint_len(ui.ctx(), layer);
                rect = crate::vlist::as_sketching(|| {
                    ui.scope(|ui| {
                        if full {
                            let mut cx = Ctx { t: &t, id: Id::unique("sketch-full"), n: 0, highlighted: false };
                            ui.spacing_mut().item_spacing.y = 6.0;
                            draw_blocks(ui, &mut cx, &parse_cached(&md), 0, t.ink);
                        } else {
                            show(ui, &t, &md, Id::unique("sketch-test"));
                        }
                    })
                    .response
                    .rect
                });
                let end = crate::vlist::paint_len(ui.ctx(), layer);
                let all = egui::Rangef::new(f32::NEG_INFINITY, f32::INFINITY);
                sketch = crate::vlist::capture(ui.ctx(), layer, (start, end), rect.min, all);
            });
            drop(out);
            (rect, sketch)
        };
        // Every block drawn (its tables' grids settled on the second frame).
        frame(true);
        let (rect0, sketch0) = frame(true);
        // Measured, and measured again to be sure of it.
        DRAWN.set(0);
        frame(false);
        frame(false);
        let drawn0 = DRAWN.get();
        DRAWN.set(0);
        let (rect, sketch) = frame(false);
        let drawn = DRAWN.get();
        assert_eq!(rect, rect0);
        assert!(drawn * 20 < drawn0, "{drawn} blocks drawn again, of {drawn0}");
        assert!(sketch0.len() > 500, "{}", sketch0.len());
        assert_eq!(sketch.len(), sketch0.len());
        assert!(same_ink(&sketch, &sketch0));
    }

    /// A click anywhere on a link opens it, its last glyph's right half
    /// included; past the text, nothing.
    #[test]
    fn clicking_a_link_opens_it() {
        let (ctx, t) = test_ctx(true, false);
        let md = "See [the readme](README.md) now.";
        let mut time = 0.0;
        let mut run = |events: Vec<egui::Event>| {
            let out = run_frame(&ctx, vec2(600.0, 300.0), &mut time, 0.1, events, |ui| {
                set_link_base(ui.ctx(), Some(std::path::Path::new("/work")));
                show(ui, &t, md, Id::unique("link-test"));
            });
            out.shapes.iter().filter_map(|s| if let egui::Shape::Text(x) = &s.shape { Some((x.pos, x.galley.clone())) } else { None }).collect::<Vec<_>>()
        };
        let (pos, galley) = run(vec![]).last().cloned().unwrap();
        let x_of = |i: usize| galley.pos_from_cursor(egui::text::CCursor::new(i)).min.x;
        let click_at = |x: f32| click(pos + vec2(x, galley.size().y / 2.0), egui::PointerButton::Primary);
        let readme = std::path::Path::new("/work").join("README.md").display().to_string();
        // The middle of "the readme", the right half of its "e", then
        // "now".
        for (x, want) in [((x_of(4) + x_of(14)) / 2.0, true), (x_of(14) - 1.5, true), (x_of(17) + 2.0, false)] {
            OPENED.lock().unwrap().clear();
            run(vec![egui::Event::PointerMoved(pos + vec2(x, 4.0))]);
            run(click_at(x));
            let opened = OPENED.lock().unwrap().clone();
            assert_eq!(opened == [readme.clone()], want, "at x={x}: {opened:?}");
        }
    }

    /// A web address in the text is a link, without the punctuation
    /// after it; one in code stays code.
    #[test]
    fn bare_urls_are_links() {
        let b = parse("See https://x.dev/a_(b). Or (http://y.io/c), not `https://z.dev`.");
        let Block::Para(spans) = &b[0] else { panic!("{b:?}") };
        let links: Vec<_> = spans.iter().filter_map(|s| s.link.as_deref()).collect();
        assert_eq!(links, ["https://x.dev/a_(b)", "http://y.io/c"]);
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "See https://x.dev/a_(b). Or (http://y.io/c), not https://z.dev.");
    }

    #[test]
    fn unfinished_stream() {
        // A reply cut off mid-fence still renders the code so far.
        let b = parse("Here:\n\n```py\nprint(1)\n");
        assert!(matches!(&b[1], Block::Code { text, .. } if text == "print(1)\n"));
    }

    /// Duck.ai's replies color their code; the plain path draws the same
    /// text, and neither panics on an unknown fence or an empty block.
    #[test]
    fn highlighted_code_draws() {
        let (ctx, t) = test_ctx(true, false);
        let md = "Code:\n\n```rust\nfn main() { let x = 1; }\n```\n\n```klingon\nqo'noS\n```\n\n```\n```\n";
        for highlighted in [true, false] {
            run_frame(&ctx, vec2(600.0, 300.0), &mut 0.0, 1.0 / 60.0, vec![], |ui| {
                let id = Id::unique(("md", highlighted));
                let mut cx = Ctx { t: &t, id, n: 0, highlighted };
                draw_blocks(ui, &mut cx, &parse(md), 0, t.ink);
            });
        }
    }
}
