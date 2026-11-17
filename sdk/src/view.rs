//! The parts a page is made of, in Oak's look: what most plugins would
//! otherwise draw by hand. Oak draws each (`abi`'s `oak_page`,
//! `oak_action`, ...), in the theme in use, so a plugin's page looks like
//! Oak's own and follows it when Oak's look changes.
//!
//! Each is a builder: made with `new` (or a kind's own maker, as
//! `Action::primary`), set up with its methods, drawn with `show` (those
//! without a body are `Widget`s too: `ui.add(Badge::new(..))`).
//!
//! ```ignore
//! use oak_sdk::view::{Action, Field, Item, Page, Section, Tone, toast};
//!
//! Page::new("Bookmarks").subtitle("Pages kept for later.").show(ui, |ui| {
//!     Section::new("Add").card().show(ui, |ui| {
//!         let r = Field::new(&mut s.url).label("Address").hint("https://").button("Add").show(ui);
//!         if r.confirmed() {
//!             s.add();
//!             toast(Tone::Good, "Added");
//!         }
//!     });
//!     for b in &s.all {
//!         if Item::new(&b.title).subtitle(&b.url).show(ui).clicked() { /* ... */ }
//!     }
//! });
//! ```
//!
//! Beside them: `State` (what a plugin keeps while Oak runs), `Stored`
//! (what it keeps from one run to the next, in Oak's folder) and `Job`
//! (work on a thread of its own, its result picked up while drawing).

use crate::abi::*;
use crate::egui::{Align, Color32, Id, InnerResponse, Response, TextureId, Ui, Widget, align_code, color_in, color_out, with_body};
use crate::rt::{api, carry, has_api, rethrow, s, text_from};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::ffi::c_void;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard};

/// What something says by its color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tone {
    #[default]
    Plain,
    /// The theme's accent.
    Accent,
    /// Done, connected.
    Good,
    Warn,
    /// An error.
    Bad,
}

impl Tone {
    pub fn code(self) -> u32 {
        match self {
            Tone::Plain => OAK_TONE_PLAIN,
            Tone::Accent => OAK_TONE_ACCENT,
            Tone::Good => OAK_TONE_GOOD,
            Tone::Warn => OAK_TONE_WARN,
            Tone::Bad => OAK_TONE_BAD,
        }
    }
}

/// The color `tone` is drawn in, in the theme in use.
pub fn tone_color(tone: Tone) -> Color32 {
    color_out(unsafe { (api().oak_tone_color)(tone.code()) })
}

/// A short message in the window's corner that goes by itself ("Saved",
/// "Couldn't reach the server"). Any thread.
pub fn toast(tone: Tone, text: &str) {
    if has_api() {
        unsafe { (api().toast)(tone.code(), s(text)) };
    }
}

// ------------------------------------------------------------------ look

/// How a component looks, over Oak's: give it to one (`.look(..)` on any
/// builder), or to all drawn in a body (`look_scope`). Every part is
/// optional: what isn't set stays as Oak draws it. A length of 0 is a true
/// zero (no padding, square corners).
///
/// ```ignore
/// let loud = Look::new().fill(Color32::from_rgb(0x2b, 0x5c, 0xd6)).ink(Color32::WHITE).radius(6.0).height(36.0);
/// Action::new("Deploy").look(loud).show(ui);
/// look_scope(ui, &Look::new().accent(Color32::from_rgb(0xe0, 0x6c, 0x9f)).radius(4.0), |ui| { /* the whole page */ });
/// ```
///
/// What each part means to each component is in docs/PLUGINS.md; most
/// take the colors, `radius`, `stroke_width` and `text_size`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[must_use]
pub struct Look {
    pub fill: Option<Color32>,
    pub fill_hover: Option<Color32>,
    pub stroke: Option<Color32>,
    pub ink: Option<Color32>,
    pub ink_weak: Option<Color32>,
    pub accent: Option<Color32>,
    pub good: Option<Color32>,
    pub warn: Option<Color32>,
    pub bad: Option<Color32>,
    pub radius: Option<f32>,
    pub stroke_width: Option<f32>,
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    pub height: Option<f32>,
    pub width: Option<f32>,
    pub text_size: Option<f32>,
    pub gap: Option<f32>,
    pub text_scale: Option<f32>,
    pub monospace: bool,
    pub no_frame: bool,
}

impl Look {
    pub fn new() -> Look {
        Look::default()
    }

    /// Its background.
    pub fn fill(mut self, c: Color32) -> Self {
        self.fill = Some(c);
        self
    }

    /// Its background under the pointer.
    pub fn fill_hover(mut self, c: Color32) -> Self {
        self.fill_hover = Some(c);
        self
    }

    /// Its outline (a row's line, a notice's bar).
    pub fn stroke(mut self, c: Color32) -> Self {
        self.stroke = Some(c);
        self
    }

    /// Its text.
    pub fn ink(mut self, c: Color32) -> Self {
        self.ink = Some(c);
        self
    }

    /// Its fainter text (help, subtitles, captions).
    pub fn ink_weak(mut self, c: Color32) -> Self {
        self.ink_weak = Some(c);
        self
    }

    /// What is chosen or matters most: primary buttons, switches, bars,
    /// selections, links.
    pub fn accent(mut self, c: Color32) -> Self {
        self.accent = Some(c);
        self
    }

    /// `Tone::Good`'s color.
    pub fn good(mut self, c: Color32) -> Self {
        self.good = Some(c);
        self
    }

    pub fn warn(mut self, c: Color32) -> Self {
        self.warn = Some(c);
        self
    }

    /// `Tone::Bad`'s color, and errors'.
    pub fn bad(mut self, c: Color32) -> Self {
        self.bad = Some(c);
        self
    }

    /// Its corners' radius.
    pub fn radius(mut self, r: f32) -> Self {
        self.radius = Some(r);
        self
    }

    /// Its outline's width.
    pub fn stroke_width(mut self, w: f32) -> Self {
        self.stroke_width = Some(w);
        self
    }

    /// Inside it, all around.
    pub fn padding(self, p: f32) -> Self {
        self.padding_x(p).padding_y(p)
    }

