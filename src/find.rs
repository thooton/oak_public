//! Find in a conversation (Ctrl+F): a bar over the transcript's top right
//! that finds text in the messages (the user's and the model's replies, as
//! they are shown), counts the matches, steps through them (Enter, Shift+
//! Enter, F3) and scrolls each into view. Matches are marked where they
//! are drawn (`mark`), the current one stronger.

use crate::Theme;
use eframe::egui::{self, Color32, Id, Key, Modifiers, Pos2, Rect, RichText, Ui, text::LayoutJob, vec2};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

/// A conversation's find bar (kept in egui's temp data, by conversation).
#[derive(Clone, Default)]
pub(crate) struct Find {
    pub open: bool,
    pub query: String,
    /// The current match, an index into `matches`.
    current: usize,
    /// The matches: (unit, which of the unit's, in the order drawn).
    matches: Vec<(usize, usize)>,
    /// Each unit's match count, by its signature and the query.
    counts: HashMap<u64, usize>,
    /// What `matches` were found for, and the query they were found for.
    found_for: u64,
    searched: String,
    /// The current match is to be scrolled into view (frames left to try).
    reveal: u8,
    /// The field takes the keyboard (just opened).
    focus: bool,
}

/// With two transcripts visible, shortcuts belong to the pane last clicked.
pub(crate) fn keyboard_target(ctx: &egui::Context, target: Option<String>) {
    ctx.data_mut(|d| d.insert_temp(Id::unique("find-key-target"), target));
}

fn takes_keys(ui: &Ui, id: &str) -> bool {
    ui.data(|d| d.get_temp::<Option<String>>(Id::unique("find-key-target"))).flatten().is_none_or(|target| target == id)
}

/// The color matches are marked in, and the current one.
const MATCH: Color32 = Color32::from_rgba_premultiplied(0x5c, 0x4c, 0x1c, 0x60);
const CURRENT: Color32 = Color32::from_rgba_premultiplied(0xc8, 0x82, 0x1e, 0xc8);

/// While a transcript is drawn: what to mark, and where the current match
/// was drawn.
struct Paint {
    /// The query, as `fold`ed chars.
    needle: Vec<char>,
    /// The current match: (unit, which of the unit's).
    current: Option<(usize, usize)>,
    /// The unit being drawn, and how many matches it has drawn so far.
    unit: usize,
    seen: usize,
    /// Where the current match was drawn (units are also laid out off
    /// screen, to measure them).
    found: Vec<Rect>,
}

thread_local! {
    static PAINT: RefCell<Option<Paint>> = const { RefCell::new(None) };
}

/// One char as matching compares it (ignoring case).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Where `needle` (folded chars) is in `hay`, ignoring case: byte ranges,
/// not overlapping.
fn find_in(hay: &str, needle: &[char]) -> Vec<Range<usize>> {
    let mut out = vec![];
    if needle.is_empty() {
        return out;
    }
    let chars: Vec<(usize, char)> = hay.char_indices().collect();
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        if (0..needle.len()).all(|k| fold(chars[i + k].1) == needle[k]) {
            let (end_at, end_c) = chars[i + needle.len() - 1];
            out.push(chars[i].0..end_at + end_c.len_utf8());
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

fn needle(query: &str) -> Vec<char> {
    query.chars().map(fold).collect()
}

/// The texts of a unit that find looks in, in the order they are drawn.
pub(crate) enum Text<'a> {
    /// Drawn as it is.
    Plain(&'a str),
    /// Drawn as markdown.
    Markdown(&'a str),
}

/// How many matches of `query` are in `texts`.
fn count(texts: &[Text], query: &[char]) -> usize {
    texts
        .iter()
        .map(|t| match t {
            Text::Plain(s) => find_in(s, query).len(),
            Text::Markdown(s) => crate::markdown::plain_parts(s).iter().map(|p| find_in(p, query).len()).sum(),
        })
        .sum()
}

impl Find {
    /// The matches in the units (`texts` gives a unit's; `sigs` their
    /// signatures, so only units that changed are searched again). True
    /// when the query is new.
    fn search<'a>(&mut self, sigs: &[u64], texts: impl Fn(usize) -> Vec<Text<'a>>) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (&self.query, sigs).hash(&mut h);
        let key = h.finish();
        if key == self.found_for {
            return false;
        }
        self.found_for = key;
        let new = self.searched != self.query;
        self.searched = self.query.clone();
        let query = needle(&self.query);
        let mut counts = HashMap::new();
        self.matches.clear();
        if !query.is_empty() {
            for (u, sig) in sigs.iter().enumerate() {
                let mut h = std::hash::DefaultHasher::new();
                (sig, u, &self.query).hash(&mut h);
                let k = h.finish();
                let n = *self.counts.get(&k).unwrap_or(&usize::MAX);
                let n = if n == usize::MAX { count(&texts(u), &query) } else { n };
                counts.insert(k, n);
                self.matches.extend((0..n).map(|i| (u, i)));
            }
        }
        self.counts = counts;
        self.current = self.current.min(self.matches.len().saturating_sub(1));
        new
    }

    fn step(&mut self, by: isize) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.current = (self.current as isize + by).rem_euclid(n as isize) as usize;
        self.reveal = 3;
    }
}

