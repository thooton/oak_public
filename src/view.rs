//! The parts a page is made of, in Oak's look: pages, headings, sections,
//! cards, buttons, badges, list rows, fields, switches, choices, progress,
//! notices, empty states, stats, sliders, tables, columns and toasts.
//! Plugins draw them through the interface (`oak_page`, `oak_action`, ...;
//! `oak_sdk::view` in Rust, `oak_view.h` in C), and Oak's own pages can use
//! them as they are.
//!
//! The look is Oak's: square corners, 1-point lines, the theme's panel
//! colors, the accent for what is chosen or matters most. What something
//! says by its color is a `Tone`. A `Look` changes it: for the next
//! component drawn (`look_next`), or for everything drawn in a scope
//! (`look_scope`). Each component reads both as it starts (`style`).

use crate::look;
use crate::theme::{Theme, hex, mix};
use crate::widgets::{self, clip_width, text_width};
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, Layout, Margin, Modifiers, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2};
use std::cell::{Cell, RefCell};
use std::sync::{Mutex, OnceLock};

/// What something says by its color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Plain,
    Accent,
    Good,
    Warn,
    Bad,
}

// (Settings' "connected" dot.)
const GOOD: u32 = 0x5fb36b;
const WARN: u32 = 0xd9a441;

/// The color `tone` is drawn in, in Oak's look.
pub fn tone_color(t: &Theme, tone: Tone) -> Color32 {
    match tone {
        Tone::Plain => t.ink3,
        Tone::Accent => t.accent,
        Tone::Good => hex(GOOD),
        Tone::Warn => hex(WARN),
        Tone::Bad => t.error,
    }
}

fn spinner(ui: &Ui, rect: Rect, color: Color32) {
    look::paint_spinner(ui.painter(), rect, color);
}

fn glyph_font(size: f32) -> FontId {
    look::icon_font(size)
}

fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

fn margin(x: f32, y: f32) -> Margin {
    let m = |v: f32| v.round().clamp(0.0, 127.0) as i8;
    Margin { left: m(x), right: m(x), top: m(y), bottom: m(y) }
}

// ------------------------------------------------------------------ look

/// How components look, over Oak's. Every part is optional: None keeps
/// what Oak would draw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Look {
    /// Its background, under the pointer, its outline, its text, its
    /// fainter text.
    pub fill: Option<Color32>,
    pub fill_hover: Option<Color32>,
    pub stroke: Option<Color32>,
    pub ink: Option<Color32>,
    pub ink_weak: Option<Color32>,
    /// What is chosen or matters most.
    pub accent: Option<Color32>,
    /// The tones' colors.
    pub good: Option<Color32>,
    pub warn: Option<Color32>,
    pub bad: Option<Color32>,
    pub radius: Option<f32>,
    pub stroke_width: Option<f32>,
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    pub height: Option<f32>,
    pub width: Option<f32>,
    /// Its main text's size (the rest follow it).
    pub text_points: Option<f32>,
    /// Between its parts, or after it.
    pub gap: Option<f32>,
    /// Its texts' sizes times this.
    pub text_scale: Option<f32>,
    /// Its texts in the code font.
    pub monospace: bool,
    /// No background or outline.
    pub no_frame: bool,
}

impl Look {
    /// What a scope passes on to what is drawn in it: colors, corners,
    /// outlines, the text's scale and font. (Sizes, room and frames are
    /// each component's own.)
    fn inherited(&self) -> Look {
        Look { padding_x: None, padding_y: None, height: None, width: None, text_points: None, gap: None, no_frame: false, ..*self }
    }

    /// `top` over this one: what `top` says wins; scales multiply.
    fn under(self, top: &Look) -> Look {
        Look {
            fill: top.fill.or(self.fill),
            fill_hover: top.fill_hover.or(self.fill_hover),
            stroke: top.stroke.or(self.stroke),
            ink: top.ink.or(self.ink),
            ink_weak: top.ink_weak.or(self.ink_weak),
            accent: top.accent.or(self.accent),
            good: top.good.or(self.good),
            warn: top.warn.or(self.warn),
            bad: top.bad.or(self.bad),
            radius: top.radius.or(self.radius),
            stroke_width: top.stroke_width.or(self.stroke_width),
            padding_x: top.padding_x.or(self.padding_x),
            padding_y: top.padding_y.or(self.padding_y),
            height: top.height.or(self.height),
            width: top.width.or(self.width),
            text_points: top.text_points.or(self.text_points),
            gap: top.gap.or(self.gap),
            text_scale: match (self.text_scale, top.text_scale) {
                (Some(a), Some(b)) => Some(a * b),
                (a, b) => b.or(a),
            },
            monospace: top.monospace || self.monospace,
            no_frame: top.no_frame || self.no_frame,
        }
    }

    /// `t` in this look's colors.
    fn paint(&self, t: &mut Theme) {
        if let Some(c) = self.ink {
            t.ink = c;
            t.ink2 = mix(t.bg, c, 0.8);
        }
        if let Some(c) = self.ink_weak {
            t.ink3 = c;
            t.ink4 = mix(t.bg, c, 0.65);
        }
        if let Some(c) = self.fill {
            t.panel = c;
            t.panel2 = mix(c, t.ink, 0.06);
        }
        if let Some(c) = self.fill_hover {
            t.panel2 = c;
            t.tool_hover = c;
        }
        if let Some(c) = self.stroke {
            t.line = c;
        }
        if let Some(c) = self.accent {
            t.accent = c;
        }
        if let Some(c) = self.bad {
            t.error = c;
        }
    }
}

thread_local! {
    /// The looks of the scopes being drawn, outermost first.
    static SCOPES: RefCell<Vec<Look>> = const { RefCell::new(Vec::new()) };
    /// The next component's.
    static NEXT: Cell<Option<Look>> = const { Cell::new(None) };
}

/// The next component drawn looks as `look` says (only it: not what is
/// drawn in it).
pub fn look_next(look: Option<Look>) {
    NEXT.with(|n| n.set(look));
}

/// What `f` draws looks as `look` says: its colors, corners, outlines and
/// text apply to every component in it (scopes in scopes add to it), and
/// egui's own widgets take its colors.
pub fn look_scope<R>(ui: &mut Ui, t: &Theme, look: &Look, f: impl FnOnce(&mut Ui) -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            SCOPES.with(|s| s.borrow_mut().pop());
        }
    }
    SCOPES.with(|s| s.borrow_mut().push(look.inherited()));
    let _pop = Pop;
    let s = scoped(t);
    ui.scope(|ui| {
        let mut v = crate::theme::visuals(&s.t);
        if let Some(r) = s.scope.radius {
            for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
                w.corner_radius = radius(r);
            }
        }
        if let Some(width) = s.scope.stroke_width {
            for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
                w.bg_stroke.width = width;
            }
        }
        ui.style_mut().visuals = v;
        if s.scale != 1.0 || s.mono {
            for font in ui.style_mut().text_styles.values_mut() {
                font.size *= s.scale;
                if s.mono {
                    *font = look::code_font(font.size);
                }
            }
        }
        f(ui)
    })
    .inner
}

/// Forgets a next look no component took (each frame).
pub fn begin_frame() {
    NEXT.with(|n| n.set(None));
}

/// What a component draws with: the theme in its scopes' and its own
/// colors, and its own look's sizes.
#[derive(Clone)]
pub struct Style {
    pub t: Theme,
    good: Color32,
    warn: Color32,
    /// The component's own look.
    own: Look,
    /// The scopes' and its own, for what scopes pass on.
    scope: Look,
    scale: f32,
    mono: bool,
}

/// The style a component starts with: `t` in the scopes' looks and the
/// next look (which it takes).
pub fn style(t: &Theme) -> Style {
    let own = NEXT.with(|n| n.take()).unwrap_or_default();
    resolve(t, own)
}

/// The style in the scopes alone (the next look stays for the next
/// component).
pub fn scoped(t: &Theme) -> Style {
    resolve(t, Look::default())
}

fn resolve(t: &Theme, own: Look) -> Style {
    let scopes = SCOPES.with(|s| s.borrow().iter().fold(Look::default(), |a, l| a.under(l)));
    let all = scopes.under(&own);
    let mut theme = t.clone();
    all.paint(&mut theme);
    Style {
        t: theme,
        good: all.good.unwrap_or(hex(GOOD)),
        warn: all.warn.unwrap_or(hex(WARN)),
        own,
        scope: all,
        scale: all.text_scale.filter(|s| *s > 0.0).unwrap_or(1.0),
        mono: all.monospace,
    }
}

impl Style {
    /// The same, but for its own look: `own` instead (a part passing its
    /// look on to the parts it is made of).
    fn with_own(&self, own: Look) -> Style {
        Style { own, ..self.clone() }
    }

    /// The same, its own sizes and frame left behind: for what is in it.
    fn inner(&self) -> Style {
        self.with_own(self.own.inherited())
    }

    pub fn tone(&self, tone: Tone) -> Color32 {
        match tone {
            Tone::Plain => self.t.ink3,
            Tone::Accent => self.t.accent,
            Tone::Good => self.good,
            Tone::Warn => self.warn,
            Tone::Bad => self.t.error,
        }
    }