    /// Inside it, at its sides.
    pub fn padding_x(mut self, p: f32) -> Self {
        self.padding_x = Some(p);
        self
    }

    /// Inside it, above and below.
    pub fn padding_y(mut self, p: f32) -> Self {
        self.padding_y = Some(p);
        self
    }

    pub fn height(mut self, h: f32) -> Self {
        self.height = Some(h);
        self
    }

    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }

    /// Its main text's size in points (its other texts follow it).
    pub fn text_size(mut self, points: f32) -> Self {
        self.text_size = Some(points);
        self
    }

    /// Between its parts, or after it (a section, a page's header).
    pub fn gap(mut self, g: f32) -> Self {
        self.gap = Some(g);
        self
    }

    /// Its texts' sizes times this (in a scope: all of them, egui's too).
    pub fn text_scale(mut self, k: f32) -> Self {
        self.text_scale = Some(k);
        self
    }

    /// Its texts in the code font.
    pub fn monospace(mut self) -> Self {
        self.monospace = true;
        self
    }

    /// No background or outline (a button, a card, a badge, a notice).
    pub fn no_frame(mut self) -> Self {
        self.no_frame = true;
        self
    }

    /// The interface's struct.
    pub fn raw(&self) -> OakLook {
        let c = |c: Option<Color32>| c.map_or(0, color_in);
        // (0 keeps Oak's: a zero asked for is sent below 0.)
        let len = |v: Option<f32>| v.map_or(0.0, |v| if v > 0.0 { v } else { -1.0 });
        OakLook {
            size: size_of::<OakLook>(),
            flags: flag(self.monospace, OAK_LOOK_MONOSPACE) | flag(self.no_frame, OAK_LOOK_NO_FRAME),
            fill: c(self.fill),
            fill_hover: c(self.fill_hover),
            stroke: c(self.stroke),
            ink: c(self.ink),
            ink_weak: c(self.ink_weak),
            accent: c(self.accent),
            good: c(self.good),
            warn: c(self.warn),
            bad: c(self.bad),
            radius: len(self.radius),
            stroke_width: len(self.stroke_width),
            padding_x: len(self.padding_x),
            padding_y: len(self.padding_y),
            height: len(self.height),
            width: len(self.width),
            text_points: self.text_size.filter(|p| *p > 0.0).unwrap_or(0.0),
            gap: len(self.gap),
            text_scale: self.text_scale.filter(|k| *k > 0.0).unwrap_or(0.0),
        }
    }
}

/// The next component drawn (only it) looks as `look` says: for drawing
/// with the interface's own calls. (Builders take `.look(..)`.)
pub fn next_look(look: &Look) {
    unsafe { (api().oak_look_next)(&look.raw()) };
}

fn next(look: &Option<Look>) {
    if let Some(l) = look {
        next_look(l);
    }
}

/// What `body` draws looks as `look` says: its colors, corners, outlines,
/// text scale and code font apply to every component in it, and egui's
/// widgets take its colors. Scopes in scopes add to it.
pub fn look_scope<R>(ui: &mut Ui, look: &Look, body: impl FnOnce(&mut Ui) -> R) -> R {
    let raw = ui.as_raw();
    let l = look.raw();
    with_body(body, |body, user| unsafe { (api().oak_look_scope)(raw, &l, body, user) }).0.expect("Oak didn't draw the scope")
}

fn size_of<T>() -> u32 {
    std::mem::size_of::<T>() as u32
}

fn flag(on: bool, f: u32) -> u32 {
    if on { f } else { 0 }
}

fn id_of(id: Option<Id>) -> u64 {
    id.map_or(0, |i| i.value())
}

/// Runs `call` with `f` (which may be absent) as a body Oak calls back.
fn maybe_body<T>(f: Option<impl FnOnce(&mut Ui)>, call: impl FnOnce(Option<OakBody>, *mut c_void) -> T) -> T {
    match f {
        Some(f) => with_body(f, |body, user| call(Some(body), user)).1,
        None => call(None, std::ptr::null_mut()),
    }
}

/// Runs `call` with `f` as a cell Oak calls back, as often as it likes.
fn with_cell<T>(f: &mut dyn FnMut(&mut Ui, usize, usize), call: impl FnOnce(OakCell, *mut c_void) -> T) -> T {
    struct Cells<'a> {
        f: &'a mut dyn FnMut(&mut Ui, usize, usize),
        /// It panicked: the rest aren't drawn.
        failed: bool,
    }
    unsafe extern "C" fn run(user: *mut c_void, ui: *mut OakUi, row: usize, column: usize) {
        let c = unsafe { &mut *(user as *mut Cells) };
        if !c.failed {
            let f = &mut c.f;
            c.failed = carry(|| f(&mut unsafe { Ui::from_raw(ui) }, row, column)).is_none();
        }
    }
    let mut cells = Cells { f, failed: false };
    let t = call(run, &mut cells as *mut Cells as *mut c_void);
    rethrow();
    t
}

// ------------------------------------------------------------------ page

/// A page: a column in the middle of what scrolls, its title on top.
#[must_use]
pub struct Page<'a, A = fn(&mut Ui)> {
    look: Option<Look>,
    title: &'a str,
    subtitle: &'a str,
    back: &'a str,
    id: Option<Id>,
    width: f32,
    no_scroll: bool,
    actions: Option<A>,
}

impl<'a> Page<'a> {
    pub fn new(title: &'a str) -> Page<'a> {
        Page { look: None, title, subtitle: "", back: "", id: None, width: 0.0, no_scroll: false, actions: None }
    }
}

impl<'a, A: FnOnce(&mut Ui)> Page<'a, A> {
    /// A line under the title.
    pub fn subtitle(mut self, text: &'a str) -> Self {
        self.subtitle = text;
        self
    }

    /// A link above the title (to the page this one was opened from): it
    /// and Escape make `show` return true.
    pub fn back(mut self, text: &'a str) -> Self {
        self.back = text;
        self
    }

    /// Where the scroll position is kept (by default, from the title).
    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    /// The column's widest (760 by default).
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }

