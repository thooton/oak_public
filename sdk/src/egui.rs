//! Drawing, under the names of the egui crate.
//!
//! Nothing here draws: every call goes to Oak through the C interface
//! (`abi`), and Oak's own egui does the work. The names and shapes follow
//! egui's so that code written for egui reads the same here; what isn't
//! here, Oak doesn't offer (yet). Geometry and colors are the `emath` and
//! `ecolor` crates' own.
//!
//! A `Ui` is valid inside the closure or the part's method it was given
//! to. State a widget keeps between frames (`Ui::data`) lives in the
//! plugin.

use crate::abi::*;
use crate::rt::{api, bytes, carry, rethrow, s, text_from};
use std::any::Any;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

pub use ecolor::{self, Color32, Rgba};
pub use emath::{self, Align, Align2, NumExt, Pos2, Rangef, Rect, Vec2, lerp, pos2, remap, remap_clamp, vec2};

// ---------------------------------------------------------------- values

pub(crate) fn rect_in(r: Rect) -> OakRect {
    OakRect { x0: r.min.x, y0: r.min.y, x1: r.max.x, y1: r.max.y }
}

pub(crate) fn rect_out(r: OakRect) -> Rect {
    Rect::from_min_max(pos2(r.x0, r.y0), pos2(r.x1, r.y1))
}

/// A color as the interface takes it: `0xRRGGBBAA` (0: none, the default).
pub fn color_in(c: Color32) -> u32 {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    u32::from_be_bytes([r, g, b, a])
}