    /// `tone`'s color faintly over the page: what a badge or a notice
    /// sits on.
    fn tint(&self, tone: Tone) -> Color32 {
        if tone == Tone::Plain { self.t.panel2 } else { mix(self.t.bg, self.tone(tone), 0.16) }
    }

    /// What sizes are multiplied by, when the main text's is `main`.
    fn k(&self, main: f32) -> f32 {
        self.own.text_points.filter(|p| *p > 0.0).map_or(1.0, |p| p / main) * self.scale
    }

    fn font(&self, size: f32) -> FontId {
        if self.mono { look::code_font(size) } else { FontId::proportional(size) }
    }

    fn radius(&self, default: f32) -> f32 {
        self.own.radius.or(self.scope.radius).unwrap_or(default)
    }

    fn stroke_w(&self, default: f32) -> f32 {
        self.own.stroke_width.or(self.scope.stroke_width).unwrap_or(default)
    }

    fn pad_x(&self, default: f32) -> f32 {
        self.own.padding_x.unwrap_or(default)
    }

    fn pad_y(&self, default: f32) -> f32 {
        self.own.padding_y.unwrap_or(default)
    }

    fn height(&self, default: f32) -> f32 {
        self.own.height.unwrap_or(default)
    }

    fn width(&self, default: f32) -> f32 {
        self.own.width.unwrap_or(default)
    }

    fn gap(&self, default: f32) -> f32 {
        self.own.gap.unwrap_or(default)
    }

    fn no_frame(&self) -> bool {
        self.own.no_frame
    }

    /// Its own frame's parts: what a section or a stat gives its card.
    fn frame_look(&self) -> Look {
        let o = &self.own;
        Look { fill: o.fill, fill_hover: o.fill_hover, stroke: o.stroke, radius: o.radius, stroke_width: o.stroke_width, padding_x: o.padding_x, padding_y: o.padding_y, width: o.width, no_frame: o.no_frame, ..o.inherited() }
    }
}

// ------------------------------------------------------------------ page

/// A page's column: at most this wide.
pub const PAGE_W: f32 = 760.0;

#[derive(Clone, Copy, Default)]
pub struct PageOpts<'a> {
    /// Where the scroll position is kept.
    pub id: Option<Id>,
    pub title: &'a str,
    pub subtitle: &'a str,
    /// A link above the title; it and Escape go back.
    pub back: &'a str,
    /// 0: `PAGE_W`.
    pub width: f32,
    /// What it draws scrolls itself, or fits.
    pub no_scroll: bool,
}

/// A page: `body` in a column in the middle, under the title, all in a
/// scroll area. `actions` draws at the title's right end, right to left.
/// Returns whether its back link was pressed (or Escape, when it has one).
///
/// Its look: `width` the column's, `padding_y` the room above,
/// `text_points` the title's size, `gap` the room under the header.
pub fn page(ui: &mut Ui, t: &Theme, o: &PageOpts, actions: Option<&mut dyn FnMut(&mut Ui)>, body: &mut dyn FnMut(&mut Ui)) -> bool {
    let s = style(t);
    let k = s.k(20.0);
    let inner = s.inner();
    let mut back = false;
    let mut actions = actions;
    let mut column = |ui: &mut Ui| {
        let w = s.width(if o.width > 0.0 { o.width } else { PAGE_W });
        widgets::column(ui, w, |ui| {
            ui.add_space(s.pad_y(20.0));
            if !o.back.is_empty() {
                let r = action_s(ui, &inner, &ActionOpts { kind: Kind::Link, text: &format!("← {}", o.back), small: true, ..Default::default() });
                back |= r.clicked();
                ui.add_space(2.0);
            }
            if !o.title.is_empty() || actions.is_some() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(o.title).font(s.font(20.0 * k)).color(s.t.ink));
                    if let Some(a) = actions.as_mut() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| a(ui));
                    }
                });
            }
            if !o.subtitle.is_empty() {
                ui.add(egui::Label::new(RichText::new(o.subtitle).font(s.font(13.0 * k)).color(s.t.ink3)).wrap());
            }
            if !o.title.is_empty() || !o.subtitle.is_empty() {
                ui.add_space(s.gap(14.0));
            }
            body(ui);
            ui.add_space(28.0);
        });
    };
    if o.no_scroll {
        column(ui);
    } else {
        let id = o.id.unwrap_or_else(|| ui.scope_id().with(("page", o.title)));
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| column(ui));
    }
    if !o.back.is_empty() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        back = true;
    }
    back
}

// --------------------------------------------------------------- heading

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    /// 14.5 points: a section's.
    #[default]
    Section,
    /// 20 points: a page's.
    Title,
    /// A small caption's.
    Small,
}

#[derive(Clone, Copy, Default)]
pub struct HeadingOpts<'a> {
    pub level: Level,
    pub text: &'a str,
    /// Faint, after the text.
    pub detail: &'a str,
    /// A Nerd Font symbol before it.
    pub icon: &'a str,
    /// A link at the right end.
    pub action: &'a str,
    /// A line under it.
    pub rule: bool,
    /// A spinner after it.
    pub busy: bool,
}

/// A heading; `right` draws at its right end, before its action. Returns
/// its row, and whether its action was pressed.
///
/// Its look: `text_points` its size, `height` its row's, `ink` its text,
/// `stroke` and `stroke_width` its rule.
pub fn heading(ui: &mut Ui, t: &Theme, o: &HeadingOpts, right: Option<&mut dyn FnMut(&mut Ui)>) -> (egui::Response, bool) {
    heading_row(ui, &style(t), o, None, right)
}

/// A heading; with `fold`, a chevron before it says whether what it heads
/// is open, and the row takes clicks.
fn heading_row(ui: &mut Ui, s: &Style, o: &HeadingOpts, fold: Option<bool>, right: Option<&mut dyn FnMut(&mut Ui)>) -> (egui::Response, bool) {
    let t = &s.t;
    let (size, h, color) = match o.level {
        Level::Title => (20.0, 32.0, t.ink),
        Level::Section => (14.5, 26.0, t.ink),
        Level::Small => (11.5, 20.0, t.ink3),
    };
    let k = s.k(size);
    let w = ui.available_width();
    let h = s.height(h * k.max(1.0));
    let (r, resp) = ui.allocate_exact_size(vec2(w, h), if fold.is_some() { Sense::click() } else { Sense::hover() });
    let lit = fold.is_some() && resp.hovered();
    let p = ui.painter().clone();
    let y = r.center().y;
    let mut x = r.left();
    if let Some(open) = fold {
        widgets::chevron(&p, pos2(x + 4.0, y), open, if lit { t.ink } else { t.ink3 });
        x += 16.0;
    }
    if !o.icon.is_empty() {
        let g = p.text(pos2(x, y), Align2::LEFT_CENTER, o.icon, glyph_font((size + 1.0) * k), if o.level == Level::Small { t.ink3 } else { t.ink2 });
        x = g.right() + 7.0;
    }
    // The action's room is kept first, then the right end's: the title is
    // cut short before them.
    let small = s.font(11.5 * k);
    let action_w = if o.action.is_empty() { 0.0 } else { text_width(ui, o.action, &small) + 12.0 };
    let mut end = r.right() - action_w;
    if let Some(right) = right {
        let area = Rect::from_min_max(pos2(x, r.top()), pos2(end, r.bottom()));
        let mut child = ui.new_child(UiBuilder::new().max_rect(area).layout(Layout::right_to_left(Align::Center)));
        right(&mut child);
        let used = child.min_rect();
        if used.width() > 0.0 {
            end = used.left() - 8.0;
        }
    }
    let font = s.font(size * k);
    let title = clip_width(ui, o.text, &font, (end - x).max(20.0));
    let g = p.text(pos2(x, y), Align2::LEFT_CENTER, title, font, if lit { t.ink } else { color });
    x = g.right() + 8.0;
    if !o.detail.is_empty() {
        let g = p.text(pos2(x, y + 0.5), Align2::LEFT_CENTER, o.detail, small.clone(), t.ink3);
        x = g.right() + 8.0;
    }
    if o.busy {
        spinner(ui, Rect::from_center_size(pos2(x + 6.0, y), vec2(13.0, 13.0)), t.ink3);
    }
    let mut pressed = false;
    if !o.action.is_empty() {
        let at = Rect::from_min_max(pos2(r.right() - action_w, r.top()), r.max);
        let a = ui.interact(at, resp.id.with("action"), Sense::click());
        let lit = a.hovered();
        let g = p.text(pos2(r.right(), y), Align2::RIGHT_CENTER, o.action, small, t.accent);
        if lit {
            p.hline(g.x_range(), g.bottom(), Stroke::new(1.0, t.accent));
        }
        pressed = a.on_hover_cursor(CursorIcon::PointingHand).clicked();
    }
    if o.rule {
        p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(s.stroke_w(1.0), t.line));
    }
    let resp = if fold.is_some() { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    (resp, pressed)
}

// --------------------------------------------------------------- section