    /// No scroll area: what it holds scrolls itself, or fits.
    pub fn no_scroll(mut self) -> Self {
        self.no_scroll = true;
        self
    }

    /// Draws at the title's right end, right to left (its buttons).
    pub fn actions<B: FnOnce(&mut Ui)>(self, f: B) -> Page<'a, B> {
        Page { title: self.title, subtitle: self.subtitle, back: self.back, id: self.id, width: self.width, no_scroll: self.no_scroll, actions: Some(f), look: self.look }
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    /// Draws it, `body` in its column. True: its back link (or Escape)
    /// was pressed.
    pub fn show(self, ui: &mut Ui, body: impl FnOnce(&mut Ui)) -> bool {
        let raw = ui.as_raw();
        next(&self.look);
        let flags = with_body(body, |body, user| {
            maybe_body(self.actions, |actions, actions_user| {
                let p = OakPage {
                    size: size_of::<OakPage>(),
                    flags: flag(self.no_scroll, OAK_PAGE_NO_SCROLL),
                    id: id_of(self.id),
                    title: s(self.title),
                    subtitle: s(self.subtitle),
                    back: s(self.back),
                    width: self.width,
                    actions,
                    actions_user,
                };
                unsafe { (api().oak_page)(raw, &p, body, user) }
            })
        })
        .1;
        flags & OAK_CANCELLED != 0
    }
}

// --------------------------------------------------------------- heading

/// A heading: a section's (by default), a page's title, or a small
/// caption's.
#[must_use]
pub struct Heading<'a, R = fn(&mut Ui)> {
    text: &'a str,
    level: u32,
    flags: u32,
    detail: &'a str,
    icon: &'a str,
    action: &'a str,
    look: Option<Look>,
    right: Option<R>,
}

impl<'a> Heading<'a> {
    pub fn new(text: &'a str) -> Heading<'a> {
        Heading { text, level: OAK_HEADING_SECTION, flags: 0, detail: "", icon: "", action: "", look: None, right: None }
    }
}

impl<'a, R: FnOnce(&mut Ui)> Heading<'a, R> {
    /// A page's title's size.
    pub fn title(mut self) -> Self {
        self.level = OAK_HEADING_TITLE;
        self
    }

    /// A small faint caption.
    pub fn small(mut self) -> Self {
        self.level = OAK_HEADING_SMALL;
        self
    }

    /// Faint, after the text (a count).
    pub fn detail(mut self, text: &'a str) -> Self {
        self.detail = text;
        self
    }

    /// A Nerd Font symbol before the text.
    pub fn icon(mut self, glyph: &'a str) -> Self {
        self.icon = glyph;
        self
    }

    /// A link at the right end ("See all"): the response is clicked when
    /// it is pressed.
    pub fn action(mut self, text: &'a str) -> Self {
        self.action = text;
        self
    }

    /// A line under it.
    pub fn rule(mut self) -> Self {
        self.flags |= OAK_HEADING_RULE;
        self
    }

    /// A spinner after it.
    pub fn busy(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_HEADING_BUSY);
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    /// Draws at its right end, right to left, before its action (buttons,
    /// a badge, a choice): what it draws takes its own clicks.
    pub fn right<B: FnOnce(&mut Ui)>(self, f: B) -> Heading<'a, B> {
        let Heading { text, level, flags, detail, icon, action, look, .. } = self;
        Heading { text, level, flags, detail, icon, action, look, right: Some(f) }
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let raw = ui.as_raw();
        let mut out = OakResponse::default();
        next(&self.look);
        maybe_body(self.right, |right, right_user| {
            let h = OakHeading {
                size: size_of::<OakHeading>(),
                flags: self.flags,
                level: self.level,
                text: s(self.text),
                detail: s(self.detail),
                icon: s(self.icon),
                action: s(self.action),
                right,
                right_user,
            };
            unsafe { (api().oak_heading)(raw, &h, &mut out) }
        });
        Response::of(out)
    }
}

impl<R: FnOnce(&mut Ui)> Widget for Heading<'_, R> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// --------------------------------------------------------------- section

/// A section: a heading, what it holds under it, and room after.
#[must_use]
pub struct Section<'a, H = fn(&mut Ui)> {
    title: &'a str,
    flags: u32,
    id: Option<Id>,
    detail: &'a str,
    icon: &'a str,
    action: &'a str,
    look: Option<Look>,
    right: Option<H>,
}

impl<'a> Section<'a> {
    pub fn new(title: &'a str) -> Section<'a> {
        Section { title, flags: 0, id: None, detail: "", icon: "", action: "", look: None, right: None }
    }
}

impl<'a, H: FnOnce(&mut Ui)> Section<'a, H> {
    /// What it holds is in a card.
    pub fn card(mut self) -> Self {
        self.flags |= OAK_SECTION_CARD;
        self
    }

    /// Its heading opens and closes it (open at first).
    pub fn folds(mut self) -> Self {
        self.flags |= OAK_SECTION_FOLDS;
        self
    }

    /// Its heading opens and closes it, closed at first.
    pub fn closed(mut self) -> Self {
        self.flags |= OAK_SECTION_FOLDS | OAK_SECTION_CLOSED;
        self
    }

    pub fn busy(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_SECTION_BUSY);
        self
    }

    /// Where whether it is open is kept (by default, from the title).
    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    pub fn detail(mut self, text: &'a str) -> Self {
        self.detail = text;
        self
    }

    pub fn icon(mut self, glyph: &'a str) -> Self {
        self.icon = glyph;
        self
    }

    /// A link at the heading's right end: the response is clicked.
    pub fn action(mut self, text: &'a str) -> Self {
        self.action = text;
        self
    }