pub fn color_out(c: u32) -> Color32 {
    let [r, g, b, a] = c.to_be_bytes();
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// A widget's id, or a salt for one: a hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(u64);

impl Id {
    pub const NULL: Id = Id(0);

    pub fn new(source: impl Hash) -> Id {
        let mut h = std::hash::DefaultHasher::new();
        source.hash(&mut h);
        Id(h.finish().max(1))
    }

    /// The same everywhere it is made from `source` (not mixed with a `Ui`'s id).
    pub fn unique(source: impl Hash) -> Id {
        Id::new(source)
    }

    pub fn with(self, salt: impl Hash) -> Id {
        Id::new((self.0, Id::new(salt).0))
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

impl From<&str> for Id {
    fn from(s: &str) -> Id {
        Id::new(s)
    }
}

impl From<String> for Id {
    fn from(s: String) -> Id {
        Id::new(s)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub color: Color32,
}

impl Stroke {
    pub const NONE: Stroke = Stroke { width: 0.0, color: Color32::TRANSPARENT };

    /// (The width is a plain `f32`, so a literal needs no suffix.)
    pub fn new(width: f32, color: impl Into<Color32>) -> Stroke {
        Stroke { width, color: color.into() }
    }
}

impl From<(f32, Color32)> for Stroke {
    fn from((width, color): (f32, Color32)) -> Stroke {
        Stroke { width, color }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum StrokeKind {
    Inside,
    #[default]
    Middle,
    Outside,
}

/// How round a rectangle's corners are: top left, top right, bottom left
/// and bottom right.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CornerRadius {
    pub nw: u8,
    pub ne: u8,
    pub sw: u8,
    pub se: u8,
}

impl CornerRadius {
    pub const ZERO: CornerRadius = CornerRadius::same(0);

    pub const fn same(r: u8) -> CornerRadius {
        CornerRadius { nw: r, ne: r, sw: r, se: r }
    }

    /// All four the same.
    pub fn is_same(&self) -> bool {
        self.nw == self.ne && self.nw == self.sw && self.nw == self.se
    }

    pub(crate) fn all(&self) -> [f32; 4] {
        [self.nw as f32, self.ne as f32, self.sw as f32, self.se as f32]
    }
}

macro_rules! radius_from {
    ($($t:ty),*) => { $(impl From<$t> for CornerRadius { fn from(r: $t) -> CornerRadius { CornerRadius::same((r as f32).round().clamp(0.0, 255.0) as u8) } })* };
}
radius_from!(f32, u8, i32, i8);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Margin {
    pub x: f32,
    pub y: f32,
}

impl Margin {
    pub const ZERO: Margin = Margin { x: 0.0, y: 0.0 };

    pub fn same(m: i8) -> Margin {
        Margin { x: m as f32, y: m as f32 }
    }

    pub fn symmetric(x: i8, y: i8) -> Margin {
        Margin { x: x as f32, y: y as f32 }
    }
}

macro_rules! margin_from {
    ($($t:ty),*) => { $(impl From<$t> for Margin { fn from(m: $t) -> Margin { Margin { x: m as f32, y: m as f32 } } })* };
}
margin_from!(f32, i8, i32);

#[derive(Clone, Debug, PartialEq)]
pub enum FontFamily {
    Proportional,
    Monospace,
    /// Oak's "code" and "icons".
    Name(Arc<str>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontId {
    pub size: f32,
    pub family: FontFamily,
}

impl FontId {
    pub fn new(size: f32, family: FontFamily) -> FontId {
        FontId { size, family }
    }

    pub fn proportional(size: f32) -> FontId {
        FontId { size, family: FontFamily::Proportional }
    }

    pub fn monospace(size: f32) -> FontId {
        FontId { size, family: FontFamily::Monospace }
    }

    fn code(&self) -> (u32, u32) {
        match &self.family {
            FontFamily::Proportional => (OAK_FONT_TEXT, 0),
            FontFamily::Monospace => (OAK_FONT_TEXT, OAK_TEXT_MONOSPACE),
            FontFamily::Name(n) if &**n == "icons" => (OAK_FONT_ICONS, 0),
            FontFamily::Name(_) => (OAK_FONT_CODE, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextStyle {
    Small,
    Body,
    Monospace,
    Button,
    Heading,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextWrapMode {
    Extend,
    Wrap,
    Truncate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense(u32);

impl Sense {
    pub fn hover() -> Sense {
        Sense(OAK_SENSE_HOVER)
    }

    pub fn click() -> Sense {
        Sense(OAK_SENSE_CLICK)
    }

    pub fn drag() -> Sense {
        Sense(OAK_SENSE_DRAG)
    }

    pub fn click_and_drag() -> Sense {
        Sense(OAK_SENSE_CLICK_AND_DRAG)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorIcon {
    Default,
    PointingHand,
    Text,
    Grab,
    Grabbing,
    ResizeHorizontal,
    ResizeVertical,
    NotAllowed,
    Wait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Background,
    Middle,
    Foreground,
    Tooltip,
}

impl Order {
    fn code(self) -> u32 {
        match self {
            Order::Middle => OAK_ORDER_MIDDLE,
            Order::Background => OAK_ORDER_BACKGROUND,
            Order::Foreground => OAK_ORDER_FOREGROUND,
            Order::Tooltip => OAK_ORDER_TOOLTIP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Space,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    /// A letter or a digit, by its ASCII code ('A', '7').
    Char(u8),
}

impl Key {
    fn code(self) -> u32 {
        match self {
            Key::Enter => OAK_KEY_ENTER,
            Key::Escape => OAK_KEY_ESCAPE,
            Key::Tab => OAK_KEY_TAB,
            Key::Backspace => OAK_KEY_BACKSPACE,
            Key::Delete => OAK_KEY_DELETE,
            Key::Space => OAK_KEY_SPACE,
            Key::ArrowLeft => OAK_KEY_LEFT,
            Key::ArrowRight => OAK_KEY_RIGHT,
            Key::ArrowUp => OAK_KEY_UP,
            Key::ArrowDown => OAK_KEY_DOWN,
            Key::Home => OAK_KEY_HOME,
            Key::End => OAK_KEY_END,
            Key::PageUp => OAK_KEY_PAGE_UP,
            Key::PageDown => OAK_KEY_PAGE_DOWN,
            Key::Char(c) => c.to_ascii_uppercase() as u32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    /// Ctrl, or Cmd on a Mac.
    pub command: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, command: false };
    pub const SHIFT: Modifiers = Modifiers { alt: false, ctrl: false, shift: true, command: false };
    pub const COMMAND: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, command: true };

    fn code(self) -> u32 {
        (if self.shift { OAK_MOD_SHIFT } else { 0 }) | (if self.ctrl { OAK_MOD_CTRL } else { 0 }) | (if self.alt { OAK_MOD_ALT } else { 0 }) | (if self.command { OAK_MOD_COMMAND } else { 0 })
    }

    fn from_code(c: u32) -> Modifiers {
        Modifiers { shift: c & OAK_MOD_SHIFT != 0, ctrl: c & OAK_MOD_CTRL != 0, alt: c & OAK_MOD_ALT != 0, command: c & OAK_MOD_COMMAND != 0 }
    }

    pub fn any(&self) -> bool {
        self.alt || self.ctrl || self.shift || self.command
    }
}

fn align_code(a: Align) -> u32 {
    match a {
        Align::Min => OAK_ALIGN_MIN,
        Align::Center => OAK_ALIGN_CENTER,
        Align::Max => OAK_ALIGN_MAX,
    }
}

/// How a container lays out what is drawn in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    dir: u32,
    main_align: Align,
    cross_align: Align,
    flags: u32,
}

impl Layout {
    fn of(dir: u32, cross_align: Align) -> Layout {
        // (Centered along its direction, as egui's are: it is what puts a
        // button's text in the middle of a button wider than it.)
        Layout { dir, main_align: Align::Center, cross_align, flags: 0 }
    }

    pub fn left_to_right(valign: Align) -> Layout {
        Layout::of(OAK_DIR_RIGHT, valign)
    }

    pub fn right_to_left(valign: Align) -> Layout {
        Layout::of(OAK_DIR_LEFT, valign)
    }

    pub fn top_down(halign: Align) -> Layout {
        Layout::of(OAK_DIR_DOWN, halign)
    }

    pub fn bottom_up(halign: Align) -> Layout {
        Layout::of(OAK_DIR_UP, halign)
    }

    pub fn top_down_justified(halign: Align) -> Layout {
        Layout::top_down(halign).with_cross_justify(true)
    }

    pub fn with_main_wrap(mut self, wrap: bool) -> Layout {
        self.flag(OAK_LAYOUT_WRAP, wrap)
    }

    pub fn with_cross_justify(mut self, on: bool) -> Layout {
        self.flag(OAK_LAYOUT_CROSS_JUSTIFY, on)
    }

    pub fn with_main_justify(mut self, on: bool) -> Layout {
        self.flag(OAK_LAYOUT_MAIN_JUSTIFY, on)
    }

    pub fn with_main_align(mut self, a: Align) -> Layout {
        self.main_align = a;
        self
    }

    pub fn with_cross_align(mut self, a: Align) -> Layout {
        self.cross_align = a;
        self
    }

    fn flag(&mut self, f: u32, on: bool) -> Layout {
        self.flags = if on { self.flags | f } else { self.flags & !f };
        *self
    }

    fn raw(&self) -> OakLayout {
        OakLayout { size: std::mem::size_of::<OakLayout>() as u32, dir: self.dir, main_align: align_code(self.main_align), cross_align: align_code(self.cross_align), flags: self.flags }
    }
}

fn anchor_code(a: Align2) -> u32 {
    let col = match a.0[0] {
        Align::Min => 0,
        Align::Center => 1,
        Align::Max => 2,
    };
    let row = match a.0[1] {
        Align::Min => 0,
        Align::Center => 1,
        Align::Max => 2,
    };
    row * 3 + col
}

// ------------------------------------------------------------------ text

/// Text and how it looks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    text: String,
    color: Option<Color32>,
    size: f32,
    flags: u32,
    font: u32,
}

/// What a widget takes as its text.
pub type WidgetText = RichText;

impl RichText {
    pub fn new(text: impl Into<String>) -> RichText {
        RichText { text: text.into(), ..Default::default() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn color(mut self, c: impl Into<Color32>) -> RichText {
        self.color = Some(c.into());
        self
    }

    pub fn size(mut self, points: f32) -> RichText {
        self.size = points;
        self
    }

    pub fn font(mut self, f: FontId) -> RichText {
        let (font, flags) = f.code();
        self.size = f.size;
        self.font = font;
        self.flags |= flags;
        self
    }

    pub fn family(mut self, f: FontFamily) -> RichText {
        let (font, flags) = FontId::new(0.0, f).code();
        self.font = font;
        self.flags |= flags;
        self
    }

    pub fn text_style(mut self, style: TextStyle) -> RichText {
        self.flags |= match style {
            TextStyle::Small => OAK_TEXT_SMALL,
            TextStyle::Monospace => OAK_TEXT_MONOSPACE,
            TextStyle::Heading => OAK_TEXT_HEADING,
            TextStyle::Body | TextStyle::Button => 0,
        };
        self
    }

    fn with(mut self, flag: u32) -> RichText {
        self.flags |= flag;
        self
    }

    pub fn strong(self) -> RichText {
        self.with(OAK_TEXT_STRONG)
    }

    pub fn italics(self) -> RichText {
        self.with(OAK_TEXT_ITALIC)
    }

    pub fn small(self) -> RichText {
        self.with(OAK_TEXT_SMALL)
    }

    pub fn weak(self) -> RichText {
        self.with(OAK_TEXT_WEAK)
    }

    pub fn underline(self) -> RichText {
        self.with(OAK_TEXT_UNDERLINE)
    }

    pub fn strikethrough(self) -> RichText {
        self.with(OAK_TEXT_STRIKE)
    }

    pub fn heading(self) -> RichText {
        self.with(OAK_TEXT_HEADING)
    }

    pub fn monospace(self) -> RichText {
        self.with(OAK_TEXT_MONOSPACE)
    }

    /// Lent to Oak for a call, with more flags.
    pub(crate) fn raw(&self, more: u32) -> OakText {
        OakText { size: std::mem::size_of::<OakText>() as u32, flags: self.flags | more, text: s(&self.text), color: self.color.map_or(0, color_in), points: self.size, font: self.font }
    }
}

impl From<&str> for RichText {
    fn from(t: &str) -> RichText {
        RichText::new(t)
    }
}

impl From<String> for RichText {
    fn from(t: String) -> RichText {
        RichText::new(t)
    }
}

impl From<&String> for RichText {
    fn from(t: &String) -> RichText {
        RichText::new(t.as_str())
    }
}

impl From<std::borrow::Cow<'_, str>> for RichText {
    fn from(t: std::borrow::Cow<'_, str>) -> RichText {
        RichText::new(t.into_owned())
    }
}

// -------------------------------------------------------------- response

/// What a widget answers.
#[derive(Clone, Debug)]
pub struct Response {
    pub rect: Rect,
    pub id: Id,
    flags: u32,
    handle: u32,
    drag: Vec2,
    pointer: Option<Pos2>,
}

/// A container's answer, and what its closure returned.
pub struct InnerResponse<R> {
    pub inner: R,
    pub response: Response,
}

impl Response {
    pub(crate) fn of(r: OakResponse) -> Response {
        Response {
            rect: rect_out(r.rect),
            id: Id(r.widget),
            flags: r.flags,
            handle: r.id,
            drag: vec2(r.drag_x, r.drag_y),
            pointer: (r.flags & OAK_HAS_POINTER != 0).then(|| pos2(r.pointer_x, r.pointer_y)),
        }
    }

    fn has(&self, f: u32) -> bool {
        self.flags & f != 0
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub fn clicked(&self) -> bool {
        self.has(OAK_CLICKED)
    }

    pub fn secondary_clicked(&self) -> bool {
        self.has(OAK_SECONDARY_CLICKED)
    }

    pub fn double_clicked(&self) -> bool {
        self.has(OAK_DOUBLE_CLICKED)
    }

    pub fn middle_clicked(&self) -> bool {
        self.has(OAK_MIDDLE_CLICKED)
    }

    pub fn hovered(&self) -> bool {
        self.has(OAK_HOVERED)
    }

    pub fn contains_pointer(&self) -> bool {
        self.has(OAK_CONTAINS_POINTER)
    }

    pub fn changed(&self) -> bool {
        self.has(OAK_CHANGED)
    }

    pub fn mark_changed(&mut self) {
        self.flags |= OAK_CHANGED;
    }

    pub fn dragged(&self) -> bool {
        self.has(OAK_DRAGGED)
    }

    pub fn drag_started(&self) -> bool {
        self.has(OAK_DRAG_STARTED)
    }

    pub fn drag_stopped(&self) -> bool {
        self.has(OAK_DRAG_STOPPED)
    }

    pub fn drag_delta(&self) -> Vec2 {
        self.drag
    }

    pub fn has_focus(&self) -> bool {
        self.has(OAK_HAS_FOCUS)
    }

    pub fn gained_focus(&self) -> bool {
        self.has(OAK_GAINED_FOCUS)
    }

    pub fn lost_focus(&self) -> bool {
        self.has(OAK_LOST_FOCUS)
    }

    pub fn enabled(&self) -> bool {
        self.has(OAK_ENABLED)
    }

    pub fn clicked_elsewhere(&self) -> bool {
        self.has(OAK_CLICKED_ELSEWHERE)
    }

    pub fn is_pointer_button_down_on(&self) -> bool {
        self.has(OAK_POINTER_DOWN_ON)
    }

    pub fn interact_pointer_pos(&self) -> Option<Pos2> {
        self.pointer
    }

    pub fn hover_pos(&self) -> Option<Pos2> {
        self.pointer.filter(|_| self.hovered())
    }

    fn act(&self, what: u32, value: u32, text: &str) {
        unsafe { (api().ui_response)(self.handle, what, value, s(text), None, std::ptr::null_mut()) };
    }

    fn act_body(&self, what: u32, f: impl FnOnce(&mut Ui)) {
        with_body(f, |body, user| unsafe { (api().ui_response)(self.handle, what, 0, s(""), Some(body), user) });
    }

    pub fn on_hover_text(self, text: impl Into<RichText>) -> Response {
        self.act(OAK_RESPONSE_HOVER_TEXT, 0, &text.into().text);
        self
    }

    pub fn on_disabled_hover_text(self, text: impl Into<RichText>) -> Response {
        self.act(OAK_RESPONSE_DISABLED_HOVER_TEXT, 0, &text.into().text);
        self
    }

    pub fn on_hover_ui(self, f: impl FnOnce(&mut Ui)) -> Response {
        self.act_body(OAK_RESPONSE_HOVER_UI, f);
        self
    }

    pub fn on_hover_cursor(self, cursor: CursorIcon) -> Response {
        let code = match cursor {
            CursorIcon::Default => OAK_CURSOR_DEFAULT,
            CursorIcon::PointingHand => OAK_CURSOR_POINTING_HAND,
            CursorIcon::Text => OAK_CURSOR_TEXT,
            CursorIcon::Grab => OAK_CURSOR_GRAB,
            CursorIcon::Grabbing => OAK_CURSOR_GRABBING,
            CursorIcon::ResizeHorizontal => OAK_CURSOR_RESIZE_HORIZONTAL,
            CursorIcon::ResizeVertical => OAK_CURSOR_RESIZE_VERTICAL,
            CursorIcon::NotAllowed => OAK_CURSOR_NOT_ALLOWED,
            CursorIcon::Wait => OAK_CURSOR_WAIT,
        };
        self.act(OAK_RESPONSE_CURSOR, code, "");
        self
    }

    /// Its right-click menu.
    pub fn context_menu(&self, f: impl FnOnce(&mut Ui)) {
        self.act_body(OAK_RESPONSE_CONTEXT_MENU, f);
    }

    pub fn request_focus(&self) {
        self.act(OAK_RESPONSE_REQUEST_FOCUS, 0, "");
    }

    pub fn surrender_focus(&self) {
        self.act(OAK_RESPONSE_SURRENDER_FOCUS, 0, "");
    }

    pub fn scroll_to_me(&self, align: Option<Align>) {
        self.act(OAK_RESPONSE_SCROLL_TO, align.map_or(3, align_code), "");
    }

    pub fn highlight(self) -> Response {
        self.act(OAK_RESPONSE_HIGHLIGHT, 0, "");
        self
    }

    /// Both: over either, clicked on either.
    pub fn union(&self, other: Response) -> Response {
        let mut r = self.clone();
        r.rect = r.rect.union(other.rect);
        r.flags |= other.flags;
        r
    }
}

impl std::ops::BitOr for Response {
    type Output = Response;

    fn bitor(self, other: Response) -> Response {
        self.union(other)
    }
}

// --------------------------------------------------------------- closures

/// Runs `call` with `f` as the body Oak calls back (once, or not at all).
pub(crate) fn with_body<R, T>(f: impl FnOnce(&mut Ui) -> R, call: impl FnOnce(OakBody, *mut c_void) -> T) -> (Option<R>, T) {
    struct Slot<F, R>(Option<F>, Option<R>);
    unsafe extern "C" fn run<F: FnOnce(&mut Ui) -> R, R>(user: *mut c_void, ui: *mut OakUi) {
        let slot = unsafe { &mut *(user as *mut Slot<F, R>) };
        if let Some(f) = slot.0.take() {
            slot.1 = carry(|| f(&mut Ui::raw(ui)));
        }
    }
    fn tramp<F: FnOnce(&mut Ui) -> R, R>(_: &Slot<F, R>) -> OakBody {
        run::<F, R>
    }
    let mut slot = Slot(Some(f), None);
    let body = tramp(&slot);
    let t = call(body, &mut slot as *mut Slot<_, R> as *mut c_void);
    rethrow();
    (slot.1, t)
}

/// A body Oak always runs: its answer, and the container's.
fn contained<R>(f: impl FnOnce(&mut Ui) -> R, call: impl FnOnce(OakBody, *mut c_void, *mut OakResponse)) -> InnerResponse<R> {
    let mut out = OakResponse::default();
    let (inner, ()) = with_body(f, |body, user| call(body, user, &mut out));
    InnerResponse { inner: inner.expect("Oak didn't draw the container"), response: Response::of(out) }
}

/// A sink that hands a `String` what it gets (a text field's new text).
pub(crate) unsafe extern "C" fn sink_string(user: *mut c_void, ptr: *const u8, len: usize) {
    let out = unsafe { &mut *(user as *mut Option<String>) };
    let bytes = if ptr.is_null() || len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(ptr, len) } };
    *out = Some(String::from_utf8_lossy(bytes).into_owned());
}

/// Calls `call` with a function that gives Oak `f`'s text when asked.
pub(crate) fn with_text_fn<T>(f: &dyn Fn() -> String, call: impl FnOnce(OakTextFn, *mut c_void) -> T) -> T {
    unsafe extern "C" fn run(user: *mut c_void, out: OakSink, out_user: *mut c_void) {
        let f = unsafe { &*(user as *const &dyn Fn() -> String) };
        if let Some(text) = carry(f) {
            unsafe { out(out_user, text.as_ptr(), text.len()) };
        }
    }
    let t = call(run, &f as *const &dyn Fn() -> String as *mut c_void);
    rethrow();
    t
}

// ----------------------------------------------------------------- context

/// The window. It can be cloned (as egui's is; not `Copy`, so code written
/// for egui reads the same), kept and used from any thread (to ask
/// for a repaint); what reads input or draws works on the UI thread.
#[derive(Clone, Debug, Default)]
pub struct Context;

static CONTEXT: Context = Context;

/// What is typed and pointed this frame.
pub struct InputState {
    pub modifiers: Modifiers,
    /// Seconds since Oak started.
    pub time: f64,
    pub pointer: PointerState,
}

pub struct PointerState {
    pos: Option<Pos2>,
    down: bool,
    origin: Option<Pos2>,
}

impl PointerState {
    pub fn hover_pos(&self) -> Option<Pos2> {
        self.pos
    }

    pub fn interact_pos(&self) -> Option<Pos2> {
        self.pos
    }

    pub fn latest_pos(&self) -> Option<Pos2> {
        self.pos
    }

    pub fn primary_down(&self) -> bool {
        self.down
    }

    pub fn any_down(&self) -> bool {
        self.down
    }

    pub fn press_origin(&self) -> Option<Pos2> {
        self.origin
    }
}

impl InputState {
    fn now() -> InputState {
        let get = |what: u32| {
            let mut r = OakRect::default();
            let ok = unsafe { (api().input_get)(what, &mut r) };
            (ok, r)
        };
        let point = |what: u32| {
            let (ok, r) = get(what);
            ok.then(|| pos2(r.x0, r.y0))
        };
        InputState {
            modifiers: Modifiers::from_code(get(OAK_INPUT_MODIFIERS).1.x0 as u32),
            time: unsafe { (api().input_time)() },
            pointer: PointerState { pos: point(OAK_INPUT_POINTER), down: get(OAK_INPUT_POINTER_DOWN).0, origin: point(OAK_INPUT_PRESS_ORIGIN) },
        }
    }

    /// Whether `key` was pressed this frame, whatever modifiers are held.
    pub fn key_pressed(&self, key: Key) -> bool {
        unsafe { (api().input_key)(key.code(), OAK_KEY_ANY_MODS) }
    }

    /// Whether `key` was pressed with exactly `modifiers`; nothing else
    /// sees the press then.
    pub fn consume_key(&mut self, modifiers: Modifiers, key: Key) -> bool {
        unsafe { (api().input_key)(key.code(), modifiers.code() | OAK_KEY_CONSUME) }
    }

    pub fn smooth_scroll_delta(&self) -> Vec2 {
        let mut r = OakRect::default();
        unsafe { (api().input_get)(OAK_INPUT_SCROLL, &mut r) };
        vec2(r.x0, r.y0)
    }
}

impl Context {
    pub fn request_repaint(&self) {
        if crate::rt::has_api() {
            unsafe { (api().repaint)() };
        }
    }

    pub fn request_repaint_after(&self, after: std::time::Duration) {
        if crate::rt::has_api() {
            unsafe { (api().repaint_after)(after.as_millis() as u64) };
        }
    }

    pub fn input<R>(&self, f: impl FnOnce(&InputState) -> R) -> R {
        f(&InputState::now())
    }

    pub fn input_mut<R>(&self, f: impl FnOnce(&mut InputState) -> R) -> R {
        f(&mut InputState::now())
    }

    pub fn copy_text(&self, text: String) {
        unsafe { (api().copy_text)(s(&text)) };
    }

    pub fn open_url(&self, url: &str) {
        unsafe { (api().open_url)(s(url)) };
    }

    /// The window's whole area.
    pub fn screen_rect(&self) -> Rect {
        let mut r = OakRect::default();
        unsafe { (api().ui_get)(std::ptr::null_mut(), OAK_GET_SCREEN, &mut r) };
        rect_out(r)
    }

    /// How many pixels a point is.
    pub fn pixels_per_point(&self) -> f32 {
        let mut r = OakRect::default();
        unsafe { (api().ui_get)(std::ptr::null_mut(), OAK_GET_PIXELS_PER_POINT, &mut r) };
        if r.x1 > 0.0 { r.x1 } else { 1.0 }
    }

    pub fn memory<R>(&self, f: impl FnOnce(&Memory) -> R) -> R {
        f(&Memory)
    }

    pub fn memory_mut<R>(&self, f: impl FnOnce(&mut Memory) -> R) -> R {
        f(&mut Memory)
    }

    /// 0 to 1, moving toward `on` over a moment.
    pub fn animate_bool(&self, id: Id, on: bool) -> f32 {
        unsafe { (api().animate)(id.0, on) }
    }

    /// A texture from an image. Dropping the last handle frees it.
    pub fn load_texture(&self, name: impl Into<String>, image: ColorImage, options: TextureOptions) -> TextureHandle {
        let name = name.into();
        let id = unsafe { (api().texture_new)(s(&name), image.size[0] as u32, image.size[1] as u32, image.rgba.as_ptr(), options.0) };
        TextureHandle(Arc::new(Texture { id, size: vec2(image.size[0] as f32, image.size[1] as f32) }))
    }

    /// Per-id state, kept in the plugin between frames.
    pub fn data<R>(&self, f: impl FnOnce(&IdMap) -> R) -> R {
        f(&DATA.lock().unwrap())
    }

    pub fn data_mut<R>(&self, f: impl FnOnce(&mut IdMap) -> R) -> R {
        f(&mut DATA.lock().unwrap())
    }
}

static DATA: std::sync::LazyLock<Mutex<IdMap>> = std::sync::LazyLock::new(|| Mutex::new(IdMap::default()));

/// What `Ui::data` keeps: a value of each type under each id.
#[derive(Default)]
pub struct IdMap(HashMap<(u64, std::any::TypeId), Box<dyn Any + Send>>);

impl IdMap {
    pub fn get_temp<T: 'static + Clone + Send>(&self, id: Id) -> Option<T> {
        self.0.get(&(id.0, std::any::TypeId::of::<T>())).and_then(|v| v.downcast_ref::<T>()).cloned()
    }

    pub fn insert_temp<T: 'static + Clone + Send>(&mut self, id: Id, value: T) {
        self.0.insert((id.0, std::any::TypeId::of::<T>()), Box::new(value));
    }

    pub fn get_temp_mut_or<T: 'static + Clone + Send>(&mut self, id: Id, or: T) -> &mut T {
        self.0.entry((id.0, std::any::TypeId::of::<T>())).or_insert_with(|| Box::new(or)).downcast_mut::<T>().expect("a value of the type it was kept as")
    }

    pub fn get_temp_mut_or_default<T: 'static + Clone + Send + Default>(&mut self, id: Id) -> &mut T {
        self.get_temp_mut_or(id, T::default())
    }

    pub fn remove<T: 'static>(&mut self, id: Id) {
        self.0.remove(&(id.0, std::any::TypeId::of::<T>()));
    }

    pub fn remove_temp<T: 'static + Clone + Send>(&mut self, id: Id) -> Option<T> {
        self.0.remove(&(id.0, std::any::TypeId::of::<T>())).and_then(|v| v.downcast::<T>().ok()).map(|v| *v)
    }
}

/// Which widget has the keyboard (`Ui::memory`).
pub struct Memory;

impl Memory {
    pub fn has_focus(&self, id: Id) -> bool {
        unsafe { (api().focus)(id.0, false) }
    }

    pub fn request_focus(&mut self, id: Id) {
        unsafe { (api().focus)(id.0, true) };
    }
}

// ---------------------------------------------------------------- textures

/// Pixels for a texture: RGBA, not premultiplied.
pub struct ColorImage {
    pub size: [usize; 2],
    rgba: Vec<u8>,
}

impl ColorImage {
    pub fn from_rgba_unmultiplied(size: [usize; 2], rgba: &[u8]) -> ColorImage {
        assert_eq!(size[0] * size[1] * 4, rgba.len(), "an image's pixels are four bytes each");
        ColorImage { size, rgba: rgba.to_vec() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureOptions(u32);

impl TextureOptions {
    pub const LINEAR: TextureOptions = TextureOptions(OAK_TEXTURE_LINEAR);
    pub const NEAREST: TextureOptions = TextureOptions(0);
}

impl Default for TextureOptions {
    fn default() -> Self {
        TextureOptions::LINEAR
    }
}

struct Texture {
    id: u64,
    size: Vec2,
}

impl Drop for Texture {
    fn drop(&mut self) {
        if self.id != 0 && crate::rt::has_api() {
            unsafe { (api().texture_free)(self.id) };
        }
    }
}

/// A texture Oak holds; the last clone dropped frees it.
#[derive(Clone)]
pub struct TextureHandle(Arc<Texture>);

/// A texture, by its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextureId(pub u64);

impl TextureHandle {
    /// From an image file's bytes (PNG, JPEG, GIF, WebP): Oak decodes it.
    pub fn decode(name: &str, file: &[u8], options: TextureOptions) -> Option<TextureHandle> {
        let mut size = OakRect::default();
        let id = unsafe { (api().texture_decode)(s(name), bytes(file), options.0, &mut size) };
        (id != 0).then(|| TextureHandle(Arc::new(Texture { id, size: vec2(size.x1, size.y1) })))
    }

    pub fn id(&self) -> TextureId {
        TextureId(self.0.id)
    }

    /// In pixels.
    pub fn size_vec2(&self) -> Vec2 {
        self.0.size
    }

    pub fn size(&self) -> [usize; 2] {
        [self.0.size.x as usize, self.0.size.y as usize]
    }

    pub fn aspect_ratio(&self) -> f32 {
        self.0.size.x / self.0.size.y.max(1.0)
    }
}

impl From<&TextureHandle> for TextureId {
    fn from(t: &TextureHandle) -> TextureId {
        t.id()
    }
}

// ----------------------------------------------------------------- painter

/// Paints in the window, clipped to its `Ui` (and to a rectangle of its
/// own, `with_clip_rect`).
#[derive(Clone)]
pub struct Painter<'a> {
    ui: *mut OakUi,
    clip: Option<OakRect>,
    _ui: PhantomData<&'a Ui>,
}

/// A shape for `Painter::add`.
pub enum Shape {
    Line { points: Vec<Pos2>, stroke: Stroke, closed: bool },
    Polygon { points: Vec<Pos2>, fill: Color32, stroke: Stroke },
}

impl Shape {
    pub fn line(points: Vec<Pos2>, stroke: impl Into<Stroke>) -> Shape {
        Shape::Line { points, stroke: stroke.into(), closed: false }
    }

    pub fn closed_line(points: Vec<Pos2>, stroke: impl Into<Stroke>) -> Shape {
        Shape::Line { points, stroke: stroke.into(), closed: true }
    }

    pub fn convex_polygon(points: Vec<Pos2>, fill: impl Into<Color32>, stroke: impl Into<Stroke>) -> Shape {
        Shape::Polygon { points, fill: fill.into(), stroke: stroke.into() }
    }
}

impl<'a> Painter<'a> {
    pub fn ctx(&self) -> &Context {
        &CONTEXT
    }

    pub(crate) fn raw(&self) -> *mut OakUi {
        self.ui
    }

    fn clip(&self) -> *const OakRect {
        self.clip.as_ref().map_or(std::ptr::null(), |c| c as *const OakRect)
    }

    /// The same painter, drawing only inside `rect`.
    pub fn with_clip_rect(&self, rect: Rect) -> Painter<'a> {
        let rect = self.clip.map_or(rect, |c| rect_out(c).intersect(rect));
        Painter { ui: self.ui, clip: Some(rect_in(rect)), _ui: PhantomData }
    }

    pub fn rect_filled(&self, rect: Rect, radius: impl Into<CornerRadius>, fill: impl Into<Color32>) {
        self.rect(rect, radius, fill, Stroke::NONE, StrokeKind::Middle);
    }

    pub fn rect_stroke(&self, rect: Rect, radius: impl Into<CornerRadius>, stroke: impl Into<Stroke>, kind: StrokeKind) {
        self.rect(rect, radius, Color32::TRANSPARENT, stroke, kind);
    }

    pub fn rect(&self, rect: Rect, radius: impl Into<CornerRadius>, fill: impl Into<Color32>, stroke: impl Into<Stroke>, kind: StrokeKind) {
        let (radius, stroke, fill) = (radius.into(), stroke.into(), color_in(fill.into()));
        let [nw, ne, sw, se] = radius.all();
        if radius.is_same() {
            unsafe { (api().paint_rect)(self.ui, &rect_in(rect), nw, fill, stroke.width, color_in(stroke.color), kind as u32, self.clip()) };
        } else {
            unsafe { (api().paint_rect_corners)(self.ui, &rect_in(rect), nw, ne, sw, se, fill, stroke.width, color_in(stroke.color), kind as u32, self.clip()) };
        }
    }

    pub fn line_segment(&self, points: [Pos2; 2], stroke: impl Into<Stroke>) {
        let stroke = stroke.into();
        unsafe { (api().paint_line)(self.ui, points[0].x, points[0].y, points[1].x, points[1].y, stroke.width, color_in(stroke.color), self.clip()) };
    }

    pub fn hline(&self, x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) {
        let x = x.into();
        self.line_segment([pos2(x.min, y), pos2(x.max, y)], stroke);
    }

    pub fn vline(&self, x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) {
        let y = y.into();
        self.line_segment([pos2(x, y.min), pos2(x, y.max)], stroke);
    }

    pub fn circle_filled(&self, center: Pos2, radius: f32, fill: impl Into<Color32>) {
        unsafe { (api().paint_circle)(self.ui, center.x, center.y, radius, color_in(fill.into()), 0.0, 0, self.clip()) };
    }

    pub fn circle_stroke(&self, center: Pos2, radius: f32, stroke: impl Into<Stroke>) {
        let stroke = stroke.into();
        unsafe { (api().paint_circle)(self.ui, center.x, center.y, radius, 0, stroke.width, color_in(stroke.color), self.clip()) };
    }

    pub fn circle(&self, center: Pos2, radius: f32, fill: impl Into<Color32>, stroke: impl Into<Stroke>) {
        let stroke = stroke.into();
        unsafe { (api().paint_circle)(self.ui, center.x, center.y, radius, color_in(fill.into()), stroke.width, color_in(stroke.color), self.clip()) };
    }

    pub fn add(&self, shape: Shape) {
        let (points, stroke, fill, closed) = match shape {
            Shape::Line { points, stroke, closed } => (points, stroke, 0, closed),
            Shape::Polygon { points, fill, stroke } => (points, stroke, color_in(fill), true),
        };
        let flat: Vec<f32> = points.iter().flat_map(|p| [p.x, p.y]).collect();
        unsafe { (api().paint_path)(self.ui, flat.as_ptr(), points.len(), stroke.width, color_in(stroke.color), fill, closed, self.clip()) };
    }

    /// Text with its `anchor` point at `pos`; where it went.
    pub fn text(&self, pos: Pos2, anchor: Align2, text: impl ToString, font: FontId, color: Color32) -> Rect {
        let t = RichText::new(text.to_string()).font(font).color(color);
        self.text_wrapped(pos, anchor, &t, 0.0)
    }

    /// The same, of any look, wrapped at `width` (0: one line).
    pub fn text_wrapped(&self, pos: Pos2, anchor: Align2, text: &RichText, width: f32) -> Rect {
        let mut out = OakRect::default();
        unsafe { (api().paint_text)(self.ui, pos.x, pos.y, anchor_code(anchor), &text.raw(0), width, &mut out, self.clip()) };
        rect_out(out)
    }

    pub fn image(&self, texture: TextureId, rect: Rect, uv: Rect, tint: Color32) {
        unsafe { (api().paint_image)(self.ui, texture.0, &rect_in(rect), &rect_in(uv), color_in(tint), 0.0, self.clip()) };
    }
}

// ---------------------------------------------------------------------- ui

/// Somewhere to draw, as Oak lays it out.
pub struct Ui {
    raw: *mut OakUi,
    /// What `painter` lends.
    painter: Painter<'static>,
}

/// The spacing between and inside widgets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spacing {
    pub item_spacing: Vec2,
    pub button_padding: Vec2,
    pub interact_size: Vec2,
    pub indent: f32,
}

/// `Ui::spacing_mut`: what is changed is set when it goes.
pub struct SpacingMut<'a> {
    ui: &'a mut Ui,
    was: Spacing,
    now: Spacing,
}

impl Deref for SpacingMut<'_> {
    type Target = Spacing;

    fn deref(&self) -> &Spacing {
        &self.now
    }
}

impl DerefMut for SpacingMut<'_> {
    fn deref_mut(&mut self) -> &mut Spacing {
        &mut self.now
    }
}

impl Drop for SpacingMut<'_> {
    fn drop(&mut self) {
        let (was, now) = (self.was, self.now);
        if now.item_spacing != was.item_spacing {
            self.ui.set(OAK_SET_ITEM_SPACING, now.item_spacing.x, now.item_spacing.y);
        }
        if now.button_padding != was.button_padding {
            self.ui.set(OAK_SET_BUTTON_PADDING, now.button_padding.x, now.button_padding.y);
        }
        if now.interact_size != was.interact_size {
            self.ui.set(OAK_SET_INTERACT_SIZE, now.interact_size.x, now.interact_size.y);
        }
        if now.indent != was.indent {
            self.ui.set(OAK_SET_INDENT, now.indent, 0.0);
        }
    }
}

/// How a part of a `Ui` is made (`Ui::scope_builder`).
#[derive(Clone, Copy, Debug, Default)]
pub struct UiBuilder {
    rect: Option<Rect>,
    layout: Option<Layout>,
    id: u64,
}

impl UiBuilder {
    pub fn new() -> UiBuilder {
        UiBuilder::default()
    }

    pub fn max_rect(mut self, rect: Rect) -> UiBuilder {
        self.rect = Some(rect);
        self
    }

    pub fn layout(mut self, layout: Layout) -> UiBuilder {
        self.layout = Some(layout);
        self
    }

    pub fn id_salt(mut self, salt: impl Hash) -> UiBuilder {
        self.id = Id::new(salt).0;
        self
    }
}

/// Something `Ui::add` draws.
pub trait Widget {
    fn ui(self, ui: &mut Ui) -> Response;

    /// Draws it greyed, taking no clicks (`Ui::add_enabled`).
    fn ui_disabled(self, ui: &mut Ui) -> Response
    where
        Self: Sized,
    {
        ui.row(OAK_DISABLED, |ui| self.ui(ui)).inner
    }
}

impl Ui {
    pub(crate) fn raw(raw: *mut OakUi) -> Ui {
        Ui { raw, painter: Painter { ui: raw, clip: None, _ui: PhantomData } }
    }

    /// The `Ui` behind a handle Oak gave (a callback's `ui`).
    ///
    /// # Safety
    /// `raw` is one Oak handed out, used only inside the callback it came with.
    pub unsafe fn from_raw(raw: *mut OakUi) -> Ui {
        Ui::raw(raw)
    }

    /// The handle, for calling the C interface directly.
    pub fn as_raw(&mut self) -> *mut OakUi {
        self.raw
    }

    pub fn ctx(&self) -> &Context {
        &CONTEXT
    }

    pub fn painter(&self) -> &Painter<'_> {
        &self.painter
    }

    /// A painter that draws only inside `rect`.
    pub fn painter_at(&self, rect: Rect) -> Painter<'_> {
        self.painter().with_clip_rect(rect)
    }

    /// What has the keyboard.
    pub fn memory<R>(&self, f: impl FnOnce(&Memory) -> R) -> R {
        f(&Memory)
    }

    pub fn memory_mut<R>(&self, f: impl FnOnce(&mut Memory) -> R) -> R {
        f(&mut Memory)
    }

    pub fn id(&self) -> Id {
        Id(unsafe { (api().ui_id)(self.raw) })
    }

    pub fn make_persistent_id(&self, salt: impl Hash) -> Id {
        self.id().with(salt)
    }

    fn get(&self, what: u32) -> OakRect {
        let mut r = OakRect::default();
        unsafe { (api().ui_get)(self.raw, what, &mut r) };
        r
    }

    fn set(&self, what: u32, a: f32, b: f32) {
        unsafe { (api().ui_set)(self.raw, what, a, b, 0.0, 0.0) };
    }

    pub fn available_size(&self) -> Vec2 {
        let r = self.get(OAK_GET_AVAILABLE);
        vec2(r.x1, r.y1)
    }

    pub fn available_width(&self) -> f32 {
        self.available_size().x
    }

    pub fn available_height(&self) -> f32 {
        self.available_size().y
    }

    pub fn available_rect_before_wrap(&self) -> Rect {
        rect_out(self.get(OAK_GET_AVAILABLE_RECT))
    }

    pub fn max_rect(&self) -> Rect {
        rect_out(self.get(OAK_GET_MAX_RECT))
    }

    pub fn min_rect(&self) -> Rect {
        rect_out(self.get(OAK_GET_MIN_RECT))
    }

    pub fn clip_rect(&self) -> Rect {
        rect_out(self.get(OAK_GET_CLIP_RECT))
    }

    pub fn set_clip_rect(&mut self, r: Rect) {
        unsafe { (api().ui_set)(self.raw, OAK_SET_CLIP_RECT, r.min.x, r.min.y, r.max.x, r.max.y) };
    }

    /// Where the next widget goes.
    pub fn cursor(&self) -> Rect {
        let r = self.get(OAK_GET_CURSOR);
        Rect::from_min_max(pos2(r.x0, r.y0), pos2(r.x1, r.y1))
    }

    pub fn next_widget_position(&self) -> Pos2 {
        let r = self.get(OAK_GET_NEXT_POSITION);
        pos2(r.x0, r.y0)
    }

    pub fn spacing(&self) -> Spacing {
        let v = |what: u32| {
            let r = self.get(what);
            vec2(r.x1, r.y1)
        };
        Spacing { item_spacing: v(OAK_GET_ITEM_SPACING), button_padding: v(OAK_GET_BUTTON_PADDING), interact_size: v(OAK_GET_INTERACT_SIZE), indent: self.get(OAK_GET_INDENT).x1 }
    }

    pub fn spacing_mut(&mut self) -> SpacingMut<'_> {
        let was = self.spacing();
        SpacingMut { ui: self, was, now: was }
    }

    /// A line's height in the body font.
    pub fn text_row_height(&self) -> f32 {
        self.get(OAK_GET_ROW_HEIGHT).y1
    }

    pub fn set_width(&mut self, w: f32) {
        self.set(OAK_SET_WIDTH, w, 0.0);
    }

    pub fn set_max_width(&mut self, w: f32) {
        self.set(OAK_SET_MAX_WIDTH, w, 0.0);
    }

    pub fn set_min_width(&mut self, w: f32) {
        self.set(OAK_SET_MIN_WIDTH, w, 0.0);
    }

    pub fn set_height(&mut self, h: f32) {
        self.set(OAK_SET_HEIGHT, h, 0.0);
    }

    pub fn set_max_height(&mut self, h: f32) {
        self.set(OAK_SET_MAX_HEIGHT, h, 0.0);
    }

    pub fn set_min_height(&mut self, h: f32) {
        self.set(OAK_SET_MIN_HEIGHT, h, 0.0);
    }

    pub fn set_min_size(&mut self, size: Vec2) {
        self.set_min_width(size.x);
        self.set_min_height(size.y);
    }

    pub fn multiply_opacity(&mut self, opacity: f32) {
        self.set(OAK_SET_OPACITY, opacity, 0.0);
    }

    pub fn disable(&mut self) {
        self.set(OAK_SET_ENABLED, 0.0, 0.0);
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.disable();
        }
    }

    pub fn set_wrap_mode(&mut self, mode: TextWrapMode) {
        self.set(OAK_SET_WRAP, mode as u32 as f32, 0.0);
    }

    /// The text color of widgets that have none of their own.
    pub fn set_text_color(&mut self, color: Color32) {
        unsafe { (api().ui_set)(self.raw, OAK_SET_TEXT_COLOR, f32::from_bits(color_in(color)), 0.0, 0.0, 0.0) };
    }

    pub fn add_space(&mut self, amount: f32) {
        unsafe { (api().ui_add_space)(self.raw, amount) };
    }

    pub fn separator(&mut self) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_separator)(self.raw, &mut out) };
        Response::of(out)
    }

    pub fn end_row(&mut self) {
        unsafe { (api().ui_end_row)(self.raw) };
    }

    /// Closes the menu or popup this is in.
    pub fn close(&self) {
        unsafe { (api().ui_close)(self.raw) };
    }

    pub fn close_menu(&self) {
        self.close();
    }

    pub fn input<R>(&self, f: impl FnOnce(&InputState) -> R) -> R {
        CONTEXT.input(f)
    }

    pub fn input_mut<R>(&self, f: impl FnOnce(&mut InputState) -> R) -> R {
        CONTEXT.input_mut(f)
    }

    pub fn data<R>(&self, f: impl FnOnce(&IdMap) -> R) -> R {
        CONTEXT.data(f)
    }

    pub fn data_mut<R>(&self, f: impl FnOnce(&mut IdMap) -> R) -> R {
        CONTEXT.data_mut(f)
    }

    // -- containers

    fn row<R>(&mut self, kind: u32, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let raw = self.raw;
        contained(f, |body, user, out| unsafe { (api().ui_row)(raw, kind, body, user, out) })
    }

    pub fn horizontal<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_ROW, f)
    }

    pub fn horizontal_wrapped<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_ROW_WRAPPED, f)
    }

    pub fn horizontal_top<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_ROW_TOP, f)
    }

    pub fn vertical<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_COLUMN, f)
    }

    pub fn vertical_centered<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_COLUMN_CENTERED, f)
    }

    pub fn vertical_centered_justified<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_COLUMN_CENTERED_JUSTIFIED, f)
    }

    pub fn scope<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(OAK_SCOPE, f)
    }

    pub fn indent<R>(&mut self, salt: impl Hash, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        // (The salt tells what is kept under ids in it from the same in
        // another indent beside it.)
        self.push_id(salt, |ui| ui.row(OAK_INDENT, f).inner)
    }

    pub fn add_enabled_ui<R>(&mut self, enabled: bool, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.row(if enabled { OAK_SCOPE } else { OAK_DISABLED }, f)
    }

    pub fn with_layout<R>(&mut self, layout: Layout, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let raw = self.raw;
        contained(f, |body, user, out| unsafe { (api().ui_layout)(raw, &layout.raw(), body, user, out) })
    }

    fn child<R>(&mut self, c: OakChild, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let raw = self.raw;
        contained(f, |body, user, out| unsafe { (api().ui_child)(raw, &c, body, user, out) })
    }

    pub fn scope_builder<R>(&mut self, b: UiBuilder, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let mut c = OakChild { size: std::mem::size_of::<OakChild>() as u32, id: b.id, ..Default::default() };
        if let Some(r) = b.rect {
            c.flags |= OAK_CHILD_RECT;
            c.rect = rect_in(r);
        }
        let layout = b.layout.map(|l| l.raw());
        if let Some(l) = &layout {
            c.flags |= OAK_CHILD_LAYOUT;
            c.layout = l;
        }
        self.child(c, f)
    }

    /// A part of this `Ui` at `rect`, which doesn't move the cursor.
    pub fn allocate_new_ui<R>(&mut self, b: UiBuilder, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        self.scope_builder(b, f)
    }

    pub fn allocate_ui<R>(&mut self, size: Vec2, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let c = OakChild { size: std::mem::size_of::<OakChild>() as u32, flags: OAK_CHILD_SIZE, width: size.x, height: size.y, ..Default::default() };
        self.child(c, f)
    }

    pub fn allocate_ui_with_layout<R>(&mut self, size: Vec2, layout: Layout, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let layout = layout.raw();
        let c = OakChild { size: std::mem::size_of::<OakChild>() as u32, flags: OAK_CHILD_SIZE | OAK_CHILD_LAYOUT, width: size.x, height: size.y, layout: &layout, ..Default::default() };
        self.child(c, f)
    }

    pub fn push_id<R>(&mut self, id: impl Hash, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let raw = self.raw;
        let id = Id::new(id).0;
        contained(f, |body, user, out| unsafe { (api().ui_push_id)(raw, id, body, user, out) })
    }

    pub fn collapsing<R>(&mut self, title: impl Into<RichText>, f: impl FnOnce(&mut Ui) -> R) -> CollapsingResponse<R> {
        CollapsingHeader::new(title).show(self, f)
    }

    pub fn menu_button<R>(&mut self, title: impl Into<RichText>, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>> {
        let raw = self.raw;
        let title = title.into();
        let mut out = OakResponse::default();
        let (inner, _) = with_body(f, |body, user| unsafe { (api().ui_menu_button)(raw, &title.raw(0), body, user, &mut out) });
        InnerResponse { inner, response: Response::of(out) }
    }

    // -- taking room

    pub fn allocate_exact_size(&mut self, size: Vec2, sense: Sense) -> (Rect, Response) {
        let mut out = OakResponse::default();
        unsafe { (api().ui_allocate)(self.raw, size.x, size.y, sense.0, &mut out) };
        let r = Response::of(out);
        (r.rect, r)
    }

    pub fn allocate_response(&mut self, size: Vec2, sense: Sense) -> Response {
        self.allocate_exact_size(size, sense).1
    }

    pub fn allocate_space(&mut self, size: Vec2) -> (Id, Rect) {
        let (rect, r) = self.allocate_exact_size(size, Sense::hover());
        (r.id, rect)
    }

    pub fn allocate_rect(&mut self, rect: Rect, sense: Sense) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_allocate_rect)(self.raw, &rect_in(rect), sense.0, &mut out) };
        Response::of(out)
    }

    /// Senses in `rect` under `id`, taking no room.
    pub fn interact(&self, rect: Rect, id: Id, sense: Sense) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_interact)(self.raw, &rect_in(rect), id.0, sense.0, &mut out) };
        Response::of(out)
    }

    // -- widgets

    pub fn add(&mut self, widget: impl Widget) -> Response {
        widget.ui(self)
    }

    pub fn add_enabled(&mut self, enabled: bool, widget: impl Widget) -> Response {
        if enabled { widget.ui(self) } else { widget.ui_disabled(self) }
    }

    pub fn add_sized(&mut self, size: impl Into<Vec2>, widget: impl Widget) -> Response {
        let layout = Layout::top_down_justified(Align::Center).with_main_align(Align::Center).with_main_justify(true);
        self.allocate_ui_with_layout(size.into(), layout, |ui| widget.ui(ui)).inner
    }

    /// `widget` in `rect`, which doesn't move the cursor.
    pub fn put(&mut self, rect: Rect, widget: impl Widget) -> Response {
        let layout = Layout::top_down_justified(Align::Center).with_main_align(Align::Center).with_main_justify(true);
        self.scope_builder(UiBuilder::new().max_rect(rect).layout(layout), |ui| widget.ui(ui)).inner
    }

    pub fn label(&mut self, text: impl Into<RichText>) -> Response {
        Label::new(text).ui(self)
    }

    pub fn heading(&mut self, text: impl Into<RichText>) -> Response {
        Label::new(text.into().heading()).ui(self)
    }

    pub fn small(&mut self, text: impl Into<RichText>) -> Response {
        Label::new(text.into().small()).ui(self)
    }

    pub fn weak(&mut self, text: impl Into<RichText>) -> Response {
        Label::new(text.into().weak()).ui(self)
    }

    pub fn strong(&mut self, text: impl Into<RichText>) -> Response {
        Label::new(text.into().strong()).ui(self)
    }

    pub fn monospace(&mut self, text: impl Into<RichText>) -> Response {
        Label::new(text.into().monospace()).ui(self)
    }

    pub fn colored_label(&mut self, color: impl Into<Color32>, text: impl Into<RichText>) -> Response {
        Label::new(text.into().color(color)).ui(self)
    }

    pub fn button(&mut self, text: impl Into<RichText>) -> Response {
        Button::new(text).ui(self)
    }

    pub fn small_button(&mut self, text: impl Into<RichText>) -> Response {
        Button::new(text).small().ui(self)
    }

    pub fn link(&mut self, text: impl Into<RichText>) -> Response {
        Button::new(text).flag(OAK_BUTTON_LINK).ui(self)
    }

    pub fn hyperlink_to(&mut self, text: impl Into<RichText>, url: impl ToString) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_hyperlink)(self.raw, &text.into().raw(0), s(&url.to_string()), &mut out) };
        Response::of(out)
    }

    pub fn hyperlink(&mut self, url: impl ToString) -> Response {
        let url = url.to_string();
        self.hyperlink_to(url.clone(), url)
    }

    pub fn selectable_label(&mut self, selected: bool, text: impl Into<RichText>) -> Response {
        Button::selectable(selected, text).ui(self)
    }

    pub fn selectable_value<V: PartialEq>(&mut self, current: &mut V, value: V, text: impl Into<RichText>) -> Response {
        let mut r = self.selectable_label(*current == value, text);
        if r.clicked() && *current != value {
            *current = value;
            r.mark_changed();
        }
        r
    }

    pub fn toggle_value(&mut self, on: &mut bool, text: impl Into<RichText>) -> Response {
        let mut r = self.selectable_label(*on, text);
        if r.clicked() {
            *on = !*on;
            r.mark_changed();
        }
        r
    }

    pub fn checkbox(&mut self, on: &mut bool, text: impl Into<RichText>) -> Response {
        Checkbox::new(on, text).ui(self)
    }

    pub fn spinner(&mut self) -> Response {
        Spinner::new().ui(self)
    }

    pub fn text_edit_singleline(&mut self, text: &mut String) -> Response {
        TextEdit::singleline(text).ui(self)
    }

    pub fn text_edit_multiline(&mut self, text: &mut String) -> Response {
        TextEdit::multiline(text).ui(self)
    }

    pub fn image(&mut self, image: Image) -> Response {
        image.ui(self)
    }
}