#[derive(Clone, Copy, Default)]
pub struct SectionOpts<'a> {
    /// Where whether it is open is kept.
    pub id: Option<Id>,
    pub title: &'a str,
    pub detail: &'a str,
    pub icon: &'a str,
    pub action: &'a str,
    /// What it holds is in a card.
    pub card: bool,
    /// Its heading opens and closes it.
    pub folds: bool,
    /// Closed until it is opened (with `folds`).
    pub closed: bool,
    pub busy: bool,
}

/// A section: a heading, `body` under it (in a card, with `card`), and
/// room after; `right` draws at the heading's right end. Returns its
/// heading's row, whether it is open (`body` was drawn), and whether its
/// action was pressed.
///
/// Its look: its heading's (`text_points`, `ink`, `height`), its card's
/// (`fill`, `fill_hover`, `stroke`, `stroke_width`, `radius`, `padding`,
/// `width`, `no_frame`), and `gap` the room after it.
pub fn section(ui: &mut Ui, t: &Theme, o: &SectionOpts, right: Option<&mut dyn FnMut(&mut Ui)>, body: &mut dyn FnMut(&mut Ui)) -> (egui::Response, bool, bool) {
    let s = style(t);
    let id = o.id.unwrap_or_else(|| ui.scope_id().with(("section", o.title)));
    let mut open = !o.folds || widgets::is_open(ui, id, !o.closed);
    let h = HeadingOpts { level: Level::Section, text: o.title, detail: o.detail, icon: o.icon, action: o.action, rule: !o.card, busy: o.busy };
    let head = s.with_own(Look { text_points: s.own.text_points, height: s.own.height, ..s.own.inherited() });
    let (resp, pressed) = heading_row(ui, &head, &h, o.folds.then_some(open), right);
    if o.folds && resp.clicked() && !pressed {
        open = !open;
        widgets::set_open(ui, id, open);
    }
    if open {
        ui.add_space(4.0);
        if o.card {
            card_s(ui, &s.with_own(s.frame_look()), &CardOpts::default(), body);
        } else {
            body(ui);
        }
    }
    ui.add_space(s.gap(14.0));
    (resp, open, pressed)
}

// ------------------------------------------------------------------ card

#[derive(Clone, Copy, Default)]
pub struct CardOpts {
    pub id: Option<Id>,
    /// None: the theme's panel color.
    pub fill: Option<Color32>,
    /// Inside the border; 0: 12.
    pub padding: f32,
    /// 0: all there is.
    pub width: f32,
    /// Lit under the pointer, and clicked as a whole.
    pub click: bool,
    /// Marked chosen.
    pub selected: bool,
}

/// `body` in a bordered box. Clicks on the widgets in it are theirs; with
/// `click`, a click elsewhere in it is the card's.
///
/// Its look: `fill`, `fill_hover`, `stroke`, `stroke_width`, `radius`,
/// `padding_x`, `padding_y`, `width`, `no_frame`.
pub fn card(ui: &mut Ui, t: &Theme, o: &CardOpts, body: &mut dyn FnMut(&mut Ui)) -> egui::Response {
    card_s(ui, &style(t), o, body)
}

fn card_s(ui: &mut Ui, s: &Style, o: &CardOpts, body: &mut dyn FnMut(&mut Ui)) -> egui::Response {
    let t = &s.t;
    let pad = if o.padding > 0.0 { o.padding } else { 12.0 };
    let (px, py) = (s.pad_x(pad), s.pad_y(pad));
    let bare = s.no_frame();
    let fill = s.own.fill.or(o.fill).unwrap_or(if bare { Color32::TRANSPARENT } else { t.panel });
    let line = if bare { Color32::TRANSPARENT } else { t.line };
    let width = s.width(o.width);
    let stroke_w = s.stroke_w(1.0);
    let mut b = UiBuilder::new().sense(if o.click { Sense::click() } else { Sense::hover() });
    if let Some(id) = o.id {
        b = b.id_salt(id);
    }
    let r = ui.scope_builder(b, |ui| {
        // (The response is last frame's: what the pointer is over now.)
        let lit = o.click && ui.response().contains_pointer();
        let mut frame = egui::Frame::new().fill(fill).stroke(Stroke::new(stroke_w, line)).corner_radius(radius(s.radius(0.0))).inner_margin(margin(px, py)).begin(ui);
        {
            let inner = &mut frame.content_ui;
            if o.click {
                // (Its text is part of what is clicked, not for selecting.)
                inner.style_mut().interaction.selectable_labels = false;
            }
            let room = inner.available_width();
            let w = if width > 0.0 { (width - 2.0 * px - 2.0 * stroke_w).min(room) } else { room };
            inner.set_width(w.max(0.0));
            body(inner);
        }
        let hover = s.own.fill_hover.unwrap_or_else(|| mix(if bare { t.bg } else { fill }, t.ink, 0.04));
        frame.frame.fill = if o.selected { mix(if bare { t.bg } else { fill }, t.accent, 0.08) } else if lit { hover } else { fill };
        frame.frame.stroke = Stroke::new(stroke_w, if o.selected { t.accent } else if lit && !bare { t.ink3 } else { line });
        frame.end(ui);
    });
    if o.click { r.response.on_hover_cursor(CursorIcon::PointingHand) } else { r.response }
}

// ---------------------------------------------------------------- action

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// Framed.
    #[default]
    Default,
    /// Filled with the accent: the page's main action.
    Primary,
    /// Frameless until the pointer is over it.
    Quiet,
    /// In the error color.
    Danger,
    Link,
    /// A rounded chip, lit when selected.
    Pill,
    /// A square with a symbol.
    Icon,
}

#[derive(Clone, Copy, Default)]
pub struct ActionOpts<'a> {
    pub kind: Kind,
    pub text: &'a str,
    /// A Nerd Font symbol before the text.
    pub icon: &'a str,
    pub tip: &'a str,
    pub min_width: f32,
    pub disabled: bool,
    /// A spinner, and it can't be pressed.
    pub busy: bool,
    pub selected: bool,
    pub small: bool,
    /// As wide as there is room.
    pub fill: bool,
    /// 0: as its kind and size say.
    pub height: f32,
}

/// A button.
///
/// Its look: `fill`, `fill_hover`, `stroke`, `ink` (over its kind's),
/// `stroke_width`, `radius`, `padding_x`, `height`, `width` (exactly),
/// `text_points`, `monospace`, `no_frame`.
pub fn action(ui: &mut Ui, t: &Theme, o: &ActionOpts) -> egui::Response {
    action_s(ui, &style(t), o)
}

fn action_s(ui: &mut Ui, s: &Style, o: &ActionOpts) -> egui::Response {
    let t = &s.t;
    let base = if o.small { 12.0 } else { 13.5 };
    let k = s.k(base);
    let font = s.font(base * k);
    let gfont = glyph_font((if o.small { 13.0 } else { 15.0 }) * k);
    let base_h = (if o.small { 22.0_f32 } else { 28.0 }).max(font.size + 12.0);
    let h = if o.height > 0.0 {
        o.height
    } else {
        s.height(match o.kind {
            Kind::Link => font.size + 6.0,
            Kind::Pill => base_h - 2.0,
            _ => base_h,
        })
    };
    let pad = s.pad_x(match o.kind {
        Kind::Link => 0.0,
        Kind::Icon => 0.0,
        Kind::Pill => 11.0,
        _ if o.small => 9.0,
        _ => 13.0,
    });
    let text_w = if o.text.is_empty() { 0.0 } else { text_width(ui, o.text, &font) };
    let lead = if o.busy { 13.0 } else if o.icon.is_empty() { 0.0 } else { text_width(ui, o.icon, &gfont) };
    let gap = if lead > 0.0 && text_w > 0.0 { 6.0 } else { 0.0 };
    let content = lead + gap + text_w;
    let mut w = if o.kind == Kind::Icon && o.text.is_empty() { h.max(content + 2.0 * pad) } else { content + 2.0 * pad };
    w = w.max(o.min_width);
    if o.fill {
        w = w.max(ui.available_width());
    }
    if let Some(exact) = s.own.width {
        w = exact;
    }
    let live = !o.disabled && !o.busy;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), if live { Sense::click() } else { Sense::hover() });
    let hover = live && resp.hovered();
    let down = live && resp.is_pointer_button_down_on();
    let none = Color32::TRANSPARENT;
    // Fill, outline, ink, corner radius, as its kind has them.
    let (mut fill, mut stroke, ink, round) = match o.kind {
        Kind::Default if o.selected => (mix(t.bg, t.accent, 0.16), t.accent, t.ink, 0.0),
        Kind::Default => (if hover { t.panel2 } else { t.panel }, if hover { t.ink3 } else { t.line }, t.ink, 0.0),
        Kind::Primary if !live => (t.panel2, none, t.ink4, 0.0),
        Kind::Primary => (if down { mix(t.accent, Color32::BLACK, 0.12) } else if hover { mix(t.accent, Color32::WHITE, 0.12) } else { t.accent }, none, t.bg, 0.0),
        Kind::Quiet => (if hover { t.panel2 } else { none }, none, if o.selected { t.accent } else if hover { t.ink } else { t.ink2 }, 0.0),
        Kind::Danger => (if hover { mix(t.bg, t.error, 0.14) } else { t.panel }, if hover { t.error } else { t.line }, t.error, 0.0),
        Kind::Link => (none, none, t.accent, 0.0),
        Kind::Pill if o.selected => (mix(t.bg, t.accent, 0.2), t.accent.gamma_multiply(0.7), t.ink, h / 2.0),
        Kind::Pill => (if hover { t.panel2 } else { t.panel }, if hover { t.ink3 } else { t.line }, t.ink2, h / 2.0),
        Kind::Icon => (if hover || down { t.tool_hover } else { none }, none, if o.selected { t.accent } else if hover { t.ink } else { t.ink2 }, 0.0),
    };
    // Its own look's colors over its kind's.
    let own = &s.own;
    if let Some(f) = own.fill {
        fill = if hover || down { own.fill_hover.unwrap_or_else(|| mix(f, t.ink, 0.12)) } else { f };
    } else if let (Some(f), true) = (own.fill_hover, hover) {
        fill = f;
    }
    if let Some(c) = own.stroke {
        stroke = c;
    }
    let ink = if !live && o.kind != Kind::Primary { t.ink4 } else { own.ink.unwrap_or(ink) };
    if s.no_frame() {
        fill = if hover { own.fill_hover.unwrap_or(t.tool_hover) } else { none };
        stroke = none;
    }
    let p = ui.painter();
    if fill != none || stroke != none {
        p.rect(rect, s.radius(round), fill, Stroke::new(if stroke == none { 0.0 } else { s.stroke_w(1.0) }, stroke), StrokeKind::Inside);
    }
    let mut x = if o.kind == Kind::Link { rect.left() + pad } else { rect.center().x - content / 2.0 };
    let y = rect.center().y;
    if o.busy {
        spinner(ui, Rect::from_min_size(pos2(x, y - 6.5), vec2(13.0, 13.0)), ink);
        x += lead + gap;
    } else if !o.icon.is_empty() {
        p.text(pos2(x, y), Align2::LEFT_CENTER, o.icon, gfont, ink);
        x += lead + gap;
    }
    if !o.text.is_empty() {
        let g = p.text(pos2(x, y), Align2::LEFT_CENTER, o.text, font, ink);
        if o.kind == Kind::Link && hover {
            p.hline(g.x_range(), g.bottom(), Stroke::new(1.0, ink));
        }
    }
    let resp = if hover { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    if o.tip.is_empty() { resp } else { resp.on_hover_text(o.tip) }
}