    /// How it looks, over Oak's (`Look`): its heading's text, its card's
    /// frame and room, and `gap` the room after it.
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    /// Draws at its heading's right end, right to left, before its action.
    pub fn right<B: FnOnce(&mut Ui)>(self, f: B) -> Section<'a, B> {
        let Section { title, flags, id, detail, icon, action, look, .. } = self;
        Section { title, flags, id, detail, icon, action, look, right: Some(f) }
    }

    /// Draws it; `body` when it is open (the response's `open`).
    pub fn show<R>(self, ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>> {
        let raw = ui.as_raw();
        let mut out = OakResponse::default();
        next(&self.look);
        let (inner, _) = with_body(body, |body, user| {
            maybe_body(self.right, |right, right_user| {
                let sec = OakSection {
                    size: size_of::<OakSection>(),
                    flags: self.flags,
                    id: id_of(self.id),
                    title: s(self.title),
                    detail: s(self.detail),
                    icon: s(self.icon),
                    action: s(self.action),
                    right,
                    right_user,
                };
                unsafe { (api().oak_section)(raw, &sec, body, user, &mut out) }
            })
        });
        InnerResponse { inner, response: Response::of(out) }
    }
}

// ------------------------------------------------------------------ card

/// A bordered box.
#[must_use]
#[derive(Default)]
pub struct Card {
    look: Option<Look>,
    flags: u32,
    id: Option<Id>,
    fill: Option<Color32>,
    padding: f32,
    width: f32,
}

impl Card {
    pub fn new() -> Card {
        Card::default()
    }

    /// Lit under the pointer, and clicked as a whole (where what is in it
    /// doesn't take the click).
    pub fn clickable(mut self) -> Self {
        self.flags |= OAK_CARD_CLICK;
        self
    }

    /// Marked chosen.
    pub fn selected(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_CARD_SELECTED);
        self
    }

    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    /// Its color (the theme's panel color by default).
    pub fn fill(mut self, c: Color32) -> Self {
        self.fill = Some(c);
        self
    }

    /// Inside its border (12 by default).
    pub fn padding(mut self, p: f32) -> Self {
        self.padding = p;
        self
    }

    /// Its width (all there is by default).
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show<R>(self, ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let raw = ui.as_raw();
        let c = OakCard { size: size_of::<OakCard>(), flags: self.flags, id: id_of(self.id), fill: self.fill.map_or(0, color_in), padding: self.padding, width: self.width };
        let mut out = OakResponse::default();
        next(&self.look);
        let (inner, _) = with_body(body, |body, user| unsafe { (api().oak_card)(raw, &c, body, user, &mut out) });
        InnerResponse { inner: inner.expect("Oak didn't draw the card"), response: Response::of(out) }
    }
}

// ---------------------------------------------------------------- action

/// A button: framed (`new`), the page's main one (`primary`), frameless
/// (`quiet`), red (`danger`), a link, a chip (`pill`), or a symbol alone
/// (`icon`).
#[must_use]
pub struct Action<'a> {
    look: Option<Look>,
    kind: u32,
    flags: u32,
    text: &'a str,
    icon: &'a str,
    tip: &'a str,
    min_width: f32,
}

impl<'a> Action<'a> {
    pub fn new(text: &'a str) -> Action<'a> {
        Action { look: None, kind: OAK_ACTION_DEFAULT, flags: 0, text, icon: "", tip: "", min_width: 0.0 }
    }

    fn of(kind: u32, text: &'a str) -> Action<'a> {
        Action { kind, ..Action::new(text) }
    }

    pub fn primary(text: &'a str) -> Action<'a> {
        Action::of(OAK_ACTION_PRIMARY, text)
    }

    pub fn quiet(text: &'a str) -> Action<'a> {
        Action::of(OAK_ACTION_QUIET, text)
    }

    pub fn danger(text: &'a str) -> Action<'a> {
        Action::of(OAK_ACTION_DANGER, text)
    }

    pub fn link(text: &'a str) -> Action<'a> {
        Action::of(OAK_ACTION_LINK, text)
    }

    pub fn pill(text: &'a str) -> Action<'a> {
        Action::of(OAK_ACTION_PILL, text)
    }

    /// A square with a Nerd Font symbol; `tip` says what it does.
    pub fn icon(glyph: &'a str, tip: &'a str) -> Action<'a> {
        Action { icon: glyph, tip, ..Action::of(OAK_ACTION_ICON, "") }
    }

    /// A symbol before the text.
    pub fn glyph(mut self, glyph: &'a str) -> Self {
        self.icon = glyph;
        self
    }

    pub fn tip(mut self, text: &'a str) -> Self {
        self.tip = text;
        self
    }

    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.flags |= flag(!on, OAK_ACTION_DISABLED);
        self
    }

    /// A spinner, and it can't be pressed.
    pub fn busy(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_ACTION_BUSY);
        self
    }

    pub fn selected(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_ACTION_SELECTED);
        self
    }

    pub fn small(mut self) -> Self {
        self.flags |= OAK_ACTION_SMALL;
        self
    }

    /// As wide as there is room.
    pub fn fill(mut self) -> Self {
        self.flags |= OAK_ACTION_FILL;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let a = OakAction { size: size_of::<OakAction>(), flags: self.flags, kind: self.kind, text: s(self.text), icon: s(self.icon), tip: s(self.tip), min_width: self.min_width };
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_action)(ui.as_raw(), &a, &mut out) };
        Response::of(out)
    }
}

impl Widget for Action<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ----------------------------------------------------------------- badge

/// A small tag in a tone's color ("beta", "3 new", "offline").
#[must_use]
pub struct Badge<'a> {
    look: Option<Look>,
    text: &'a str,
    tone: Tone,
}

impl<'a> Badge<'a> {
    pub fn new(text: &'a str, tone: Tone) -> Badge<'a> {
        Badge { look: None, text, tone }
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_badge)(ui.as_raw(), s(self.text), self.tone.code(), 0, &mut out) };
        Response::of(out)
    }
}

impl Widget for Badge<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ------------------------------------------------------------------ item

/// A row of a list, as wide as there is room: what leads it (a dot, a
/// picture, a symbol, or what `left` draws), its title over its subtitle,
/// and at its right end faint text and what `right` draws. Clicked when
/// it is.
#[must_use]
pub struct Item<'a, R = fn(&mut Ui), L = fn(&mut Ui)> {
    title: &'a str,
    flags: u32,
    id: Option<Id>,
    subtitle: &'a str,
    trailing: &'a str,
    icon: &'a str,
    tone: Tone,
    texture: u64,
    height: f32,
    look: Option<Look>,
    right: Option<R>,
    left: Option<L>,
}