// ----------------------------------------------------------------- widgets

/// Text in the layout.
pub struct Label {
    text: RichText,
    flags: u32,
}

impl Label {
    pub fn new(text: impl Into<RichText>) -> Label {
        Label { text: text.into(), flags: 0 }
    }

    pub fn wrap(mut self) -> Label {
        self.flags |= OAK_TEXT_WRAP;
        self
    }

    pub fn truncate(mut self) -> Label {
        self.flags |= OAK_TEXT_TRUNCATE;
        self
    }

    pub fn extend(mut self) -> Label {
        self.flags |= OAK_TEXT_EXTEND;
        self
    }

    pub fn wrap_mode(self, mode: TextWrapMode) -> Label {
        match mode {
            TextWrapMode::Extend => self.extend(),
            TextWrapMode::Wrap => self.wrap(),
            TextWrapMode::Truncate => self.truncate(),
        }
    }

    /// Where its lines start: `Align::LEFT` keeps them at the left also
    /// where the layout puts widgets at the right.
    pub fn halign(mut self, align: Align) -> Label {
        if align == Align::Min {
            self.flags |= OAK_TEXT_LEFT;
        }
        self
    }

    pub fn selectable(mut self, on: bool) -> Label {
        if !on {
            self.flags |= OAK_TEXT_NO_SELECT;
        }
        self
    }