/// Before a transcript is drawn: the find bar's state for conversation
/// `id`, Ctrl+F opening it, and the matches marked as the units are drawn.
/// `sigs` are the units' signatures, `texts` gives a unit's texts, `first`
/// is the first unit in view (the search starts there).
pub(crate) fn begin<'a>(ui: &Ui, id: &str, sigs: &[u64], texts: impl Fn(usize) -> Vec<Text<'a>>, first: usize) -> Find {
    let key = Id::unique(("find", id));
    let mut f: Find = ui.data(|d| d.get_temp(key)).unwrap_or_default();
    if takes_keys(ui, id) && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
        f.open = true;
        f.focus = true;
    }
    if f.open {
        // A new query (or the bar opened again) starts at the first match
        // from the view on.
        let new = f.search(sigs, texts);
        if f.matches.is_empty() {
            f.reveal = 0;
        } else if f.focus || new {
            f.current = f.matches.iter().position(|m| m.0 >= first).unwrap_or(0);
            f.reveal = 3;
        }
    }
    PAINT.with(|p| {
        *p.borrow_mut() = (f.open && !f.query.is_empty()).then(|| Paint { needle: needle(&f.query), current: f.matches.get(f.current).copied(), unit: 0, seen: 0, found: vec![] });
    });
    f
}

/// Unit `i` is drawn next.
pub(crate) fn begin_unit(i: usize) {
    PAINT.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            p.unit = i;
            p.seen = 0;
        }
    });
}

/// Whether there are matches to mark.
pub(crate) fn active() -> bool {
    PAINT.with(|p| p.borrow().is_some())
}

/// Marks the matches in `job`, about to be laid out; returns where the
/// current match starts in it (a char index), if it is there.
pub(crate) fn mark(job: &mut LayoutJob) -> Option<usize> {
    PAINT.with(|p| {
        let mut p = p.borrow_mut();
        let p = p.as_mut()?;
        let found = find_in(&job.text, &p.needle);
        if found.is_empty() {
            return None;
        }
        let mut current = None;
        let mut colored = vec![];
        for r in found {
            let is_current = p.current == Some((p.unit, p.seen));
            p.seen += 1;
            if is_current {
                current = Some(job.text[..r.start].chars().count());
            }
            colored.push((r, if is_current { CURRENT } else { MATCH }));
        }
        // The sections, split where the matches start and end.
        let mut sections = vec![];
        let section = |range: Range<usize>, leading_space: f32, format: egui::TextFormat| egui::text::LayoutSection { byte_range: range.start.into()..range.end.into(), leading_space, format };
        for s in std::mem::take(&mut job.sections) {
            let (start, end): (usize, usize) = (s.byte_range.start.into(), s.byte_range.end.into());
            let lead = |at: usize| if at == start { s.leading_space } else { 0.0 };
            let mut at = start;
            for (r, color) in &colored {
                let (a, b) = (r.start.max(at), r.end.min(end));
                if a >= b {
                    continue;
                }
                if a > at {
                    sections.push(section(at..a, lead(at), s.format.clone()));
                }
                let mut format = s.format.clone();
                format.background = *color;
                sections.push(section(a..b, lead(a), format));
                at = b;
            }
            if at < end {
                sections.push(section(at..end, lead(at), s.format.clone()));
            }
        }
        job.sections = sections;
        current
    })
}

/// The current match was laid out in `galley`, drawn at `origin`, at char
/// `at`: where it is, to scroll it into view.
pub(crate) fn found(galley: &egui::Galley, origin: Pos2, at: usize) {
    let r = galley.pos_from_cursor(egui::text::CCursor::new(at)).translate(origin.to_vec2());
    PAINT.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            p.found.push(r);
        }
    });
}

/// After the transcript is drawn: the find bar (over `view`'s top right,
/// left of `right`), and the current match scrolled into view.
pub(crate) fn end(ui: &mut Ui, t: &Theme, id: &str, mut f: Find, list: &mut crate::vlist::List, view: Rect, right: f32) {
    let paint = PAINT.with(|p| p.borrow_mut().take());
    if f.open {
        bar(ui, t, id, &mut f, Rect::from_min_size(egui::pos2(right - 330.0, view.top() + 8.0), vec2(320.0, 32.0)));
    }
    if f.reveal > 0
        && let Some(&current) = f.matches.get(f.current)
    {
        // Where it was drawn on screen this frame (not off screen, being
        // measured). The bar may have stepped since it was drawn: then it
        // is looked for next frame.
        let stale = paint.as_ref().is_none_or(|p| p.current != Some(current));
        let found = paint.and_then(|p| p.found.into_iter().find(|r| view.x_range().contains(r.center().x)));
        match found {
            _ if stale => {}
            Some(r) => {
                // Clear of the edges, else in the middle.
                if r.top() < view.top() + 48.0 || r.bottom() > view.bottom() - 48.0 {
                    list.scroll_by(r.center().y - view.center().y, view.height());
                }
                f.reveal = 0;
            }
            // Not on screen: its unit is brought into view, then the match
            // placed in it.
            None => {
                list.jump_to(current.0, view.height());
                f.reveal -= 1;
            }
        }
        ui.ctx().request_repaint();
    }
    ui.data_mut(|d| d.insert_temp(Id::unique(("find", id)), f));
}