impl<'a> Item<'a> {
    pub fn new(title: &'a str) -> Item<'a> {
        Item { title, flags: 0, id: None, subtitle: "", trailing: "", icon: "", tone: Tone::Plain, texture: 0, height: 0.0, look: None, right: None, left: None }
    }
}

impl<'a, R: FnOnce(&mut Ui), L: FnOnce(&mut Ui)> Item<'a, R, L> {
    pub fn subtitle(mut self, text: &'a str) -> Self {
        self.subtitle = text;
        self
    }

    /// Faint text at the right end (a date, a size).
    pub fn trailing(mut self, text: &'a str) -> Self {
        self.trailing = text;
        self
    }

    pub fn icon(mut self, glyph: &'a str) -> Self {
        self.icon = glyph;
        self
    }

    /// A dot before it, in `tone`'s color (a state).
    pub fn dot(mut self, tone: Tone) -> Self {
        self.flags |= OAK_ITEM_DOT;
        self.tone = tone;
        self
    }

    /// A picture before it.
    pub fn texture(mut self, t: impl Into<TextureId>) -> Self {
        self.texture = t.into().0;
        self
    }

    /// Its height (34, or 48 with a subtitle, by default).
    pub fn height(mut self, h: f32) -> Self {
        self.height = h;
        self
    }

    /// Where it keeps what it senses (by default, from where it is drawn):
    /// for rows that move.
    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    pub fn selected(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_ITEM_SELECTED);
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.flags |= flag(!on, OAK_ITEM_DISABLED);
        self
    }

    pub fn busy(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_ITEM_BUSY);
        self
    }

    /// No line under it (the last row of a card).
    pub fn no_line(mut self) -> Self {
        self.flags |= OAK_ITEM_NO_LINE;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    /// Draws at its right end, right to left (buttons, a switch): what it
    /// draws takes its own clicks.
    pub fn right<B: FnOnce(&mut Ui)>(self, f: B) -> Item<'a, B, L> {
        let Item { title, flags, id, subtitle, trailing, icon, tone, texture, height, look, left, .. } = self;
        Item { title, flags, id, subtitle, trailing, icon, tone, texture, height, look, right: Some(f), left }
    }

    /// Draws at its left end, left to right, in place of the dot, picture
    /// and symbol (an avatar, a checkbox): what it draws takes its own
    /// clicks.
    pub fn left<B: FnOnce(&mut Ui)>(self, f: B) -> Item<'a, R, B> {
        let Item { title, flags, id, subtitle, trailing, icon, tone, texture, height, look, right, .. } = self;
        Item { title, flags, id, subtitle, trailing, icon, tone, texture, height, look, right, left: Some(f) }
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let raw = ui.as_raw();
        let mut out = OakResponse::default();
        next(&self.look);
        maybe_body(self.left, |left, left_user| {
            maybe_body(self.right, |right, right_user| {
                let i = OakItem {
                    size: size_of::<OakItem>(),
                    flags: self.flags,
                    id: id_of(self.id),
                    title: s(self.title),
                    subtitle: s(self.subtitle),
                    trailing: s(self.trailing),
                    icon: s(self.icon),
                    tone: self.tone.code(),
                    texture: self.texture,
                    height: self.height,
                    right,
                    right_user,
                    left,
                    left_user,
                };
                unsafe { (api().oak_item)(raw, &i, &mut out) }
            })
        });
        Response::of(out)
    }
}

impl<R: FnOnce(&mut Ui), L: FnOnce(&mut Ui)> Widget for Item<'_, R, L> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ----------------------------------------------------------------- field

/// A text field with its label over it, help (or an error) under it, and
/// a button at its right. The response is changed when the text was
/// edited, and confirmed on Enter or the button.
#[must_use]
pub struct Field<'a> {
    look: Option<Look>,
    text: &'a mut String,
    flags: u32,
    id: Option<Id>,
    label: &'a str,
    hint: &'a str,
    help: &'a str,
    error: &'a str,
    button: &'a str,
    width: f32,
    rows: u32,
}

impl<'a> Field<'a> {
    pub fn new(text: &'a mut String) -> Field<'a> {
        Field { look: None, text, flags: 0, id: None, label: "", hint: "", help: "", error: "", button: "", width: 0.0, rows: 0 }
    }

    pub fn label(mut self, text: &'a str) -> Self {
        self.label = text;
        self
    }

    /// In it while it is empty.
    pub fn hint(mut self, text: &'a str) -> Self {
        self.hint = text;
        self
    }

    pub fn help(mut self, text: &'a str) -> Self {
        self.help = text;
        self
    }

    /// Under it in the error color, instead of the help (empty: none).
    pub fn error(mut self, text: &'a str) -> Self {
        self.error = text;
        self
    }

    /// A button at its right: it and Enter confirm. Off while the field is
    /// empty, unless `empty_ok`.
    pub fn button(mut self, text: &'a str) -> Self {
        self.button = text;
        self
    }

    pub fn empty_ok(mut self) -> Self {
        self.flags |= OAK_FIELD_EMPTY_OK;
        self
    }

    /// Masked, with a Show button (a key, a password).
    pub fn secret(mut self) -> Self {
        self.flags |= OAK_FIELD_SECRET;
        self
    }

    /// `rows` lines high; Enter starts a new line.
    pub fn multiline(mut self, rows: u32) -> Self {
        self.flags |= OAK_FIELD_MULTILINE;
        self.rows = rows;
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.flags |= flag(!on, OAK_FIELD_DISABLED);
        self
    }

    /// A spinner by the button, which is off (while what it started runs).
    pub fn busy(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_FIELD_BUSY);
        self
    }

    /// Takes the keyboard.
    pub fn focus(mut self, on: bool) -> Self {
        self.flags |= flag(on, OAK_FIELD_FOCUS);
        self
    }