    pub fn sense(mut self, sense: Sense) -> Label {
        if sense != Sense::hover() {
            self.flags |= OAK_TEXT_CLICK;
        }
        self
    }
}

impl Widget for Label {
    fn ui(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_label)(ui.raw, &self.text.raw(self.flags), &mut out) };
        Response::of(out)
    }

    fn ui_disabled(mut self, ui: &mut Ui) -> Response {
        self.flags |= OAK_TEXT_DISABLED;
        self.ui(ui)
    }
}

impl Widget for RichText {
    fn ui(self, ui: &mut Ui) -> Response {
        Label::new(self).ui(ui)
    }
}

impl Widget for &str {
    fn ui(self, ui: &mut Ui) -> Response {
        Label::new(self).ui(ui)
    }
}

pub struct Button {
    text: RichText,
    flags: u32,
    fill: Option<Color32>,
    stroke: Option<Stroke>,
    min_size: Vec2,
    image: Option<(TextureId, Vec2)>,
}

impl Button {
    pub fn new(text: impl Into<RichText>) -> Button {
        Button { text: text.into(), flags: 0, fill: None, stroke: None, min_size: Vec2::ZERO, image: None }
    }

    /// A picture `size` large, then the text.
    pub fn image_and_text(texture: impl Into<TextureId>, size: Vec2, text: impl Into<RichText>) -> Button {
        Button { image: Some((texture.into(), size)), ..Button::new(text) }
    }