// ----------------------------------------------------------------- badge

/// A small tag in `tone`'s color.
///
/// Its look: `fill`, `ink`, `stroke` (an outline), `radius`, `padding_x`,
/// `height`, `text_points`, `no_frame`.
pub fn badge(ui: &mut Ui, t: &Theme, text: &str, tone: Tone) -> egui::Response {
    let s = style(t);
    let k = s.k(11.0);
    let font = s.font(11.0 * k);
    let w = text_width(ui, text, &font) + 2.0 * s.pad_x(6.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, s.height(18.0 * k.max(1.0))), Sense::hover());
    let ink = s.own.ink.unwrap_or(if tone == Tone::Plain { s.t.ink2 } else { s.tone(tone) });
    let r = s.radius(2.0);
    if !s.no_frame() {
        ui.painter().rect_filled(rect, r, s.own.fill.unwrap_or_else(|| s.tint(tone)));
        if let Some(c) = s.own.stroke {
            ui.painter().rect_stroke(rect, r, Stroke::new(s.stroke_w(1.0), c), StrokeKind::Inside);
        }
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, ink);
    resp
}

// ------------------------------------------------------------------ item

#[derive(Clone, Copy, Default)]
pub struct ItemOpts<'a> {
    pub id: Option<Id>,
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Faint, at the right end.
    pub trailing: &'a str,
    /// A Nerd Font symbol before the title.
    pub icon: &'a str,
    /// A dot before the title, in this tone's color.
    pub dot: Option<Tone>,
    /// A picture before the title.
    pub texture: Option<egui::TextureId>,
    /// 0: as its texts need.
    pub height: f32,
    pub selected: bool,
    pub disabled: bool,
    pub busy: bool,
    /// No line under it.
    pub no_line: bool,
}

/// A row of a list, as wide as there is room. `left` draws at its left
/// end, in place of the dot, picture and symbol; `right` at its right end,
/// right to left: what they draw takes its own clicks.
///
/// Its look: `fill` (its background), `fill_hover`, `stroke` and
/// `stroke_width` (the line under it), `ink` (the title), `ink_weak`,
/// `radius`, `padding_x`, `height`, `text_points`, `no_frame` (no line).
pub fn item(ui: &mut Ui, t: &Theme, o: &ItemOpts, left: Option<&mut dyn FnMut(&mut Ui)>, right: Option<&mut dyn FnMut(&mut Ui)>) -> egui::Response {
    let s = style(t);
    let t = &s.t;
    let k = s.k(14.0);
    let h = if o.height > 0.0 { o.height } else { s.height((if o.subtitle.is_empty() { 34.0 } else { 48.0 }) * k.max(1.0)) };
    let sense = if o.disabled { Sense::hover() } else { Sense::click() };
    let size = vec2(ui.available_width(), h);
    let (rect, resp) = match o.id {
        Some(id) => {
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            (rect, ui.interact(rect, id, sense))
        }
        None => ui.allocate_exact_size(size, sense),
    };
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let lit = !o.disabled && resp.hovered();
    let p = ui.painter().clone();
    let round = s.radius(0.0);
    if o.selected {
        p.rect_filled(rect, round, mix(s.own.fill.unwrap_or(t.bg), t.accent, 0.10));
        p.rect_filled(Rect::from_min_size(rect.min, vec2(2.0, h)), 0.0, t.accent);
    } else if lit {
        p.rect_filled(rect, round, s.own.fill_hover.unwrap_or(t.tool_hover));
    } else if let Some(f) = s.own.fill {
        p.rect_filled(rect, round, f);
    }
    if !o.no_line && !s.no_frame() {
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(s.stroke_w(1.0), s.own.stroke.unwrap_or(t.line.gamma_multiply(0.6))));
    }
    let pad = s.pad_x(10.0);
    let y = rect.center().y;
    let mut x = rect.left() + pad;
    let mut rx = rect.right() - pad;
    if let Some(left) = left {
        let area = Rect::from_min_max(pos2(x, rect.top()), pos2(rx, rect.bottom()));
        let mut child = ui.new_child(UiBuilder::new().max_rect(area).layout(Layout::left_to_right(Align::Center)));
        left(&mut child);
        let used = child.min_rect();
        if used.width() > 0.0 {
            x = used.right() + 10.0;
        }
    } else {
        if let Some(tone) = o.dot {
            p.circle_filled(pos2(x + 4.0, y), 4.0, s.tone(tone));
            x += 18.0;
        }
        if let Some(tex) = o.texture {
            let side = h - 12.0;
            let r = Rect::from_min_size(pos2(x, y - side / 2.0), vec2(side, side));
            p.image(tex, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            x += side + 10.0;
        }
        if !o.icon.is_empty() {
            let g = p.text(pos2(x, y), Align2::LEFT_CENTER, o.icon, glyph_font(16.0 * k), if o.disabled { t.ink4 } else { t.ink2 });
            x = g.right().max(x + 16.0 * k) + 10.0;
        }
    }
    if o.busy {
        spinner(ui, Rect::from_center_size(pos2(rx - 7.0, y), vec2(14.0, 14.0)), t.ink3);
        rx -= 24.0;
    }
    if let Some(right) = right {
        let area = Rect::from_min_max(pos2(x, rect.top()), pos2(rx, rect.bottom()));
        let mut child = ui.new_child(UiBuilder::new().max_rect(area).layout(Layout::right_to_left(Align::Center)));
        right(&mut child);
        let used = child.min_rect();
        if used.width() > 0.0 {
            rx = used.left() - 10.0;
        }
    }
    let small = s.font(12.0 * k);
    if !o.trailing.is_empty() {
        let g = p.text(pos2(rx, y), Align2::RIGHT_CENTER, o.trailing, small.clone(), t.ink3);
        rx = g.left() - 12.0;
    }
    let room = (rx - x).max(20.0);
    let title_font = s.font(14.0 * k);
    let title = clip_width(ui, o.title, &title_font, room);
    let ink = if o.disabled { t.ink3 } else { s.own.ink.unwrap_or_else(|| look::row_title_color(t, lit || o.selected)) };
    if o.subtitle.is_empty() {
        p.text(pos2(x, y), Align2::LEFT_CENTER, title, title_font, ink);
    } else {
        p.text(pos2(x, y - 1.0), Align2::LEFT_BOTTOM, title, title_font, ink);
        p.text(pos2(x, y + 2.0), Align2::LEFT_TOP, clip_width(ui, o.subtitle, &small, room), small, t.ink3);
    }
    if lit { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp }
}

// ----------------------------------------------------------------- field

#[derive(Clone, Copy, Default)]
pub struct FieldOpts<'a> {
    pub id: Option<Id>,
    /// Over the field.
    pub label: &'a str,
    /// In it while it is empty.
    pub hint: &'a str,
    /// Under it.
    pub help: &'a str,
    /// Under it in the error color, instead of `help`.
    pub error: &'a str,
    /// A button at its right: it and Enter confirm. Off while the field is
    /// empty (unless `empty_ok`).
    pub button: &'a str,
    /// 0: all there is.
    pub width: f32,
    /// The rows of a `multiline` one; 0: 4.
    pub rows: u32,
    pub secret: bool,
    pub multiline: bool,
    pub disabled: bool,
    /// A spinner by the button, which is off.
    pub busy: bool,
    /// Takes the keyboard.
    pub focus: bool,
    pub empty_ok: bool,
}