    /// Its width (all there is by default).
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }

    /// Where it keeps its cursor (by default, from its label).
    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let f = OakField {
            size: size_of::<OakField>(),
            flags: self.flags,
            id: id_of(self.id),
            label: s(self.label),
            hint: s(self.hint),
            help: s(self.help),
            error: s(self.error),
            button: s(self.button),
            width: self.width,
            rows: self.rows,
        };
        let mut out = OakResponse::default();
        let mut new: Option<String> = None;
        next(&self.look);
        unsafe { (api().oak_field)(ui.as_raw(), &f, s(self.text), crate::egui::sink_string, &mut new as *mut Option<String> as *mut c_void, &mut out) };
        if let Some(t) = new {
            *self.text = t;
        }
        Response::of(out)
    }
}

impl Widget for Field<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ---------------------------------------------------------------- toggle

/// A row with a label, help under it and a switch at its right end: a
/// click anywhere on it flips the value (the response is changed).
#[must_use]
pub struct Toggle<'a> {
    look: Option<Look>,
    on: &'a mut bool,
    label: &'a str,
    help: &'a str,
    flags: u32,
}

impl<'a> Toggle<'a> {
    pub fn new(on: &'a mut bool, label: &'a str) -> Toggle<'a> {
        Toggle { look: None, on, label, help: "", flags: 0 }
    }

    pub fn help(mut self, text: &'a str) -> Self {
        self.help = text;
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.flags |= flag(!on, OAK_TOGGLE_DISABLED);
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_toggle)(ui.as_raw(), self.on, s(self.label), s(self.help), self.flags, &mut out) };
        Response::of(out)
    }
}

impl Widget for Toggle<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// --------------------------------------------------------------- setting

/// A setting's row: `label` and `help` at the left, what `control` draws
/// (a choice, a field, a slider) at the right; one under the other when
/// there is little room.
pub fn setting(ui: &mut Ui, label: &str, help: &str, control: impl FnOnce(&mut Ui)) -> Response {
    let raw = ui.as_raw();
    let mut out = OakResponse::default();
    with_body(control, |body, user| unsafe { (api().oak_setting)(raw, s(label), s(help), Some(body), user, &mut out) });
    Response::of(out)
}

// ---------------------------------------------------------------- choice

/// One of several: joined boxes (by default), tabs, chips, a drop-down or
/// a column of rows. The response is changed when the choice was.
#[must_use]
pub struct Choice<'a> {
    look: Option<Look>,
    selected: &'a mut usize,
    options: &'a [&'a str],
    style: u32,
    id: Option<Id>,
    width: f32,
}

impl<'a> Choice<'a> {
    pub fn new(selected: &'a mut usize, options: &'a [&'a str]) -> Choice<'a> {
        Choice { look: None, selected, options, style: OAK_CHOICE_SEGMENTED, id: None, width: 0.0 }
    }

    /// Tabs over a line (a page's views).
    pub fn tabs(mut self) -> Self {
        self.style = OAK_CHOICE_TABS;
        self
    }

    /// Chips that wrap (filters).
    pub fn pills(mut self) -> Self {
        self.style = OAK_CHOICE_PILLS;
        self
    }

    /// A drop-down, `width` wide (0: as its texts need).
    pub fn dropdown(mut self, width: f32) -> Self {
        self.style = OAK_CHOICE_DROPDOWN;
        self.width = width;
        self
    }

    /// A column of rows, each with a mark.
    pub fn list(mut self) -> Self {
        self.style = OAK_CHOICE_LIST;
        self
    }

    /// Where it keeps what it senses (by default, from its options).
    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let options: Vec<OakStr> = self.options.iter().map(|o| s(o)).collect();
        let c = OakChoice { size: size_of::<OakChoice>(), flags: 0, style: self.style, id: id_of(self.id), options: options.as_ptr(), count: options.len(), width: self.width };
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_choice)(ui.as_raw(), &c, self.selected, &mut out) };
        Response::of(out)
    }
}

impl Widget for Choice<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// -------------------------------------------------------------- progress

/// A bar, with a label over it.
#[must_use]
pub struct Progress<'a> {
    look: Option<Look>,
    fraction: f32,
    label: &'a str,
    tone: Tone,
    flags: u32,
}

impl<'a> Progress<'a> {
    /// `fraction` (0 to 1) full.
    pub fn new(fraction: f32) -> Progress<'a> {
        Progress { look: None, fraction: fraction.clamp(0.0, 1.0), label: "", tone: Tone::Plain, flags: 0 }
    }

    /// Moving: work of unknown length.
    pub fn busy() -> Progress<'a> {
        Progress { fraction: -1.0, ..Progress::new(0.0) }
    }

    pub fn label(mut self, text: &'a str) -> Self {
        self.label = text;
        self
    }

    /// The bar's color (the accent by default).
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// The percentage after the label.
    pub fn percent(mut self) -> Self {
        self.flags |= OAK_PROGRESS_PERCENT;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_progress)(ui.as_raw(), self.fraction, s(self.label), self.tone.code(), self.flags, &mut out) };
        Response::of(out)
    }
}

impl Widget for Progress<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ---------------------------------------------------------------- notice

/// A line saying how things stand, in a tone's color: "Saving..." (busy),
/// "Saved", a warning, an error. Its response is cancelled when its X
/// (`dismiss`) was pressed.
#[must_use]
pub struct Notice<'a> {
    look: Option<Look>,
    tone: Tone,
    text: &'a str,
    flags: u32,
}

impl<'a> Notice<'a> {
    pub fn new(tone: Tone, text: &'a str) -> Notice<'a> {
        Notice { look: None, tone, text, flags: 0 }
    }

    /// A spinner before it.
    pub fn busy(mut self) -> Self {
        self.flags |= OAK_NOTICE_BUSY;
        self
    }

    /// In a tinted box.
    pub fn boxed(mut self) -> Self {
        self.flags |= OAK_NOTICE_BOXED;
        self
    }

    /// With an X.
    pub fn dismiss(mut self) -> Self {
        self.flags |= OAK_NOTICE_DISMISS;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_notice)(ui.as_raw(), self.tone.code(), s(self.text), self.flags, &mut out) };
        Response::of(out)
    }
}