    /// A choice among others: framed only when it is selected or under
    /// the pointer.
    pub fn selectable(selected: bool, text: impl Into<RichText>) -> Button {
        Button::new(text).selected(selected).flag(OAK_BUTTON_SELECTABLE)
    }

    fn flag(mut self, f: u32) -> Button {
        self.flags |= f;
        self
    }

    pub fn small(self) -> Button {
        self.flag(OAK_BUTTON_SMALL)
    }

    pub fn selected(self, on: bool) -> Button {
        if on { self.flag(OAK_BUTTON_SELECTED) } else { self }
    }

    pub fn frame(self, on: bool) -> Button {
        if on { self } else { self.flag(OAK_BUTTON_NO_FRAME) }
    }

    pub fn wrap(self) -> Button {
        self.flag(OAK_BUTTON_WRAP)
    }

    pub fn fill(mut self, fill: impl Into<Color32>) -> Button {
        self.fill = Some(fill.into());
        self
    }

    pub fn stroke(mut self, stroke: impl Into<Stroke>) -> Button {
        self.stroke = Some(stroke.into());
        self
    }

    pub fn min_size(mut self, size: Vec2) -> Button {
        self.min_size = size;
        self
    }
}

impl Widget for Button {
    fn ui(self, ui: &mut Ui) -> Response {
        let stroke = self.stroke.unwrap_or(Stroke::NONE);
        let (image, image_size) = self.image.map_or((0, Vec2::ZERO), |(t, size)| (t.0, size));
        let text = self.text.raw(0);
        let b = OakButton {
            size: std::mem::size_of::<OakButton>() as u32,
            flags: self.flags,
            text: &text,
            fill: self.fill.map_or(0, color_in),
            stroke_width: stroke.width,
            stroke_color: color_in(stroke.color),
            min_width: self.min_size.x,
            min_height: self.min_size.y,
            image,
            image_width: image_size.x,
            image_height: image_size.y,
        };
        let mut out = OakResponse::default();
        unsafe { (api().ui_button)(ui.raw, &b, &mut out) };
        Response::of(out)
    }