fn bar(ui: &mut Ui, t: &Theme, id: &str, f: &mut Find, rect: Rect) {
    let field = Id::unique(("find-field", id));
    let mut close = false;
    egui::Area::new(Id::unique(("find-bar", id))).fixed_pos(rect.min).order(egui::Order::Foreground).show(ui.ctx(), |ui| {
        egui::Frame::new().fill(t.panel).stroke(egui::Stroke::new(1.0, t.line)).corner_radius(6.0).inner_margin(egui::Margin::symmetric(8, 5)).shadow(egui::Shadow { offset: [0, 2], blur: 8, spread: 0, color: Color32::from_black_alpha(60) }).show(ui, |ui| {
            ui.set_width(rect.width() - 16.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let edit = egui::TextEdit::singleline(&mut f.query).id(field).hint_text("Find in conversation").desired_width(rect.width() - 140.0);
                let resp = crate::text_menu::add(ui, edit);
                if std::mem::take(&mut f.focus) {
                    resp.request_focus();
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), field) {
                        let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(f.query.chars().count()));
                        state.cursor.set_char_range(Some(all));
                        state.store(ui.ctx(), field);
                    }
                }
                // Enter steps on (a single-line field lets go of the keyboard
                // on Enter: it takes it back), Shift+Enter back; F3 too.
                let enter = ui.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } => Some(modifiers.shift),
                        _ => None,
                    })
                });
                let (enter, shift) = (enter.is_some(), enter == Some(true));
                if resp.lost_focus() && enter {
                    f.step(if shift { -1 } else { 1 });
                    resp.request_focus();
                }
                let f3 = takes_keys(ui, id).then(|| ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::F3).then_some(-1).or(i.consume_key(Modifiers::NONE, Key::F3).then_some(1)))).flatten();
                if let Some(by) = f3 {
                    f.step(by);
                }
                if (resp.has_focus() || resp.lost_focus()) && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                    close = true;
                }
                let n = f.matches.len();
                let count = if f.query.is_empty() { String::new() } else if n == 0 { "No matches".into() } else { format!("{} of {n}", f.current + 1) };
                ui.label(RichText::new(count).size(12.0).color(if n == 0 && !f.query.is_empty() { t.error } else { t.ink3 }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let small = |s: &str| egui::Button::new(RichText::new(s).size(13.0).color(t.ink2)).frame(false);
                    if ui.add(small("✕")).on_hover_text("Close (Esc)").clicked() {
                        close = true;
                    }
                    if ui.add_enabled(n > 0, small("↓")).on_hover_text("Next (Enter)").clicked() {
                        f.step(1);
                    }
                    if ui.add_enabled(n > 0, small("↑")).on_hover_text("Previous (Shift+Enter)").clicked() {
                        f.step(-1);
                    }
                });
            });
        });
    });
    if close {
        f.open = false;
        f.reveal = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matching ignores case, doesn't overlap, and gives byte ranges (for
    /// text that isn't ASCII too).
    #[test]
    fn finds_ignoring_case() {
        assert_eq!(find_in("Foo foo FOO", &needle("foo")), vec![0..3, 4..7, 8..11]);
        assert_eq!(find_in("aaaa", &needle("aa")), vec![0..2, 2..4]);
        assert_eq!(find_in("Über über", &needle("ÜBER")), vec![0..5, 6..11]);
        assert!(find_in("abc", &needle("")).is_empty());
    }

    /// Marking splits a job's sections at the matches, keeping their
    /// formats, the current match in its own color.
    #[test]
    fn marks_matches_in_a_job() {
        let mut job = LayoutJob::default();
        let fmt = egui::TextFormat::default();
        job.append("one Two ", 0.0, fmt.clone());
        job.append("two three", 0.0, egui::TextFormat { italics: true, ..fmt });
        PAINT.with(|p| *p.borrow_mut() = Some(Paint { needle: needle("two"), current: Some((4, 1)), unit: 4, seen: 0, found: vec![] }));
        let at = mark(&mut job);
        PAINT.with(|p| *p.borrow_mut() = None);
        assert_eq!(at, Some(8));
        let parts: Vec<(&str, bool, Color32)> = job.sections.iter().map(|s| (&job.text[s.byte_range.start.into()..s.byte_range.end.into()], s.format.italics, s.format.background)).collect();
        assert_eq!(parts, [("one ", false, Color32::TRANSPARENT), ("Two", false, MATCH), (" ", false, Color32::TRANSPARENT), ("two", true, CURRENT), (" three", true, Color32::TRANSPARENT)]);
    }
}