impl Widget for Notice<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ----------------------------------------------------------------- empty

/// What an empty page or list says, in the middle: a symbol, a title, a
/// line and a button (the response is the button's).
#[must_use]
pub struct Empty<'a> {
    look: Option<Look>,
    title: &'a str,
    icon: &'a str,
    text: &'a str,
    action: &'a str,
}

impl<'a> Empty<'a> {
    pub fn new(title: &'a str) -> Empty<'a> {
        Empty { look: None, title, icon: "", text: "", action: "" }
    }

    pub fn icon(mut self, glyph: &'a str) -> Self {
        self.icon = glyph;
        self
    }

    pub fn text(mut self, text: &'a str) -> Self {
        self.text = text;
        self
    }

    pub fn action(mut self, text: &'a str) -> Self {
        self.action = text;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_empty)(ui.as_raw(), s(self.icon), s(self.title), s(self.text), s(self.action), &mut out) };
        Response::of(out)
    }
}

impl Widget for Empty<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ------------------------------------------------------------------ stat

/// A number in a box: its label over it, a detail under it. A row of
/// them: `columns`.
#[must_use]
pub struct Stat<'a> {
    look: Option<Look>,
    label: &'a str,
    value: &'a str,
    detail: &'a str,
    tone: Tone,
}

impl<'a> Stat<'a> {
    pub fn new(label: &'a str, value: &'a str) -> Stat<'a> {
        Stat { look: None, label, value, detail: "", tone: Tone::Plain }
    }

    pub fn detail(mut self, text: &'a str) -> Self {
        self.detail = text;
        self
    }

    /// The number's color (the text's by default).
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_stat)(ui.as_raw(), s(self.label), s(self.value), s(self.detail), self.tone.code(), &mut out) };
        Response::of(out)
    }
}

impl Widget for Stat<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ---------------------------------------------------------------- slider

/// A slider (the response is changed when it moved).
#[must_use]
pub struct Slider<'a> {
    look: Option<Look>,
    value: &'a mut f64,
    min: f64,
    max: f64,
    step: f64,
    width: f32,
    decimals: u32,
    suffix: &'a str,
    flags: u32,
}

impl<'a> Slider<'a> {
    pub fn new(value: &'a mut f64, range: std::ops::RangeInclusive<f64>) -> Slider<'a> {
        Slider { look: None, value, min: *range.start(), max: *range.end(), step: 0.0, width: 0.0, decimals: 0, suffix: "", flags: 0 }
    }

    /// It moves in steps of this.
    pub fn step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// 200 by default.
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }

    /// Its value at its right, with `decimals` digits after the point and
    /// `suffix` after it ("%", " ms").
    pub fn show_value(mut self, decimals: u32, suffix: &'a str) -> Self {
        self.flags |= OAK_SLIDER_VALUE;
        self.decimals = decimals;
        self.suffix = suffix;
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.flags |= flag(!on, OAK_SLIDER_DISABLED);
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let sl = OakSlider { size: size_of::<OakSlider>(), flags: self.flags, min: self.min, max: self.max, step: self.step, width: self.width, decimals: self.decimals, suffix: s(self.suffix) };
        let mut out = OakResponse::default();
        next(&self.look);
        unsafe { (api().oak_slider)(ui.as_raw(), self.value, &sl, &mut out) };
        Response::of(out)
    }
}

impl Widget for Slider<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui)
    }
}

// ----------------------------------------------------------------- table

/// A table: its headings, then a closure draws each cell. Only the rows
/// in view are drawn.
#[must_use]
pub struct Table<'a> {
    look: Option<Look>,
    columns: &'a [&'a str],
    widths: &'a [f32],
    aligns: &'a [Align],
    rows: usize,
    row_height: f32,
    flags: u32,
    id: Option<Id>,
}

impl<'a> Table<'a> {
    pub fn new(columns: &'a [&'a str], rows: usize) -> Table<'a> {
        Table { look: None, columns, widths: &[], aligns: &[], rows, row_height: 0.0, flags: 0, id: None }
    }

    /// Each column's width; missing or 0: a share of what is left.
    pub fn widths(mut self, w: &'a [f32]) -> Self {
        self.widths = w;
        self
    }

    /// Each column's alignment (numbers at the right: `Align::Max`).
    pub fn aligns(mut self, a: &'a [Align]) -> Self {
        self.aligns = a;
        self
    }

    /// 28 by default.
    pub fn row_height(mut self, h: f32) -> Self {
        self.row_height = h;
        self
    }

    /// Rows lit under the pointer, and clicked.
    pub fn clickable(mut self) -> Self {
        self.flags |= OAK_TABLE_CLICK;
        self
    }

    pub fn no_head(mut self) -> Self {
        self.flags |= OAK_TABLE_NO_HEAD;
        self
    }

    pub fn id(mut self, id: impl Hash) -> Self {
        self.id = Some(Id::new(id));
        self
    }

    /// How it looks, over Oak's (`Look`).
    pub fn look(mut self, look: Look) -> Self {
        self.look = Some(look);
        self
    }

    /// Draws it, `cell(ui, row, column)` each cell. Returns the row
    /// clicked.
    pub fn show(self, ui: &mut Ui, mut cell: impl FnMut(&mut Ui, usize, usize)) -> Option<usize> {
        let n = self.columns.len();
        let columns: Vec<OakStr> = self.columns.iter().map(|c| s(c)).collect();
        let widths: Vec<f32> = (0..n).map(|i| self.widths.get(i).copied().unwrap_or(0.0)).collect();
        let aligns: Vec<u32> = (0..n).map(|i| align_code(self.aligns.get(i).copied().unwrap_or(Align::Min))).collect();
        let t = OakTable {
            size: size_of::<OakTable>(),
            flags: self.flags,
            id: id_of(self.id),
            columns: columns.as_ptr(),
            widths: widths.as_ptr(),
            aligns: aligns.as_ptr(),
            count: n,
            rows: self.rows,
            row_height: self.row_height,
        };
        let raw = ui.as_raw();
        let mut clicked = usize::MAX;
        next(&self.look);
        let flags = with_cell(&mut cell, |cell, user| unsafe { (api().oak_table)(raw, &t, cell, user, &mut clicked) });
        (flags & OAK_CLICKED != 0).then_some(clicked)
    }
}