    fn ui_disabled(self, ui: &mut Ui) -> Response {
        self.flag(OAK_BUTTON_DISABLED).ui(ui)
    }
}

pub struct Checkbox<'a> {
    on: &'a mut bool,
    text: RichText,
    flags: u32,
}

impl<'a> Checkbox<'a> {
    pub fn new(on: &'a mut bool, text: impl Into<RichText>) -> Checkbox<'a> {
        Checkbox { on, text: text.into(), flags: 0 }
    }

    pub fn without_text(on: &'a mut bool) -> Checkbox<'a> {
        Checkbox { on, text: RichText::default(), flags: 0 }
    }
}

impl Widget for Checkbox<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_checkbox)(ui.raw, self.on, &self.text.raw(self.flags), &mut out) };
        Response::of(out)
    }

    fn ui_disabled(mut self, ui: &mut Ui) -> Response {
        self.flags |= OAK_TEXT_DISABLED;
        self.ui(ui)
    }
}

#[derive(Default)]
pub struct Spinner {
    size: f32,
    color: Option<Color32>,
}

impl Spinner {
    pub fn new() -> Spinner {
        Spinner::default()
    }

    pub fn size(mut self, size: f32) -> Spinner {
        self.size = size;
        self
    }

    pub fn color(mut self, color: impl Into<Color32>) -> Spinner {
        self.color = Some(color.into());
        self
    }
}

impl Widget for Spinner {
    fn ui(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_spinner)(ui.raw, self.size, self.color.map_or(0, color_in), &mut out) };
        Response::of(out)
    }
}

/// A number, as `DragValue` edits it.
pub trait Numeric: Copy {
    fn to_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;
    const INTEGER: bool;
    /// The least and the most it can be.
    const MIN: f64;
    const MAX: f64;
}

macro_rules! numeric {
    ($int:literal: $($t:ty),*) => { $(impl Numeric for $t {
        fn to_f64(self) -> f64 { self as f64 }
        fn from_f64(v: f64) -> Self { v as $t }
        const INTEGER: bool = $int;
        const MIN: f64 = <$t>::MIN as f64;
        const MAX: f64 = <$t>::MAX as f64;
    })* };
}
numeric!(true: u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
numeric!(false: f32, f64);

pub struct DragValue<'a> {
    get: Box<dyn FnMut(Option<f64>) -> f64 + 'a>,
    drag: OakDrag,
    prefix: String,
    suffix: String,
    /// How the number shows, when not as a number.
    format: Option<NumberText<'a>>,
}

type NumberText<'a> = Box<dyn Fn(f64, std::ops::RangeInclusive<usize>) -> String + 'a>;

impl<'a> DragValue<'a> {
    pub fn new<N: Numeric>(value: &'a mut N) -> DragValue<'a> {
        let mut d = OakDrag { size: std::mem::size_of::<OakDrag>() as u32, ..Default::default() };
        // (As egui's: a whole number shows no decimals, stays one of its
        // type's, and takes four points of dragging to change by one.)
        if N::INTEGER {
            d.flags |= OAK_DRAG_MAX_DECIMALS | OAK_DRAG_RANGE;
            (d.min, d.max, d.speed) = (N::MIN, N::MAX, 0.25);
        }
        DragValue {
            get: Box::new(move |set| {
                if let Some(v) = set {
                    *value = N::from_f64(v);
                }
                value.to_f64()
            }),
            drag: d,
            prefix: String::new(),
            suffix: String::new(),
            format: None,
        }
    }

    pub fn range<N: Numeric>(mut self, range: std::ops::RangeInclusive<N>) -> DragValue<'a> {
        self.drag.flags |= OAK_DRAG_RANGE;
        (self.drag.min, self.drag.max) = (range.start().to_f64(), range.end().to_f64());
        self
    }