/// A text field with its label, help or error, and button. Returns the
/// field's response and whether it was confirmed (Enter, or the button).
///
/// Its look (the field's): `fill`, `stroke`, `stroke_width`, `ink`,
/// `radius`, `padding_x`, `padding_y`, `height`, `width`, `text_points`,
/// `monospace`; `ink_weak` its label and help.
pub fn field(ui: &mut Ui, t: &Theme, o: &FieldOpts, text: &mut String) -> (egui::Response, bool) {
    let s = style(t);
    let inner = s.inner();
    let t = &s.t;
    let k = s.k(14.0);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        if !o.label.is_empty() {
            ui.label(RichText::new(o.label).font(s.font(11.5 * k)).color(s.own.ink_weak.unwrap_or(t.ink2)));
        }
        let w = s.width(if o.width > 0.0 { o.width } else { 0.0 });
        let w = if w > 0.0 { w.min(ui.available_width()) } else { ui.available_width() };
        let can = !o.disabled && !o.busy && (o.empty_ok || !text.trim().is_empty());
        let font = s.font(13.5 * k);
        let button_w = if o.button.is_empty() { 0.0 } else { text_width(ui, o.button, &font) + 26.0 + 6.0 + if o.busy { 22.0 } else { 0.0 } };
        let mut confirmed = false;
        let edit = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let edit_w = (w - button_w).max(60.0);
                let r = ui
                    .scope(|ui| {
                        let v = ui.visuals_mut();
                        if let Some(f) = s.own.fill {
                            v.extreme_bg_color = f;
                        }
                        let stroke_w = s.stroke_w(1.0);
                        for (wv, c) in [(&mut v.widgets.inactive, s.own.stroke), (&mut v.widgets.hovered, s.own.stroke), (&mut v.widgets.active, None)] {
                            if let Some(c) = c {
                                wv.bg_stroke.color = c;
                            }
                            wv.bg_stroke.width = stroke_w;
                            wv.corner_radius = radius(s.radius(0.0));
                        }
                        v.selection.stroke.width = stroke_w;
                        if let Some(c) = s.own.ink {
                            v.override_text_color = Some(c);
                        }
                        let mut e = if o.multiline { TextEdit::multiline(text).desired_rows(if o.rows > 0 { o.rows as usize } else { 4 }) } else { TextEdit::singleline(text) };
                        if let Some(id) = o.id {
                            e = e.id(id);
                        }
                        if !o.hint.is_empty() {
                            e = e.hint_text(RichText::new(o.hint).color(t.ink4));
                        }
                        e = e.font(s.font(14.0 * k)).interactive(!o.disabled).margin(margin(s.pad_x(6.0), s.pad_y(5.0)));
                        if let Some(h) = s.own.height {
                            e = e.min_size(vec2(0.0, h));
                        }
                        if o.secret {
                            let salt = format!("oak-field-{}", o.id.map_or(0, |i| i.value()));
                            widgets::secret_edit(ui, t, &salt, e, false, Some(edit_w))
                        } else {
                            crate::text_menu::add(ui, e.desired_width(edit_w))
                        }
                    })
                    .inner;
                if o.focus {
                    r.request_focus();
                }
                let enter = !o.multiline && widgets::left_with(ui, &r, Key::Enter);
                if !o.button.is_empty() {
                    let b = action_s(ui, &inner, &ActionOpts { text: o.button, disabled: !can, height: r.rect.height(), ..Default::default() });
                    confirmed |= b.clicked();
                    if o.busy {
                        ui.spinner();
                    }
                }
                if enter && can {
                    confirmed = true;
                    // (Ready for the next.)
                    r.request_focus();
                }
                r
            })
            .inner;
        if !o.error.is_empty() {
            ui.add(egui::Label::new(RichText::new(o.error).font(s.font(11.5 * k)).color(t.error)).wrap());
        } else if !o.help.is_empty() {
            ui.add(egui::Label::new(RichText::new(o.help).font(s.font(11.5 * k)).color(s.own.ink_weak.unwrap_or(t.ink3))).wrap());
        }
        (edit, confirmed)
    })
    .inner
}

// ---------------------------------------------------------------- toggle

/// A row with `label`, `help` under it and a switch at its right end: a
/// click anywhere on it flips `on` (the response is then changed).
///
/// Its look: `ink`, `ink_weak`, `accent` (the switch), `fill_hover` (the
/// row under the pointer), `padding_y`, `height`, `text_points`.
pub fn toggle(ui: &mut Ui, t: &Theme, on: &mut bool, label: &str, help: &str, disabled: bool) -> egui::Response {
    let s = style(t);
    let t = &s.t;
    let k = s.k(14.0);
    let w = ui.available_width();
    let switch = vec2(34.0, 18.0);
    let wrap = (w - switch.x - 16.0).max(60.0);
    let ink = if disabled { t.ink3 } else { t.ink };
    let title = ui.painter().layout(label.to_string(), s.font(14.0 * k), ink, wrap);
    let note = (!help.is_empty()).then(|| ui.painter().layout(help.to_string(), s.font(12.0 * k), t.ink3, wrap));
    let text_h = title.size().y + note.as_ref().map_or(0.0, |n| n.size().y + 2.0);
    let h = s.height((text_h + 2.0 * s.pad_y(5.0)).max(30.0));
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, h), if disabled { Sense::hover() } else { Sense::click() });
    let lit = !disabled && resp.hovered();
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let p = ui.painter();
    if let (true, Some(f)) = (lit, s.own.fill_hover) {
        p.rect_filled(rect, s.radius(0.0), f);
    }
    let top = rect.center().y - text_h / 2.0;
    let title_h = title.size().y;
    p.galley(pos2(rect.left(), top), title, ink);
    if let Some(n) = note {
        p.galley(pos2(rect.left(), top + title_h + 2.0), n, t.ink3);
    }
    let sw = Rect::from_min_size(pos2(rect.right() - switch.x, rect.center().y - switch.y / 2.0), switch);
    widgets::paint_switch(p, t, sw, *on, lit);
    if lit { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp }
}

// --------------------------------------------------------------- setting

/// A setting's row: `label` and `help` at the left, what `control` draws
/// at the right; one under the other when there is little room.
///
/// Its look: `ink`, `ink_weak`, `text_points`, `width` (the left side's),
/// `gap` (the room after it).
pub fn setting(ui: &mut Ui, t: &Theme, label: &str, help: &str, control: Option<&mut dyn FnMut(&mut Ui)>) -> egui::Response {
    let s = style(t);
    let k = s.k(14.0);
    let w = ui.available_width();
    let after = s.gap(8.0);
    let texts = |ui: &mut Ui, wrap: f32| {
        ui.set_max_width(wrap);
        ui.label(RichText::new(label).font(s.font(14.0 * k)).color(s.t.ink));
        if !help.is_empty() {
            ui.add(egui::Label::new(RichText::new(help).font(s.font(12.0 * k)).color(s.t.ink3)).wrap());
        }
    };
    if w < 440.0 {
        return ui
            .vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                texts(ui, w);
                if let Some(c) = control {
                    ui.add_space(3.0);
                    c(ui);
                }
                ui.add_space(after);
            })
            .response;
    }
    let left_w = s.width((w * 0.42).clamp(160.0, 300.0)).min(w - 60.0);
    let top = ui.cursor().min;
    let left = Rect::from_min_size(top, vec2(left_w, f32::INFINITY));
    let right = Rect::from_min_max(pos2(top.x + left_w + 20.0, top.y), pos2(top.x + w, f32::INFINITY));
    let mut l = ui.new_child(UiBuilder::new().max_rect(left).layout(Layout::top_down(Align::Min)));
    l.spacing_mut().item_spacing.y = 3.0;
    texts(&mut l, left_w);
    let mut bottom = l.min_rect().bottom();
    if let Some(c) = control {
        let mut r = ui.new_child(UiBuilder::new().max_rect(right).layout(Layout::top_down(Align::Min)));
        c(&mut r);
        bottom = bottom.max(r.min_rect().bottom());
    }
    let resp = ui.allocate_rect(Rect::from_min_max(top, pos2(top.x + w, bottom)), Sense::hover());
    ui.add_space(after);
    resp
}

// ---------------------------------------------------------------- choice

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChoiceStyle {
    /// Joined boxes.
    #[default]
    Segmented,
    /// Tabs over a line.
    Tabs,
    /// Chips.
    Pills,
    Dropdown,
    /// A column of rows, each with a mark.
    List,
}