// --------------------------------------------------------------- columns

/// `count` columns of equal width, `gap` apart (0: 12): `cell(ui, i)`
/// draws each.
pub fn columns(ui: &mut Ui, count: usize, gap: f32, mut cell: impl FnMut(&mut Ui, usize)) {
    let raw = ui.as_raw();
    let mut f = |ui: &mut Ui, _row: usize, column: usize| cell(ui, column);
    with_cell(&mut f, |cell, user| unsafe { (api().oak_columns)(raw, count, gap, cell, user) });
}

// ----------------------------------------------------------------- state

/// What a plugin keeps while Oak runs, in a `static`: made with `Default`
/// when first used.
///
/// ```ignore
/// static STATE: State<Counter> = State::new();
/// STATE.with(|s| s.clicks += 1);
/// ```
pub struct State<T>(Mutex<Option<T>>);

impl<T: Default> State<T> {
    pub const fn new() -> State<T> {
        State(Mutex::new(None))
    }

    /// `f` with the state. (Don't call `with` on the same state inside `f`.)
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(self.lock().get_or_insert_with(T::default))
    }

    /// The lock on it, for longer than a closure.
    pub fn lock(&self) -> MutexGuard<'_, Option<T>> {
        // (A panic while it was held left it as it was: still usable.)
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of it.
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(|v| v.clone())
    }

    pub fn set(&self, v: T) {
        *self.lock() = Some(v);
    }
}

impl<T: Default> Default for State<T> {
    fn default() -> Self {
        State::new()
    }
}

/// What a plugin keeps from one run of Oak to the next, in a `static`:
/// JSON under `key` in Oak's folder (`plugin-prefs.json`, by plugin id),
/// read when first used, written soon after each change.
///
/// ```ignore
/// #[derive(Default, Clone, Serialize, Deserialize)]
/// struct Prefs { size: f64, dark: bool }
/// static PREFS: Stored<Prefs> = Stored::new("prefs");
/// let mut p = PREFS.get();
/// if Toggle::new(&mut p.dark, "Dark").show(ui).changed() { PREFS.set(p) }
/// ```
pub struct Stored<T> {
    key: &'static str,
    value: Mutex<Option<T>>,
}

impl<T: Default + Clone + Serialize + DeserializeOwned> Stored<T> {
    pub const fn new(key: &'static str) -> Stored<T> {
        Stored { key, value: Mutex::new(None) }
    }

    fn lock(&self) -> MutexGuard<'_, Option<T>> {
        self.value.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn load(&self) -> T {
        if !has_api() {
            return T::default();
        }
        let id = crate::rt::plugin_id();
        let (found, json) = text_from(|out, user| unsafe { (api().prefs_get)(s(&id), s(self.key), out, user) });
        if found { serde_json::from_str(&json).unwrap_or_default() } else { T::default() }
    }

    /// `f` with the value; what it changed is kept (when it changed).
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: PartialEq,
    {
        let mut v = self.lock();
        let value = v.get_or_insert_with(|| self.load());
        let before = value.clone();
        let r = f(value);
        if *value != before {
            self.save(value);
        }
        r
    }

    /// A copy of it.
    pub fn get(&self) -> T {
        self.lock().get_or_insert_with(|| self.load()).clone()
    }

    /// Keeps `v`.
    pub fn set(&self, v: T) {
        self.save(&v);
        *self.lock() = Some(v);
    }

    fn save(&self, v: &T) {
        if !has_api() {
            return;
        }
        let id = crate::rt::plugin_id();
        if let Ok(json) = serde_json::to_string(v) {
            unsafe { (api().prefs_set)(s(&id), s(self.key), s(&json)) };
        }
    }
}

// ------------------------------------------------------------------- job

enum Run<T> {
    Idle,
    Busy,
    Done(Result<T, String>),
}

/// Work on a thread of its own (a download, a search), in a `static`:
/// started from a click, its result picked up while drawing. Oak draws
/// the window again when it ends.
///
/// ```ignore
/// static FETCH: Job<String> = Job::new();
/// if Action::primary("Fetch").busy(FETCH.busy()).show(ui).clicked() {
///     FETCH.start(|| fetch().map_err(|e| e.to_string()));
/// }
/// if let Some(r) = FETCH.take() { s.text = r.unwrap_or_else(|e| e); }
/// ```
pub struct Job<T>(Mutex<Run<T>>);

impl<T: Send + 'static> Job<T> {
    pub const fn new() -> Job<T> {
        Job(Mutex::new(Run::Idle))
    }

    fn lock(&self) -> MutexGuard<'_, Run<T>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts `work`, unless it is running already (false). A panic in it
    /// ends it with an error.
    pub fn start(&'static self, work: impl FnOnce() -> Result<T, String> + Send + 'static) -> bool {
        {
            let mut r = self.lock();
            if matches!(*r, Run::Busy) {
                return false;
            }
            *r = Run::Busy;
        }
        let name = format!("{} job", crate::rt::plugin_id());
        let started = std::thread::Builder::new().name(name).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|p| Err(crate::rt::panic_text(&*p)));
            *self.lock() = Run::Done(result);
            if has_api() {
                crate::host().repaint();
            }
        });
        if let Err(e) = started {
            *self.lock() = Run::Done(Err(e.to_string()));
        }
        true
    }

    /// Whether it is running.
    pub fn busy(&self) -> bool {
        matches!(*self.lock(), Run::Busy)
    }

    /// Its result, once it ended (then it is idle again).
    pub fn take(&self) -> Option<Result<T, String>> {
        let mut r = self.lock();
        match std::mem::replace(&mut *r, Run::Idle) {
            Run::Done(v) => Some(v),
            other => {
                *r = other;
                None
            }
        }
    }
}

impl<T: Send + 'static> Default for Job<T> {
    fn default() -> Self {
        Job::new()
    }
}