    pub fn speed(mut self, speed: f64) -> DragValue<'a> {
        self.drag.speed = speed;
        self
    }

    pub fn prefix(mut self, p: impl ToString) -> DragValue<'a> {
        self.prefix = p.to_string();
        self
    }

    pub fn suffix(mut self, p: impl ToString) -> DragValue<'a> {
        self.suffix = p.to_string();
        self
    }

    pub fn fixed_decimals(mut self, n: usize) -> DragValue<'a> {
        self.drag.flags = (self.drag.flags | OAK_DRAG_DECIMALS) & !OAK_DRAG_MAX_DECIMALS;
        self.drag.decimals = n as u32;
        self
    }

    pub fn max_decimals(mut self, n: usize) -> DragValue<'a> {
        self.drag.flags = (self.drag.flags | OAK_DRAG_MAX_DECIMALS) & !OAK_DRAG_DECIMALS;
        self.drag.decimals = n as u32;
        self
    }

    /// Shows the number as `f` writes it (given the number, and how many
    /// decimals it would show with).
    pub fn custom_formatter(mut self, f: impl Fn(f64, std::ops::RangeInclusive<usize>) -> String + 'a) -> DragValue<'a> {
        self.format = Some(Box::new(f));
        self
    }
}

unsafe extern "C" fn number_text(user: *mut c_void, value: f64, out: OakSink, out_user: *mut c_void) {
    let (format, decimals) = unsafe { &*(user as *const (NumberText<'_>, usize)) };
    if let Some(text) = crate::rt::carry(|| format(value, 0..=*decimals)) {
        unsafe { out(out_user, text.as_ptr(), text.len()) };
    }
}

impl Widget for DragValue<'_> {
    fn ui(mut self, ui: &mut Ui) -> Response {
        let before = (self.get)(None);
        let mut v = before;
        let mut d = self.drag;
        (d.prefix, d.suffix) = (s(&self.prefix), s(&self.suffix));
        let format = self.format.take().map(|f| (f, d.decimals as usize));
        if let Some(format) = &format {
            d.format = Some(number_text as OakNumberText);
            d.user = format as *const (NumberText<'_>, usize) as *mut c_void;
        }
        let mut out = OakResponse::default();
        unsafe { (api().ui_drag)(ui.raw, &mut v, &d, &mut out) };
        crate::rt::rethrow();
        if v != before {
            (self.get)(Some(v));
        }
        Response::of(out)
    }
}

/// A text field over a `String` of the plugin's.
pub struct TextEdit<'a> {
    text: &'a mut String,
    edit: OakTextEdit,
    hint: String,
    /// Mixed with the `Ui`'s id when it is drawn.
    salt: Option<u64>,
}

impl<'a> TextEdit<'a> {
    fn new(text: &'a mut String, flags: u32) -> TextEdit<'a> {
        TextEdit { text, edit: OakTextEdit { size: std::mem::size_of::<OakTextEdit>() as u32, flags, ..Default::default() }, hint: String::new(), salt: None }
    }

    pub fn singleline(text: &'a mut String) -> TextEdit<'a> {
        TextEdit::new(text, 0)
    }

    pub fn multiline(text: &'a mut String) -> TextEdit<'a> {
        TextEdit::new(text, OAK_EDIT_MULTILINE)
    }

    pub(crate) fn flag(mut self, f: u32, on: bool) -> TextEdit<'a> {
        self.edit.flags = if on { self.edit.flags | f } else { self.edit.flags & !f };
        self
    }

    pub fn id(mut self, id: Id) -> TextEdit<'a> {
        self.edit.id = id.0;
        self
    }

    pub fn id_salt(mut self, salt: impl Hash) -> TextEdit<'a> {
        self.salt = Some(Id::new(salt).0);
        self
    }

    /// Enter doesn't make a newline (Shift+Enter does): the message is sent.
    pub fn enter_sends(self) -> TextEdit<'a> {
        self.flag(OAK_EDIT_ENTER_SENDS, true)
    }

    pub fn hint_text(mut self, hint: impl Into<RichText>) -> TextEdit<'a> {
        self.hint = hint.into().text;
        self
    }

    pub fn desired_width(mut self, w: f32) -> TextEdit<'a> {
        self.edit.width = w;
        self
    }

    pub fn desired_rows(mut self, rows: usize) -> TextEdit<'a> {
        self.edit.rows = rows as u32;
        self
    }

    pub fn char_limit(mut self, n: usize) -> TextEdit<'a> {
        self.edit.char_limit = n as u32;
        self
    }

    pub fn password(self, on: bool) -> TextEdit<'a> {
        self.flag(OAK_EDIT_PASSWORD, on)
    }

    pub fn frame(self, on: bool) -> TextEdit<'a> {
        self.flag(OAK_EDIT_NO_FRAME, !on)
    }

    pub fn interactive(self, on: bool) -> TextEdit<'a> {
        self.flag(OAK_EDIT_DISABLED, !on)
    }

    pub fn lock_focus(self, on: bool) -> TextEdit<'a> {
        self.flag(OAK_EDIT_LOCK_FOCUS, on)
    }

    pub fn font(mut self, font: impl Into<EditFont>) -> TextEdit<'a> {
        let EditFont(font, points, small) = font.into();
        (self.edit.font, self.edit.points) = (font, points);
        self.flag(OAK_EDIT_SMALL, small)
    }

    pub fn text_color(mut self, color: Color32) -> TextEdit<'a> {
        self.edit.text_color = color_in(color);
        self
    }

    pub fn code_editor(self) -> TextEdit<'a> {
        self.font(TextStyle::Monospace).lock_focus(true)
    }

    /// Draws it through `call`, which gets the options, the current text
    /// and where the new text goes.
    pub(crate) fn show_with(mut self, ui: &Ui, call: impl FnOnce(&OakTextEdit, OakStr, OakSink, *mut c_void, *mut OakResponse) -> u32) -> Response {
        self.edit.hint = s(&self.hint);
        if let (0, Some(salt)) = (self.edit.id, self.salt) {
            self.edit.id = ui.id().with(salt).0;
        }
        let mut new: Option<String> = None;
        let mut out = OakResponse::default();
        call(&self.edit, s(self.text), sink_string, &mut new as *mut Option<String> as *mut c_void, &mut out);
        if let Some(new) = new {
            *self.text = new;
        }
        Response::of(out)
    }
}

/// A text field's font: a style, or a font and its size.
pub struct EditFont(u32, f32, bool);

impl From<TextStyle> for EditFont {
    fn from(s: TextStyle) -> EditFont {
        EditFont(if s == TextStyle::Monospace { OAK_FONT_CODE } else { OAK_FONT_TEXT }, 0.0, s == TextStyle::Small)
    }
}

impl From<FontId> for EditFont {
    fn from(f: FontId) -> EditFont {
        EditFont(if f.family == FontFamily::Proportional { OAK_FONT_TEXT } else { OAK_FONT_CODE }, f.size, false)
    }
}

impl Widget for TextEdit<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let raw = ui.raw;
        self.show_with(ui, |edit, text, sink, user, out| unsafe { (api().ui_text_edit)(raw, edit, text, sink, user, out) })
    }
}

/// A picture in the layout.
pub struct Image {
    image: OakImage,
}

impl Image {
    pub fn new(texture: impl Into<SizedTexture>) -> Image {
        let t = texture.into();
        Image { image: OakImage { size: std::mem::size_of::<OakImage>() as u32, texture: t.id.0, width: t.size.x, height: t.size.y, ..Default::default() } }
    }

    pub fn from_texture(texture: impl Into<SizedTexture>) -> Image {
        Image::new(texture)
    }

    pub fn fit_to_exact_size(mut self, size: Vec2) -> Image {
        (self.image.width, self.image.height) = (size.x, size.y);
        self
    }

    /// As large as fits in `size`, its shape kept.
    pub fn max_size(mut self, size: Vec2) -> Image {
        let (w, h) = (self.image.width.max(1.0), self.image.height.max(1.0));
        let k = (size.x / w).min(size.y / h).min(1.0);
        (self.image.width, self.image.height) = (w * k, h * k);
        self
    }

    pub fn tint(mut self, tint: impl Into<Color32>) -> Image {
        self.image.tint = color_in(tint.into());
        self
    }

    pub fn corner_radius(mut self, r: impl Into<CornerRadius>) -> Image {
        self.image.radius = r.into().nw as f32;
        self
    }

    pub fn sense(mut self, sense: Sense) -> Image {
        self.image.sense = sense.0;
        self
    }
}

impl Widget for Image {
    fn ui(self, ui: &mut Ui) -> Response {
        let mut out = OakResponse::default();
        unsafe { (api().ui_image)(ui.raw, &self.image, &mut out) };
        Response::of(out)
    }
}

/// A texture and the size to show it at.
pub struct SizedTexture {
    pub id: TextureId,
    pub size: Vec2,
}

impl SizedTexture {
    pub fn new(id: impl Into<TextureId>, size: impl Into<Vec2>) -> SizedTexture {
        SizedTexture { id: id.into(), size: size.into() }
    }
}

impl From<&TextureHandle> for SizedTexture {
    fn from(t: &TextureHandle) -> SizedTexture {
        SizedTexture { id: t.id(), size: t.size_vec2() }
    }
}

impl From<(TextureId, Vec2)> for SizedTexture {
    fn from((id, size): (TextureId, Vec2)) -> SizedTexture {
        SizedTexture { id, size }
    }
}

// -------------------------------------------------------------- containers

pub struct ScrollArea {
    scroll: OakScroll,
}

impl ScrollArea {
    fn new(flags: u32) -> ScrollArea {
        // (egui's default: as small as its content, both ways.)
        ScrollArea { scroll: OakScroll { size: std::mem::size_of::<OakScroll>() as u32, flags: flags | OAK_SCROLL_SHRINK_X | OAK_SCROLL_SHRINK_Y, ..Default::default() } }
    }

    pub fn vertical() -> ScrollArea {
        ScrollArea::new(0)
    }

    pub fn horizontal() -> ScrollArea {
        ScrollArea::new(OAK_SCROLL_HORIZONTAL)
    }

    pub fn both() -> ScrollArea {
        ScrollArea::new(OAK_SCROLL_BOTH)
    }

    pub fn id_salt(mut self, salt: impl Hash) -> ScrollArea {
        self.scroll.id = Id::new(salt).0;
        self
    }

    pub fn max_height(mut self, h: f32) -> ScrollArea {
        self.scroll.max_height = h;
        self
    }

    pub fn max_width(mut self, w: f32) -> ScrollArea {
        self.scroll.max_width = w;
        self
    }