/// One of `options`: `selected` is its index (the response is changed
/// when it was changed). `width` is a drop-down's (0: as its texts need).
///
/// Its look: `fill`, `fill_hover`, `stroke`, `stroke_width`, `ink`,
/// `accent`, `radius`, `padding_x`, `height`, `width` (a drop-down's),
/// `text_points`, `gap` (between chips); pills take a button's.
pub fn choice(ui: &mut Ui, t: &Theme, id: Id, style_: ChoiceStyle, options: &[&str], selected: &mut usize, width: f32) -> egui::Response {
    let s = style(t);
    let before = *selected;
    let mut resp = match style_ {
        ChoiceStyle::Segmented => segmented(ui, &s, id, options, selected),
        ChoiceStyle::Tabs => tabs(ui, &s, id, options, selected),
        ChoiceStyle::Pills => {
            let gap = s.gap(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(gap, gap);
                for (i, o) in options.iter().enumerate() {
                    if ui.push_id(i, |ui| action_s(ui, &s, &ActionOpts { kind: Kind::Pill, text: o, selected: i == *selected, ..Default::default() })).inner.clicked() {
                        *selected = i;
                    }
                }
            })
            .response
        }
        ChoiceStyle::Dropdown => {
            let k = s.k(14.0);
            let font = s.font(14.0 * k);
            let w = s.width(if width > 0.0 { width } else { options.iter().map(|o| text_width(ui, o, &font)).fold(60.0, f32::max) + 36.0 });
            let shown = options.get(*selected).copied().unwrap_or("");
            ui.scope(|ui| {
                let v = ui.visuals_mut();
                for wv in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
                    if let Some(f) = s.own.fill {
                        wv.bg_fill = f;
                        wv.weak_bg_fill = f;
                    }
                    if let Some(c) = s.own.stroke {
                        wv.bg_stroke.color = c;
                    }
                    wv.bg_stroke.width = s.stroke_w(1.0);
                    wv.corner_radius = radius(s.radius(0.0));
                }
                if let Some(f) = s.own.fill_hover {
                    v.widgets.hovered.bg_fill = f;
                    v.widgets.hovered.weak_bg_fill = f;
                }
                if let Some(c) = s.own.ink {
                    v.override_text_color = Some(c);
                }
                if let Some(h) = s.own.height {
                    ui.spacing_mut().interact_size.y = h;
                }
                egui::ComboBox::from_id_salt(id)
                    .selected_text(RichText::new(shown).font(font.clone()))
                    .width(w)
                    .show_ui(ui, |ui| {
                        for (i, o) in options.iter().enumerate() {
                            ui.selectable_value(selected, i, RichText::new(*o).font(font.clone()));
                        }
                    })
                    .response
            })
            .inner
        }
        ChoiceStyle::List => {
            let t = &s.t;
            let k = s.k(14.0);
            let h = s.height(30.0 * k.max(1.0));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, o) in options.iter().enumerate() {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
                    let r = ui.interact(rect, id.with(i), Sense::click());
                    let on = i == *selected;
                    let p = ui.painter();
                    if r.hovered() {
                        p.rect_filled(rect, s.radius(0.0), s.own.fill_hover.unwrap_or(t.tool_hover));
                    } else if let Some(f) = s.own.fill {
                        p.rect_filled(rect, s.radius(0.0), f);
                    }
                    let pad = s.pad_x(12.0);
                    let at = pos2(rect.left() + pad, rect.center().y);
                    p.circle_stroke(at, 6.0, Stroke::new(1.2, if on { t.accent } else { t.ink3 }));
                    if on {
                        p.circle_filled(at, 3.0, t.accent);
                    }
                    p.text(pos2(at.x + 16.0, rect.center().y), Align2::LEFT_CENTER, *o, s.font(14.0 * k), if on || r.hovered() { t.ink } else { t.ink2 });
                    if r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        *selected = i;
                    }
                }
            })
            .response
        }
    };
    if *selected != before {
        resp.mark_changed();
    }
    resp
}

fn segmented(ui: &mut Ui, s: &Style, id: Id, options: &[&str], selected: &mut usize) -> egui::Response {
    let t = &s.t;
    let k = s.k(13.0);
    let font = s.font(13.0 * k);
    let pad = s.pad_x(12.0);
    let widths: Vec<f32> = options.iter().map(|o| text_width(ui, o, &font) + 2.0 * pad).collect();
    let h = s.height(28.0 * k.max(1.0));
    let (all, resp) = ui.allocate_exact_size(vec2(widths.iter().sum::<f32>(), h), Sense::hover());
    let r = s.radius(0.0).round().clamp(0.0, 255.0) as u8;
    let last = options.len().saturating_sub(1);
    let stroke = Stroke::new(s.stroke_w(1.0), t.line);
    let mut x = all.left();
    for (i, (o, w)) in options.iter().zip(&widths).enumerate() {
        let rect = Rect::from_min_size(pos2(x, all.top()), vec2(*w, all.height()));
        let hit = ui.interact(rect, id.with(i), Sense::click());
        let on = i == *selected;
        let p = ui.painter();
        // Only the outer corners are rounded.
        let corners = CornerRadius { nw: if i == 0 { r } else { 0 }, sw: if i == 0 { r } else { 0 }, ne: if i == last { r } else { 0 }, se: if i == last { r } else { 0 } };
        let fill = if on { mix(t.bg, t.accent, 0.2) } else if hit.hovered() { s.own.fill_hover.unwrap_or(t.panel2) } else { t.panel };
        p.rect_filled(rect, corners, fill);
        if i > 0 {
            p.vline(x, all.y_range(), stroke);
        }
        p.text(rect.center(), Align2::CENTER_CENTER, *o, font.clone(), if on || hit.hovered() { s.own.ink.unwrap_or(t.ink) } else { t.ink3 });
        if hit.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            *selected = i;
        }
        x += w;
    }
    ui.painter().rect_stroke(all, r, stroke, StrokeKind::Inside);
    resp
}

fn tabs(ui: &mut Ui, s: &Style, id: Id, options: &[&str], selected: &mut usize) -> egui::Response {
    let t = &s.t;
    let k = s.k(13.5);
    let font = s.font(13.5 * k);
    let pad = s.pad_x(11.0);
    let (all, resp) = ui.allocate_exact_size(vec2(ui.available_width(), s.height(32.0 * k.max(1.0))), Sense::hover());
    let p = ui.painter().clone();
    p.hline(all.x_range(), all.bottom() - 0.5, Stroke::new(s.stroke_w(1.0), t.line));
    let mut x = all.left();
    for (i, o) in options.iter().enumerate() {
        let w = text_width(ui, o, &font) + 2.0 * pad;
        let rect = Rect::from_min_size(pos2(x, all.top()), vec2(w, all.height()));
        let r = ui.interact(rect, id.with(i), Sense::click());
        let on = i == *selected;
        if let (true, Some(f)) = (r.hovered(), s.own.fill_hover) {
            p.rect_filled(rect, s.radius(0.0), f);
        }
        p.text(rect.center(), Align2::CENTER_CENTER, *o, font.clone(), if on || r.hovered() { s.own.ink.unwrap_or(t.ink) } else { t.ink3 });
        if on {
            p.rect_filled(Rect::from_min_max(pos2(rect.left() + 4.0, rect.bottom() - 2.0), pos2(rect.right() - 4.0, rect.bottom())), 0.0, t.accent);
        }
        if r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            *selected = i;
        }
        x += w;
    }
    resp
}

// -------------------------------------------------------------- progress

/// A bar `fraction` full (None: moving, for work of unknown length), with
/// `label` over it (and the percentage, with `percent`).
///
/// Its look: `accent` (the bar, when its tone is plain), `fill` (the
/// track), `ink_weak` (the label), `height` (the bar's), `width`, `radius`,
/// `text_points`.
pub fn progress(ui: &mut Ui, t: &Theme, fraction: Option<f32>, label: &str, tone: Tone, percent: bool) -> egui::Response {
    let s = style(t);
    let t = &s.t;
    let k = s.k(11.5);
    let w = s.width(ui.available_width()).min(ui.available_width());
    let bar_h = s.height(5.0);
    let small = s.font(11.5 * k);
    let text = !label.is_empty() || (percent && fraction.is_some());
    let h = if text { small.size + 6.0 + bar_h } else { bar_h + 1.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    if !label.is_empty() {
        p.text(rect.left_top(), Align2::LEFT_TOP, label, small.clone(), t.ink2);
    }
    if let (true, Some(f)) = (percent, fraction) {
        p.text(rect.right_top(), Align2::RIGHT_TOP, format!("{}%", (f.clamp(0.0, 1.0) * 100.0).round()), small, t.ink3);
    }
    let bar = Rect::from_min_max(pos2(rect.left(), rect.bottom() - bar_h), rect.max);
    let round = s.radius(0.0);
    p.rect_filled(bar, round, s.own.fill.unwrap_or(t.panel2));
    let color = if tone == Tone::Plain { t.accent } else { s.tone(tone) };
    match fraction {
        Some(f) => {
            p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f.clamp(0.0, 1.0), bar.height())), round, color);
        }
        None => {
            // A third of the bar, going by.
            let time = ui.input(|i| i.time) as f32;
            let phase = (time * 0.7).fract() * 1.35 - 0.35;
            let x0 = bar.left() + bar.width() * phase;
            let seg = Rect::from_min_max(pos2(x0.max(bar.left()), bar.top()), pos2((x0 + bar.width() * 0.35).min(bar.right()), bar.bottom()));
            if seg.width() > 0.0 {
                p.rect_filled(seg, round, color);
            }
            ui.ctx().request_repaint();
        }
    }
    resp
}