    pub fn auto_shrink(mut self, shrink: impl Into<[bool; 2]>) -> ScrollArea {
        let [x, y] = shrink.into();
        self.scroll.flags &= !(OAK_SCROLL_SHRINK_X | OAK_SCROLL_SHRINK_Y);
        self.scroll.flags |= (if x { OAK_SCROLL_SHRINK_X } else { 0 }) | (if y { OAK_SCROLL_SHRINK_Y } else { 0 });
        self
    }

    pub fn stick_to_bottom(mut self, on: bool) -> ScrollArea {
        if on {
            self.scroll.flags |= OAK_SCROLL_STICK_TO_END;
        }
        self
    }

    pub fn stick_to_right(self, on: bool) -> ScrollArea {
        self.stick_to_bottom(on)
    }

    pub fn show<R>(self, ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> ScrollAreaOutput<R> {
        let raw = ui.raw;
        let (inner, ()) = with_body(f, |body, user| unsafe { (api().ui_scroll)(raw, &self.scroll, body, user) });
        ScrollAreaOutput { inner: inner.expect("Oak didn't draw the scroll area") }
    }
}

pub struct ScrollAreaOutput<R> {
    pub inner: R,
}

/// A box around what is drawn in it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Frame {
    fill: Option<Color32>,
    stroke: Stroke,
    radius: CornerRadius,
    inner: Margin,
    outer: Margin,
}

impl Frame {
    pub fn new() -> Frame {
        Frame::default()
    }

    pub fn none() -> Frame {
        Frame::default()
    }

    pub fn fill(mut self, fill: impl Into<Color32>) -> Frame {
        self.fill = Some(fill.into());
        self
    }

    pub fn stroke(mut self, stroke: impl Into<Stroke>) -> Frame {
        self.stroke = stroke.into();
        self
    }

    pub fn corner_radius(mut self, r: impl Into<CornerRadius>) -> Frame {
        self.radius = r.into();
        self
    }

    pub fn inner_margin(mut self, m: impl Into<Margin>) -> Frame {
        self.inner = m.into();
        self
    }

    pub fn outer_margin(mut self, m: impl Into<Margin>) -> Frame {
        self.outer = m.into();
        self
    }

    pub fn show<R>(self, ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let [nw, ne, sw, se] = self.radius.all();
        let frame = OakFrame {
            size: std::mem::size_of::<OakFrame>() as u32,
            flags: if self.radius.is_same() { 0 } else { OAK_FRAME_CORNERS },
            fill: self.fill.map_or(0, color_in),
            stroke_width: self.stroke.width,
            stroke_color: color_in(self.stroke.color),
            radius: nw,
            pad_x: self.inner.x,
            pad_y: self.inner.y,
            margin_x: self.outer.x,
            margin_y: self.outer.y,
            radius_ne: ne,
            radius_sw: sw,
            radius_se: se,
        };
        let raw = ui.raw;
        contained(f, |body, user, out| unsafe { (api().ui_frame)(raw, &frame, body, user, out) })
    }
}

pub struct Grid {
    grid: OakGrid,
}

impl Grid {
    pub fn new(salt: impl Hash) -> Grid {
        Grid { grid: OakGrid { size: std::mem::size_of::<OakGrid>() as u32, id: Id::new(salt).0, spacing_x: -1.0, spacing_y: -1.0, ..Default::default() } }
    }

    pub fn num_columns(mut self, n: usize) -> Grid {
        self.grid.columns = n as u32;
        self
    }

    pub fn spacing(mut self, spacing: impl Into<Vec2>) -> Grid {
        let v = spacing.into();
        (self.grid.spacing_x, self.grid.spacing_y) = (v.x, v.y);
        self
    }

    pub fn striped(mut self, on: bool) -> Grid {
        if on {
            self.grid.flags |= OAK_GRID_STRIPED;
        }
        self
    }

    pub fn min_col_width(mut self, w: f32) -> Grid {
        self.grid.min_col_width = w;
        self
    }

    pub fn show<R>(self, ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let raw = ui.raw;
        contained(f, |body, user, out| unsafe { (api().ui_grid)(raw, &self.grid, body, user, out) })
    }
}

pub struct ComboBox {
    id: u64,
    selected: RichText,
    width: f32,
}

impl ComboBox {
    pub fn from_id_salt(salt: impl Hash) -> ComboBox {
        ComboBox { id: Id::new(salt).0, selected: RichText::default(), width: 0.0 }
    }

    pub fn selected_text(mut self, text: impl Into<RichText>) -> ComboBox {
        self.selected = text.into();
        self
    }

    pub fn width(mut self, w: f32) -> ComboBox {
        self.width = w;
        self
    }

    pub fn show_ui<R>(self, ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>> {
        let raw = ui.raw;
        let mut out = OakResponse::default();
        let (inner, _) = with_body(f, |body, user| unsafe { (api().ui_combo)(raw, self.id, &self.selected.raw(0), self.width, body, user, &mut out) });
        InnerResponse { inner, response: Response::of(out) }
    }
}

pub struct CollapsingHeader {
    title: RichText,
    id: Option<u64>,
    default_open: bool,
}

pub struct CollapsingResponse<R> {
    pub header_response: Response,
    pub body_returned: Option<R>,
}

impl CollapsingHeader {
    pub fn new(title: impl Into<RichText>) -> CollapsingHeader {
        CollapsingHeader { title: title.into(), id: None, default_open: false }
    }

    pub fn id_salt(mut self, salt: impl Hash) -> CollapsingHeader {
        self.id = Some(Id::new(salt).0);
        self
    }

    pub fn default_open(mut self, open: bool) -> CollapsingHeader {
        self.default_open = open;
        self
    }

    pub fn show<R>(self, ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> CollapsingResponse<R> {
        let raw = ui.raw;
        let id = self.id.unwrap_or_else(|| Id::new(self.title.text()).0);
        let mut out = OakResponse::default();
        let (inner, _) = with_body(f, |body, user| unsafe { (api().ui_collapsing)(raw, id, &self.title.raw(0), self.default_open, body, user, &mut out) });
        CollapsingResponse { header_response: Response::of(out), body_returned: inner }
    }
}

/// A window over the page.
pub struct Window<'open> {
    title: String,
    window: OakWindow,
    open: Option<&'open mut bool>,
}

impl<'open> Window<'open> {
    pub fn new(title: impl Into<String>) -> Window<'open> {
        let title = title.into();
        let id = Id::new(&title).0;
        Window { title, window: OakWindow { size: std::mem::size_of::<OakWindow>() as u32, id, ..Default::default() }, open: None }
    }

    fn flag(mut self, f: u32, on: bool) -> Self {
        self.window.flags = if on { self.window.flags | f } else { self.window.flags & !f };
        self
    }

    pub fn id(mut self, id: Id) -> Self {
        self.window.id = id.0;
        self
    }

    /// With a close button: `open` is set false when it is pressed, and
    /// nothing shows while it is false.
    pub fn open(mut self, open: &'open mut bool) -> Self {
        self.open = Some(open);
        self.flag(OAK_WINDOW_CLOSABLE, true)
    }

    pub fn title_bar(self, on: bool) -> Self {
        self.flag(OAK_WINDOW_NO_TITLE, !on)
    }

    pub fn collapsible(self, on: bool) -> Self {
        self.flag(OAK_WINDOW_NO_COLLAPSE, !on)
    }

    pub fn resizable(self, on: bool) -> Self {
        self.flag(OAK_WINDOW_FIXED, !on)
    }

    pub fn default_pos(mut self, pos: impl Into<Pos2>) -> Self {
        let p = pos.into();
        (self.window.x, self.window.y) = (p.x, p.y);
        self.flag(OAK_WINDOW_AT, true)
    }

    pub fn fixed_pos(self, pos: impl Into<Pos2>) -> Self {
        self.default_pos(pos).flag(OAK_WINDOW_PINNED, true)
    }

    pub fn default_size(mut self, size: impl Into<Vec2>) -> Self {
        let v = size.into();
        (self.window.width, self.window.height) = (v.x, v.y);
        self.flag(OAK_WINDOW_SIZE, true)
    }

    pub fn fixed_size(self, size: impl Into<Vec2>) -> Self {
        self.default_size(size).flag(OAK_WINDOW_FIXED, true)
    }

    pub fn order(mut self, order: Order) -> Self {
        self.window.order = order.code();
        self
    }

    pub fn show<R>(mut self, _ctx: &Context, f: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<Option<R>>> {
        if self.open.as_deref() == Some(&false) {
            return None;
        }
        self.window.title = s(&self.title);
        let mut out = OakResponse::default();
        let (inner, flags) = with_body(f, |body, user| unsafe { (api().window)(&self.window, body, user, &mut out) });
        if let Some(open) = self.open
            && flags & OAK_OPEN == 0
        {
            *open = false;
        }
        Some(InnerResponse { inner, response: Response::of(out) })
    }
}

/// A bare floating area over the page.
pub struct Area {
    window: OakWindow,
}

impl Area {
    pub fn new(id: Id) -> Area {
        Area { window: OakWindow { size: std::mem::size_of::<OakWindow>() as u32, flags: OAK_WINDOW_AREA, id: id.0, ..Default::default() } }
    }

    pub fn fixed_pos(mut self, pos: impl Into<Pos2>) -> Area {
        let p = pos.into();
        (self.window.x, self.window.y) = (p.x, p.y);
        self.window.flags |= OAK_WINDOW_AT;
        self
    }

    pub fn order(mut self, order: Order) -> Area {
        self.window.order = order.code();
        self
    }

    /// Which of its points is at `fixed_pos`.
    pub fn pivot(mut self, pivot: Align2) -> Area {
        self.window.pivot = anchor_code(pivot);
        self
    }

    /// False: it takes no clicks; what is under it does.
    pub fn interactable(mut self, on: bool) -> Area {
        if !on {
            self.window.flags |= OAK_WINDOW_PASSIVE;
        }
        self
    }

    pub fn show<R>(self, _ctx: &Context, f: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let mut out = OakResponse::default();
        let (inner, _) = with_body(f, |body, user| unsafe { (api().window)(&self.window, body, user, &mut out) });
        InnerResponse { inner: inner.expect("Oak didn't draw the area"), response: Response::of(out) }
    }
}

/// How large `text` is, on one line (`wrap_width` 0) or wrapped.
pub fn text_size(ui: &Ui, text: &RichText, wrap_width: f32) -> Vec2 {
    let mut out = OakRect::default();
    unsafe { (api().text_size)(ui.raw, &text.raw(0), wrap_width, &mut out) };
    vec2(out.x1, out.y1)
}

/// `text` cut short with an ellipsis to fit `width`.
pub fn text_clip(ui: &Ui, text: &RichText, width: f32) -> String {
    text_from(|out, user| unsafe { (api().text_clip)(ui.raw, &text.raw(0), width, out, user) }).1
}