// ---------------------------------------------------------------- notice

#[derive(Clone, Copy, Default)]
pub struct NoticeOpts {
    pub busy: bool,
    /// In a tinted box.
    pub boxed: bool,
    /// With an X.
    pub dismiss: bool,
}

/// A line saying how things stand, in `tone`'s color. Returns its response
/// and whether its X was pressed.
///
/// Its look: `ink` (its text), `fill` (its box), `stroke` and
/// `stroke_width` (the bar at its left), `radius`, `padding_x`,
/// `padding_y`, `text_points`, `no_frame` (no box).
pub fn notice(ui: &mut Ui, t: &Theme, tone: Tone, text: &str, o: &NoticeOpts) -> (egui::Response, bool) {
    let s = style(t);
    let t = &s.t;
    let k = s.k(12.5);
    let color = s.tone(tone);
    let ink = s.own.ink.unwrap_or(match tone {
        Tone::Plain => t.ink3,
        Tone::Accent => t.ink2,
        _ => color,
    });
    let font = s.font(12.5 * k);
    let mut dismissed = false;
    let mut line = |ui: &mut Ui| {
        // (Top-aligned: the lead stands by the first line when it wraps.)
        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let lead_h = ui.ctx().fonts_mut(|f| f.row_height(&font));
            if o.busy {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, lead_h), Sense::hover());
                spinner(ui, Rect::from_center_size(r.center(), vec2(13.0, 13.0)), t.ink3);
            } else if tone != Tone::Plain {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, lead_h), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, color);
            }
            let room = ui.available_width() - if o.dismiss { 26.0 } else { 0.0 };
            ui.allocate_ui(vec2(room.max(40.0), 0.0), |ui| ui.add(egui::Label::new(RichText::new(text).font(font.clone()).color(ink)).wrap()));
            if o.dismiss {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    let lit = resp.hovered();
                    if lit {
                        ui.painter().rect_filled(r, 0.0, t.tool_hover);
                    }
                    widgets::draw_x(ui.painter(), r, if lit { t.ink } else { t.ink3 });
                    dismissed = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Dismiss").clicked();
                });
            }
        })
        .response
    };
    let resp = if o.boxed && !s.no_frame() {
        let fill = s.own.fill.unwrap_or_else(|| mix(t.bg, if tone == Tone::Plain { t.ink } else { color }, 0.07));
        let r = egui::Frame::new()
            .fill(fill)
            .corner_radius(radius(s.radius(0.0)))
            .inner_margin(Margin { left: margin(s.pad_x(12.0), 0.0).left, right: 8, top: margin(0.0, s.pad_y(7.0)).top, bottom: margin(0.0, s.pad_y(7.0)).bottom })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                line(ui)
            })
            .response;
        let bar = s.own.stroke.unwrap_or(if tone == Tone::Plain { t.ink3 } else { color });
        ui.painter().rect_filled(Rect::from_min_size(r.rect.min, vec2(s.stroke_w(2.0), r.rect.height())), 0.0, bar);
        r
    } else {
        line(ui)
    };
    (resp, dismissed)
}

// ----------------------------------------------------------------- empty

/// What an empty page or list says, in the middle: a symbol, a title, a
/// line, and a button. Returns the button's response (or the whole's).
///
/// Its look: `ink` (the title), `ink_weak` (the line), `text_points` (the
/// title's), `padding_y` (above and below), `width` (the line's widest).
pub fn empty(ui: &mut Ui, t: &Theme, icon: &str, title: &str, text: &str, button: &str) -> egui::Response {
    let s = style(t);
    let inner = s.inner();
    let t = &s.t;
    let k = s.k(15.0);
    let pad = s.pad_y(28.0);
    let mut pressed = None;
    let whole = ui
        .vertical_centered(|ui| {
            ui.add_space(pad);
            if !icon.is_empty() {
                ui.label(RichText::new(icon).font(glyph_font(30.0 * k)).color(t.ink4));
                ui.add_space(4.0);
            }
            if !title.is_empty() {
                ui.label(RichText::new(title).font(s.font(15.0 * k)).color(s.own.ink.unwrap_or(t.ink2)));
            }
            if !text.is_empty() {
                ui.set_max_width(ui.available_width().min(s.width(380.0)));
                ui.add(egui::Label::new(RichText::new(text).font(s.font(12.5 * k)).color(t.ink3)).wrap());
            }
            if !button.is_empty() {
                ui.add_space(8.0);
                pressed = Some(action_s(ui, &inner, &ActionOpts { kind: Kind::Primary, text: button, ..Default::default() }));
            }
            ui.add_space(pad);
        })
        .response;
    pressed.unwrap_or(whole)
}

// ------------------------------------------------------------------ stat

/// A number in a box: `label` over it, `detail` under it; the number in
/// `tone`'s color (plain: the text color).
///
/// Its look: its box's (`fill`, `fill_hover`, `stroke`, `stroke_width`,
/// `radius`, `padding_x`, `padding_y`, `width`, `no_frame`), `ink` (the
/// number), `text_points` (the number's size).
pub fn stat(ui: &mut Ui, t: &Theme, label: &str, value: &str, detail: &str, tone: Tone) -> egui::Response {
    let s = style(t);
    let k = s.k(22.0);
    let ink = s.own.ink.unwrap_or(if tone == Tone::Plain { s.t.ink } else { s.tone(tone) });
    card_s(ui, &s.with_own(s.frame_look()), &CardOpts::default(), &mut |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(label).font(s.font(11.5 * k)).color(s.t.ink3));
        ui.label(RichText::new(value).font(s.font(22.0 * k)).color(ink));
        if !detail.is_empty() {
            ui.add(egui::Label::new(RichText::new(detail).font(s.font(11.5 * k)).color(s.t.ink3)).truncate());
        }
    })
}

// ---------------------------------------------------------------- slider

#[derive(Clone, Copy, Default)]
pub struct SliderOpts<'a> {
    pub min: f64,
    pub max: f64,
    /// It moves in steps of this; 0: smoothly.
    pub step: f64,
    /// 0: 200.
    pub width: f32,
    pub decimals: usize,
    pub suffix: &'a str,
    /// Its value at its right.
    pub show_value: bool,
    pub disabled: bool,
}

/// A slider over `value` (the response is changed when it moved).
///
/// Its look: `accent` (the part before the knob), `fill` (the track),
/// `ink` (the knob and the value), `radius`, `width` (the track's),
/// `height` (the row's), `text_points` (the value's).
pub fn slider(ui: &mut Ui, t: &Theme, value: &mut f64, o: &SliderOpts) -> egui::Response {
    let s = style(t);
    let t = &s.t;
    let k = s.k(12.0);
    let track_w = s.width(if o.width > 0.0 { o.width } else { 200.0 }).min(ui.available_width());
    let font = s.font(12.0 * k);
    let label = |v: f64| format!("{v:.*}{}", o.decimals, o.suffix);
    let value_w = if o.show_value { text_width(ui, &label(o.max), &font).max(text_width(ui, &label(o.min), &font)) + 12.0 } else { 0.0 };
    let (rect, mut resp) = ui.allocate_exact_size(vec2(track_w + value_w, s.height(22.0)), if o.disabled { Sense::hover() } else { Sense::click_and_drag() });
    let (lo, hi) = if o.min <= o.max { (o.min, o.max) } else { (o.max, o.min) };
    let track = Rect::from_min_max(pos2(rect.left() + 5.0, rect.center().y - 2.0), pos2(rect.left() + track_w - 5.0, rect.center().y + 2.0));
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    if let Some(at) = resp.interact_pointer_pos().filter(|_| !o.disabled && (resp.dragged() || resp.clicked() || resp.is_pointer_button_down_on())) {
        let f = ((at.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0) as f64;
        let mut v = lo + f * span;
        if o.step > 0.0 {
            v = lo + ((v - lo) / o.step).round() * o.step;
        }
        let v = v.clamp(lo, hi);
        if v != *value {
            *value = v;
            resp.mark_changed();
        }
    }
    let f = ((value.clamp(lo, hi) - lo) / span) as f32;
    let p = ui.painter();
    let round = s.radius(0.0);
    p.rect_filled(track, round, s.own.fill.unwrap_or(t.panel2));
    let kx = track.left() + track.width() * f;
    p.rect_filled(Rect::from_min_max(track.min, pos2(kx, track.bottom())), round, if o.disabled { t.ink4 } else { t.accent });
    let lit = !o.disabled && (resp.hovered() || resp.dragged());
    let knob = Rect::from_center_size(pos2(kx, track.center().y), vec2(10.0, 16.0));
    let knob_ink = s.own.ink.unwrap_or(if lit { t.ink } else { t.ink2 });
    p.rect(knob, round.min(5.0), if o.disabled { t.ink4 } else { knob_ink }, Stroke::new(1.0, t.bg), StrokeKind::Outside);
    if o.show_value {
        p.text(pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, label(value.clamp(lo, hi)), font, s.own.ink.unwrap_or(t.ink2));
    }
    if lit { resp.on_hover_cursor(CursorIcon::Grab) } else { resp }
}

// ----------------------------------------------------------------- table

#[derive(Clone, Copy, Default)]
pub struct TableOpts<'a> {
    pub id: Option<Id>,
    /// The columns' headings.
    pub columns: &'a [&'a str],
    /// Each column's width (missing or 0: a share of what is left).
    pub widths: &'a [f32],
    /// Each column's alignment (missing: at the left).
    pub aligns: &'a [Align],
    pub rows: usize,
    /// 0: 28.
    pub row_height: f32,
    /// Rows lit under the pointer and clicked.
    pub click: bool,
    pub no_head: bool,
}

/// A table: its headings, then `cell` draws each cell (row, column) in
/// its place. Returns the whole's response and the row clicked.
///
/// Its look: `fill` (the headings' row), `fill_hover` (a row under the
/// pointer), `stroke` and `stroke_width` (its lines), `ink_weak` (the
/// headings), `height` (a row's), `padding_x` (in a cell), `text_points`
/// (the headings'), `no_frame` (no lines).
pub fn table(ui: &mut Ui, t: &Theme, o: &TableOpts, cell: &mut dyn FnMut(&mut Ui, usize, usize)) -> (egui::Response, Option<usize>) {
    let s = style(t);
    let t = &s.t;
    let k = s.k(11.5);
    let id = o.id.unwrap_or_else(|| ui.scope_id().with("table"));
    let n = o.columns.len();
    let w = ui.available_width();
    let pad = s.pad_x(8.0);
    let fixed: f32 = (0..n).map(|c| o.widths.get(c).copied().unwrap_or(0.0).max(0.0)).sum();
    let flex = (0..n).filter(|c| o.widths.get(*c).copied().unwrap_or(0.0) <= 0.0).count();
    let share = if flex > 0 { ((w - fixed) / flex as f32).max(40.0) } else { 0.0 };
    let mut xs = Vec::with_capacity(n);
    let mut x = 0.0;
    for c in 0..n {
        let cw = match o.widths.get(c).copied().unwrap_or(0.0) {
            v if v > 0.0 => v,
            _ => share,
        };
        xs.push((x, cw));
        x += cw;
    }
    let align = |c: usize| o.aligns.get(c).copied().unwrap_or(Align::Min);
    let lines = !s.no_frame();
    let line_w = s.stroke_w(1.0);
    let top = ui.cursor().min.y;
    let spacing = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    if !o.no_head {
        let small = s.font(11.5 * k);
        let (head, _) = ui.allocate_exact_size(vec2(w, (26.0 * k).max(small.size + 10.0)), Sense::hover());
        let p = ui.painter();
        if let Some(f) = s.own.fill {
            p.rect_filled(head, s.radius(0.0), f);
        }
        for (c, title) in o.columns.iter().enumerate() {
            let (cx, cw) = xs[c];
            let (anchor, at) = match align(c) {
                Align::Min => (Align2::LEFT_CENTER, head.left() + cx + pad),
                Align::Center => (Align2::CENTER_CENTER, head.left() + cx + cw / 2.0),
                Align::Max => (Align2::RIGHT_CENTER, head.left() + cx + cw - pad),
            };
            p.text(pos2(at, head.center().y), anchor, clip_width(ui, title, &small, (cw - 2.0 * pad).max(8.0)), small.clone(), t.ink3);
        }
        if lines {
            p.hline(head.x_range(), head.bottom() - 0.5, Stroke::new(line_w, t.line));
        }
    }
    let rh = if o.row_height > 0.0 { o.row_height } else { s.height(28.0) };
    let soft = s.own.stroke.unwrap_or(t.line.gamma_multiply(0.6));
    let mut clicked = None;
    for r in 0..o.rows {
        let (rect, _) = ui.allocate_exact_size(vec2(w, rh), Sense::hover());
        if !ui.is_rect_visible(rect) {
            continue;
        }
        if o.click {
            let resp = ui.interact(rect, id.with(r), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 0.0, s.own.fill_hover.unwrap_or(t.tool_hover));
            }
            if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                clicked = Some(r);
            }
        }
        if lines && r + 1 < o.rows {
            ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(line_w, soft));
        }
        for (c, (cx, cw)) in xs.iter().enumerate() {
            let area = Rect::from_min_size(pos2(rect.left() + cx + pad, rect.top()), vec2((cw - 2.0 * pad).max(1.0), rh));
            let layout = match align(c) {
                Align::Min => Layout::left_to_right(Align::Center),
                Align::Center => Layout::centered_and_justified(egui::Direction::LeftToRight),
                Align::Max => Layout::right_to_left(Align::Center),
            };
            let mut child = ui.new_child(UiBuilder::new().id_salt((id, r, c)).max_rect(area).layout(layout));
            child.shrink_clip_rect(area);
            if o.click {
                // (Text in a row that is clicked isn't for selecting.)
                child.style_mut().interaction.selectable_labels = false;
            }
            cell(&mut child, r, c);
        }
    }
    ui.spacing_mut().item_spacing.y = spacing;
    let bottom = ui.cursor().min.y;
    let whole = ui.interact(Rect::from_min_max(pos2(ui.cursor().min.x, top), pos2(ui.cursor().min.x + w, bottom)), id.with("whole"), Sense::hover());
    ui.add_space(spacing);
    (whole, clicked)
}

// --------------------------------------------------------------- columns

/// `count` columns of equal width, `gap` apart (0: 12, or its look's
/// `gap`): `cell` draws each.
pub fn columns(ui: &mut Ui, count: usize, gap: f32, cell: &mut dyn FnMut(&mut Ui, usize)) {
    let own = NEXT.with(|n| n.take()).unwrap_or_default();
    if count == 0 {
        return;
    }
    let gap = if gap > 0.0 { gap } else { own.gap.unwrap_or(12.0) };
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.columns(count, |cols| {
            for (i, c) in cols.iter_mut().enumerate() {
                cell(c, i);
            }
        });
    });
}

// ---------------------------------------------------------------- toasts

struct Toast {
    tone: Tone,
    text: String,
    /// When it was first shown (the window's time); None: not yet.
    shown: Option<f64>,
}

static TOASTS: Mutex<Vec<Toast>> = Mutex::new(Vec::new());
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

/// How long a toast stays, in seconds; an error's twice as long.
const TOAST_S: f64 = 4.0;
/// The most shown at once (the oldest go first).
const TOASTS_SHOWN: usize = 4;

/// A short message in the window's corner that goes by itself. Any thread.
pub fn toast(tone: Tone, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    TOASTS.lock().unwrap().push(Toast { tone, text: text.to_string(), shown: None });
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}

/// The toasts' texts now (for tests).
#[cfg(test)]
pub fn toasts_now() -> Vec<String> {
    TOASTS.lock().unwrap().iter().map(|t| t.text.clone()).collect()
}

/// Draws the toasts at the window's top right, `top` points down; a click
/// on one dismisses it. Every frame.
pub fn show_toasts(ctx: &egui::Context, t: &Theme, top: f32) {
    let _ = CONTEXT.set(ctx.clone());
    let now = ctx.input(|i| i.time);
    let life = |tone: Tone| if tone == Tone::Bad { 2.0 * TOAST_S } else { TOAST_S };
    let mut all = TOASTS.lock().unwrap();
    all.retain(|x| x.shown.is_none_or(|at| now - at < life(x.tone)));
    if all.is_empty() {
        return;
    }
    let skip = all.len().saturating_sub(TOASTS_SHOWN);
    let mut gone = None;
    let mut next = f64::INFINITY;
    egui::Area::new(Id::unique("oak-toasts")).order(egui::Order::Foreground).anchor(Align2::RIGHT_TOP, vec2(-16.0, top)).interactable(true).show(ctx, |ui| {
        ui.set_max_width(360.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        for (i, x) in all.iter_mut().enumerate().skip(skip) {
            let at = *x.shown.get_or_insert(now);
            let left = life(x.tone) - (now - at);
            next = next.min(left);
            // Fades out over its last 0.4 seconds.
            let fade = (left / 0.4).clamp(0.0, 1.0) as f32;
            ui.scope(|ui| {
                ui.multiply_opacity(fade);
                let color = if x.tone == Tone::Plain { t.ink3 } else { tone_color(t, x.tone) };
                let r = egui::Frame::new()
                    .fill(t.panel)
                    .stroke(Stroke::new(1.0, t.line))
                    .inner_margin(Margin { left: 14, right: 12, top: 9, bottom: 9 })
                    .show(ui, |ui| {
                        ui.set_min_width(200.0);
                        ui.add(egui::Label::new(RichText::new(&x.text).size(13.0).color(t.ink)).wrap().selectable(false));
                    })
                    .response;
                ui.painter().rect_filled(Rect::from_min_size(r.rect.min, vec2(2.0, r.rect.height())), 0.0, color);
                if ui.interact(r.rect, Id::unique(("oak-toast", i, &x.text)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    gone = Some(i);
                }
            });
        }
    });
    if let Some(i) = gone {
        all.remove(i);
    }
    // Wake up for the next to fade.
    if next.is_finite() {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64((next - 0.4).clamp(0.0, TOAST_S)));
        if next < 0.45 {
            ctx.request_repaint();
        }
    }
}
