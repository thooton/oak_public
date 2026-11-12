//! Oak's side of the plugin interface: the functions of `oak_sdk::abi`'s
//! table (`HostApi`), each a few lines over what Oak's own pages are drawn
//! with.
//!
//! A plugin's `OakUi` is an `egui::Ui` of Oak's; what it asks to be drawn,
//! Oak's egui draws. The window is lent to the plugin while one of its
//! parts runs (`with_app`), so `app_` functions reach it; a widget's
//! answer is kept until the frame ends (`RESPONSES`) so the plugin can add
//! a tooltip or a menu to it by its number.

// (Every function here is `unsafe extern "C"`: its pointers are the
// plugin's to get right, as the interface's header says. Blocks around
// each use would say nothing more.)
#![allow(unsafe_op_in_unsafe_fn)]

use super::parts::{self, User};
use super::{Response as Answer, registering};
use crate::theme::Theme;
use crate::{App, look, text_menu, widgets};
use eframe::egui::{self, Align, Align2, Color32, FontFamily, FontId, Id, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextStyle, TextWrapMode, Ui, Vec2, pos2, vec2};
use oak_sdk::abi::*;
use oak_sdk::rt::{bytes_of, str_of, text_from};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Oak, as the table's functions.
pub(crate) struct Oak;

/// The table plugins are given.
pub(crate) static TABLE: OakApi = table::<Oak>();

thread_local! {
    /// The window, while a plugin's part runs with it.
    static APP: Cell<*mut App> = const { Cell::new(std::ptr::null_mut()) };
    /// This frame's widget answers: an `OakResponse.id` is a place here, plus one.
    static RESPONSES: RefCell<Vec<egui::Response>> = const { RefCell::new(Vec::new()) };
    /// The theme in use.
    static THEME: RefCell<Theme> = RefCell::new(Theme::oak());
}

/// A frame starts: last frame's answers go, and `theme` is what plugins
/// are drawn with.
pub(crate) fn begin_frame(theme: &Theme) {
    RESPONSES.with(|r| r.borrow_mut().clear());
    THEME.with(|t| *t.borrow_mut() = theme.clone());
}

fn theme() -> Theme {
    THEME.with(|t| t.borrow().clone())
}

/// Runs `f` (a call into a plugin) with the window lent to it.
pub(crate) fn with_app<R>(app: &mut App, f: impl FnOnce() -> R) -> R {
    let before = APP.replace(app as *mut App);
    let r = f();
    APP.set(before);
    r
}

/// The window, for one of a plugin's calls. None outside a part that was
/// given it, and while another call is using it (inside the transcript
/// Oak draws for the plugin, say).
fn app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    let p = APP.replace(std::ptr::null_mut());
    if p.is_null() {
        return None;
    }
    // SAFETY: `with_app` holds the only `&mut App` and doesn't use it
    // until the plugin returns; taking the pointer out keeps a second
    // call from making another.
    let r = f(unsafe { &mut *p });
    APP.set(p);
    Some(r)
}

/// Draws a part: `body` in `ui`, with the window lent.
pub(crate) fn draw(body: Option<OakBody>, user: User, ui: &mut Ui, app: &mut App) {
    if let Some(body) = body {
        // SAFETY: the plugin's own function and pointer; `ui` lives through the call.
        with_app(app, || unsafe { body(user.0, ui_ptr(ui)) });
    }
}

pub(crate) fn ui_ptr(ui: &mut Ui) -> *mut OakUi {
    ui as *mut Ui as *mut OakUi
}

/// # Safety
/// `p` is an `OakUi` Oak handed out, inside the callback it came with.
unsafe fn ui_of<'a>(p: *mut OakUi) -> &'a mut Ui {
    &mut *(p as *mut Ui)
}

/// # Safety
/// `body` and `user` are a plugin's pair.
unsafe fn run(body: OakBody, user: *mut c_void, ui: &mut Ui) {
    body(user, ui_ptr(ui));
}

/// # Safety
/// `s` points to `len` readable bytes, or is empty.
pub(crate) unsafe fn text(s: OakStr) -> String {
    str_of(s).to_string()
}

fn col(c: u32) -> Color32 {
    let [r, g, b, a] = c.to_be_bytes();
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// 0 is "none", "the default".
fn col_opt(c: u32) -> Option<Color32> {
    (c != 0).then(|| col(c))
}

fn col_out(c: Color32) -> u32 {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    u32::from_be_bytes([r, g, b, a])
}

fn rect(r: &OakRect) -> Rect {
    Rect::from_min_max(pos2(r.x0, r.y0), pos2(r.x1, r.y1))
}

pub(crate) fn rect_out(r: Rect) -> OakRect {
    OakRect { x0: r.min.x, y0: r.min.y, x1: r.max.x, y1: r.max.y }
}

fn size_out(v: Vec2) -> OakRect {
    OakRect { x0: 0.0, y0: 0.0, x1: v.x, y1: v.y }
}

/// A plugin's id is an egui id's value.
fn eid(v: u64) -> Id {
    // SAFETY: (not about memory) the value is a hash, as egui's own are.
    if v == 0 { Id::NULL } else { unsafe { Id::from_high_entropy_bits(v) } }
}

fn sense(s: u32) -> Sense {
    match s {
        OAK_SENSE_CLICK => Sense::click(),
        OAK_SENSE_DRAG => Sense::drag(),
        OAK_SENSE_CLICK_AND_DRAG => Sense::click_and_drag(),
        _ => Sense::hover(),
    }
}

fn align(a: u32) -> Align {
    match a {
        OAK_ALIGN_CENTER => Align::Center,
        OAK_ALIGN_MAX => Align::Max,
        _ => Align::Min,
    }
}

fn anchor(a: u32) -> Align2 {
    Align2([align(a % 3), align(a / 3)])
}

fn layout(l: &OakLayout) -> egui::Layout {
    let dir = match l.dir {
        OAK_DIR_LEFT => egui::Direction::RightToLeft,
        OAK_DIR_DOWN => egui::Direction::TopDown,
        OAK_DIR_UP => egui::Direction::BottomUp,
        _ => egui::Direction::LeftToRight,
    };
    egui::Layout::from_main_dir_and_cross_align(dir, align(l.cross_align))
        .with_main_align(align(l.main_align))
        .with_main_wrap(l.flags & OAK_LAYOUT_WRAP != 0)
        .with_cross_justify(l.flags & OAK_LAYOUT_CROSS_JUSTIFY != 0)
        .with_main_justify(l.flags & OAK_LAYOUT_MAIN_JUSTIFY != 0)
}

fn order(o: u32) -> egui::Order {
    match o {
        OAK_ORDER_BACKGROUND => egui::Order::Background,
        OAK_ORDER_FOREGROUND => egui::Order::Foreground,
        OAK_ORDER_TOOLTIP => egui::Order::Tooltip,
        _ => egui::Order::Middle,
    }
}

/// Text and its look, as egui takes it. Text with no look of its own is
/// egui's plain text: a widget colors it as it does its own.
///
/// # Safety
/// `t` is null or a plugin's `OakText`.
unsafe fn rich(t: *const OakText) -> egui::WidgetText {
    if t.is_null() {
        return "".into();
    }
    let t = read(t);
    let looks = [
        (OAK_TEXT_STRONG, RichText::strong as fn(RichText) -> RichText),
        (OAK_TEXT_ITALIC, RichText::italics),
        (OAK_TEXT_SMALL, RichText::small),
        (OAK_TEXT_WEAK, RichText::weak),
        (OAK_TEXT_UNDERLINE, RichText::underline),
        (OAK_TEXT_STRIKE, RichText::strikethrough),
        (OAK_TEXT_HEADING, RichText::heading),
        (OAK_TEXT_MONOSPACE, RichText::monospace),
    ];
    let family = match t.font {
        OAK_FONT_CODE => Some(FontFamily::Name(look::CODE.into())),
        OAK_FONT_ICONS => Some(FontFamily::Name(look::ICONS.into())),
        _ => None,
    };
    if t.color == 0 && family.is_none() && t.points <= 0.0 && looks.iter().all(|(flag, _)| t.flags & flag == 0) {
        return str_of(t.text).into();
    }
    let mut r = RichText::new(str_of(t.text));
    if let Some(c) = col_opt(t.color) {
        r = r.color(c);
    }
    for (flag, with) in looks {
        if t.flags & flag != 0 {
            r = with(r);
        }
    }
    match (family, t.points > 0.0) {
        (Some(f), true) => r.font(FontId::new(t.points, f)),
        (Some(f), false) => r.family(f),
        (None, true) => r.size(t.points),
        (None, false) => r,
    }
    .into()
}

/// `t` laid out: on one line, or wrapped at `wrap_width`.
unsafe fn galley(ui: &Ui, t: *const OakText, wrap_width: f32) -> Arc<egui::Galley> {
    let (mode, width) = if wrap_width > 0.0 { (TextWrapMode::Wrap, wrap_width) } else { (TextWrapMode::Extend, f32::INFINITY) };
    rich(t).into_galley(ui, Some(mode), width, TextStyle::Body)
}

fn flags_of(r: &egui::Response) -> u32 {
    let mut f = 0;
    let mut set = |on: bool, flag: u32| {
        if on {
            f |= flag;
        }
    };
    set(r.hovered(), OAK_HOVERED);
    set(r.clicked(), OAK_CLICKED);
    set(r.secondary_clicked(), OAK_SECONDARY_CLICKED);
    set(r.double_clicked(), OAK_DOUBLE_CLICKED);
    set(r.middle_clicked(), OAK_MIDDLE_CLICKED);
    set(r.changed(), OAK_CHANGED);
    set(r.dragged(), OAK_DRAGGED);
    set(r.drag_started(), OAK_DRAG_STARTED);
    set(r.drag_stopped(), OAK_DRAG_STOPPED);
    set(r.enabled(), OAK_ENABLED);
    set(r.contains_pointer(), OAK_CONTAINS_POINTER);
    set(r.is_pointer_button_down_on(), OAK_POINTER_DOWN_ON);
    set(r.clicked_elsewhere(), OAK_CLICKED_ELSEWHERE);
    if r.sense.is_focusable() {
        set(r.has_focus(), OAK_HAS_FOCUS);
        set(r.gained_focus(), OAK_GAINED_FOCUS);
        set(r.lost_focus(), OAK_LOST_FOCUS);
    }
    f
}

/// A widget's answer as the plugin gets it, kept for `ui_response`.
fn answer(r: egui::Response) -> OakResponse {
    let mut flags = flags_of(&r);
    let pointer = r.interact_pointer_pos().or_else(|| if r.hovered() { r.hover_pos() } else { None });
    if pointer.is_some() {
        flags |= OAK_HAS_POINTER;
    }
    let (drag, at) = (r.drag_delta(), pointer.unwrap_or_default());
    let (area, widget) = (rect_out(r.rect), r.id.value());
    let id = RESPONSES.with(|all| {
        let mut all = all.borrow_mut();
        all.push(r);
        all.len() as u32
    });
    OakResponse { rect: area, flags, id, drag_x: drag.x, drag_y: drag.y, pointer_x: at.x, pointer_y: at.y, widget }
}

/// Hands the plugin a widget's answer (`out` may be null); its flags.
unsafe fn give(r: egui::Response, out: *mut OakResponse) -> u32 {
    let a = answer(r);
    if !out.is_null() {
        *out = a;
    }
    a.flags
}

/// A text field over `text`, from the plugin's options.
unsafe fn field<'a>(text: &'a mut String, e: &OakTextEdit) -> egui::TextEdit<'a> {
    let on = |flag: u32| e.flags & flag != 0;
    let mut t = if on(OAK_EDIT_MULTILINE) { egui::TextEdit::multiline(text) } else { egui::TextEdit::singleline(text) };
    if e.id != 0 {
        t = t.id(eid(e.id));
    }
    let hint = text_of(e.hint);
    if !hint.is_empty() {
        t = t.hint_text(hint);
    }
    if e.width > 0.0 {
        t = t.desired_width(e.width);
    }
    if e.rows > 0 {
        t = t.desired_rows(e.rows as usize);
    }
    if e.char_limit > 0 {
        t = t.char_limit(e.char_limit as usize);
    }
    if on(OAK_EDIT_SMALL) {
        t = t.font(TextStyle::Small);
    }
    match (e.font, e.points > 0.0) {
        (OAK_FONT_CODE, true) => t = t.font(look::code_font(e.points)),
        (OAK_FONT_CODE, false) => t = t.font(TextStyle::Monospace),
        (_, true) => t = t.font(FontId::proportional(e.points)),
        _ => {}
    }
    if let Some(c) = col_opt(e.text_color) {
        t = t.text_color(c);
    }
    if on(OAK_EDIT_NO_FRAME) {
        t = t.frame(egui::Frame::NONE);
    }
    if on(OAK_EDIT_ENTER_SENDS) {
        t = t.return_key(egui::KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter));
    }
    t.password(on(OAK_EDIT_PASSWORD)).interactive(!on(OAK_EDIT_DISABLED)).lock_focus(on(OAK_EDIT_LOCK_FOCUS))
}

unsafe fn text_of(s: OakStr) -> String {
    text(s)
}

/// The textures plugins made, by their numbers.
static TEXTURES: Mutex<Option<HashMap<u64, egui::TextureHandle>>> = Mutex::new(None);
static NEXT_TEXTURE: AtomicU64 = AtomicU64::new(1);

fn texture(id: u64) -> Option<egui::TextureId> {
    TEXTURES.lock().unwrap().as_ref().and_then(|t| t.get(&id)).map(|t| t.id())
}

fn keep_texture(name: &str, image: egui::ColorImage, flags: u32) -> u64 {
    let Some(ctx) = super::context() else { return 0 };
    let options = if flags & OAK_TEXTURE_LINEAR != 0 { egui::TextureOptions::LINEAR } else { egui::TextureOptions::NEAREST };
    let handle = ctx.load_texture(name, image, options);
    let id = NEXT_TEXTURE.fetch_add(1, Ordering::Relaxed);
    TEXTURES.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, handle);
    id
}

fn key_of(k: u32) -> Option<Key> {
    Some(match k {
        OAK_KEY_ENTER => Key::Enter,
        OAK_KEY_ESCAPE => Key::Escape,
        OAK_KEY_TAB => Key::Tab,
        OAK_KEY_BACKSPACE => Key::Backspace,
        OAK_KEY_DELETE => Key::Delete,
        OAK_KEY_SPACE => Key::Space,
        OAK_KEY_LEFT => Key::ArrowLeft,
        OAK_KEY_RIGHT => Key::ArrowRight,
        OAK_KEY_UP => Key::ArrowUp,
        OAK_KEY_DOWN => Key::ArrowDown,
        OAK_KEY_HOME => Key::Home,
        OAK_KEY_END => Key::End,
        OAK_KEY_PAGE_UP => Key::PageUp,
        OAK_KEY_PAGE_DOWN => Key::PageDown,
        c @ 0..=127 => return Key::from_name(&(c as u8 as char).to_ascii_uppercase().to_string()),
        _ => return None,
    })
}

fn modifiers(flags: u32) -> Modifiers {
    let mut m = Modifiers::NONE;
    for (flag, with) in [(OAK_MOD_SHIFT, Modifiers::SHIFT), (OAK_MOD_CTRL, Modifiers::CTRL), (OAK_MOD_ALT, Modifiers::ALT), (OAK_MOD_COMMAND, Modifiers::COMMAND)] {
        if flags & flag != 0 {
            m |= with;
        }
    }
    m
}

fn modifiers_out(m: Modifiers) -> u32 {
    (if m.shift { OAK_MOD_SHIFT } else { 0 }) | (if m.ctrl { OAK_MOD_CTRL } else { 0 }) | (if m.alt { OAK_MOD_ALT } else { 0 }) | (if m.command { OAK_MOD_COMMAND } else { 0 })
}

/// What a plugin's function hands a sink, as text.
unsafe fn text_fn(f: OakTextFn, user: *mut c_void) -> String {
    text_from(|out, out_user| f(user, out, out_user)).1
}

unsafe fn give_text(out: OakSink, user: *mut c_void, text: &str) {
    out(user, text.as_ptr(), text.len());
}

/// An answer that worked (true, with its text) or didn't (false, with why).
unsafe fn give_result(r: Result<String, String>, out: OakSink, user: *mut c_void) -> bool {
    match r {
        Ok(text) => {
            give_text(out, user, &text);
            true
        }
        Err(why) => {
            give_text(out, user, &why);
            false
        }
    }
}

fn headers_from(lines: &str) -> Vec<(String, String)> {
    lines.lines().filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_string(), v.trim().to_string())).collect()
}

/// A plugin's transcript, from the JSON it is handed over as.
fn chat_from(text: &str) -> oak_sdk::chat::Chat {
    let v: Value = serde_json::from_str(text).unwrap_or_default();
    let marks = v["marks"].as_array().map(|list| list.iter().map(|m| oak_sdk::chat::Mark { at: m["at"].as_u64().unwrap_or(0) as usize, failed: m["failed"].as_bool().unwrap_or(false), text: m["text"].as_str().unwrap_or("").to_string(), time: m["time"].as_i64().unwrap_or(0) }).collect());
    oak_sdk::chat::Chat { messages: v["messages"].as_array().cloned().unwrap_or_default(), marks: marks.unwrap_or_default(), busy: v["busy"].as_bool().unwrap_or(false), status: v["status"].as_str().unwrap_or("").to_string() }
}

impl HostApi for Oak {
    // ---- parts

    unsafe extern "C" fn add_home_tab(part: *const OakHomeTab) {
        registering(|p| p.home_tabs.push(Arc::new(parts::HomeTab::new(&read(part)))));
    }

    unsafe extern "C" fn add_tab_kind(part: *const OakTabKind) {
        registering(|p| p.tab_kinds.push(Arc::new(parts::TabKind::new(&read(part)))));
    }

    unsafe extern "C" fn add_new_kind(part: *const OakNewKind) {
        registering(|p| p.new_kinds.push(Arc::new(parts::NewKind::new(&read(part)))));
    }

    unsafe extern "C" fn add_cloud_backend(part: *const OakCloudBackend) {
        registering(|p| p.cloud_backends.push(Arc::new(parts::CloudBackend::new(&read(part)))));
    }

    unsafe extern "C" fn add_session_kit(part: *const OakSessionKit) {
        registering(|p| p.session_kits.push((text(read(part).backend), Arc::new(parts::SessionKit::new(&read(part))))));
    }

    unsafe extern "C" fn add_theme(part: *const OakThemeDef) {
        registering(|p| p.themes.push(Arc::new(parts::ThemeDef::new(&read(part)))));
    }

    unsafe extern "C" fn add_font(part: *const OakFontDef) {
        registering(|p| p.fonts.push(Arc::new(parts::FontDef::new(&read(part)))));
    }

    unsafe extern "C" fn add_role(part: *const OakRole) {
        let r = read(part);
        registering(|p| p.roles.push(parts::Role { id: text(r.id), name: text(r.name), text: text(r.text), offered: r.flags & OAK_ROLE_OFFERED != 0, icon: text(r.icon) }));
    }

    unsafe extern "C" fn add_vault(part: *const OakVault) {
        registering(|p| p.vault = Some(Arc::new(parts::Vault::new(&read(part)))));
    }

    unsafe extern "C" fn add_settings(part: *const OakSettings) {
        registering(|p| p.settings.push(Arc::new(parts::SettingsSection::new(&read(part)))));
    }

    unsafe extern "C" fn add_gate(part: *const OakGate) {
        registering(|p| p.gates.push(Arc::new(parts::Gate::new(&read(part)))));
    }

    unsafe extern "C" fn add_hooks(part: *const OakHooks) {
        registering(|p| p.hooks.push(Arc::new(parts::Hooks::new(&read(part)))));
    }

    // ---- services

    unsafe extern "C" fn version(out: OakSink, out_user: *mut c_void) {
        give_text(out, out_user, crate::update::VERSION);
    }

    unsafe extern "C" fn dir(out: OakSink, out_user: *mut c_void) {
        give_text(out, out_user, &crate::store::dir().to_string_lossy());
    }

    unsafe extern "C" fn log_crash(text: OakStr) {
        crate::log_crash(str_of(text));
    }

    unsafe extern "C" fn plugin_failed(id: OakStr, why: OakStr) {
        super::failed(str_of(id), str_of(why));
    }

    unsafe extern "C" fn repaint() {
        if let Some(ctx) = super::context() {
            ctx.request_repaint();
        }
    }

    unsafe extern "C" fn repaint_after(ms: u64) {
        if let Some(ctx) = super::context() {
            ctx.request_repaint_after(Duration::from_millis(ms));
        }
    }

    unsafe extern "C" fn now_ms() -> i64 {
        oak_sdk::now_ms()
    }

    unsafe extern "C" fn http_send(request: *const OakRequest, error: OakSink, error_user: *mut c_void) -> u64 {
        let r = read(request);
        let sent = super::http(str_of(r.method), str_of(r.url), &headers_from(str_of(r.headers)), bytes_of(r.body), Duration::from_millis(if r.timeout_ms == 0 { 30_000 } else { r.timeout_ms }), r.flags & OAK_HTTP_NO_REDIRECTS == 0, r.flags & OAK_HTTP_DIRECT != 0);
        match sent {
            Ok(answer) => super::keep_answer(answer),
            Err(why) => {
                give_text(error, error_user, &why);
                0
            }
        }
    }

    unsafe extern "C" fn http_status(answer: u64) -> u32 {
        super::with_answer(answer, |a| a.status as u32).unwrap_or(0)
    }

    unsafe extern "C" fn http_headers(answer: u64, out: OakSink, out_user: *mut c_void) {
        let lines = super::with_answer(answer, |a| a.headers.iter().map(|(n, v)| format!("{}: {v}\n", n.to_ascii_lowercase())).collect::<String>()).unwrap_or_default();
        give_text(out, out_user, &lines);
    }

    unsafe extern "C" fn http_read(answer: u64, buffer: *mut u8, capacity: usize) -> i64 {
        if buffer.is_null() || capacity == 0 {
            return 0;
        }
        let buf = std::slice::from_raw_parts_mut(buffer, capacity);
        match super::with_answer(answer, |a| std::io::Read::read(&mut a.body, buf)) {
            Some(Ok(n)) => n as i64,
            _ => -1,
        }
    }

    unsafe extern "C" fn http_close(answer: u64) {
        drop(super::take_answer(answer));
    }

    unsafe extern "C" fn http_make(status: u32, headers: OakStr, body: OakBytes) -> u64 {
        super::keep_answer(Answer { status: status as u16, headers: headers_from(str_of(headers)), body: Box::new(std::io::Cursor::new(bytes_of(body).to_vec())) })
    }

    unsafe extern "C" fn write_private(path: OakStr, data: OakBytes, error: OakSink, error_user: *mut c_void) -> bool {
        give_result(crate::store::write_private(std::path::Path::new(str_of(path)), bytes_of(data)).map(|()| String::new()).map_err(|e| e.to_string()), error, error_user)
    }

    unsafe extern "C" fn uuid(out: OakSink, out_user: *mut c_void) {
        give_text(out, out_user, &crate::api::new_uuid());
    }

    unsafe extern "C" fn shared(name: OakStr, out: OakSink, out_user: *mut c_void) -> bool {
        match super::shared(str_of(name)) {
            Some(v) => {
                give_text(out, out_user, &v.to_string());
                true
            }
            None => false,
        }
    }

    unsafe extern "C" fn share(name: OakStr, json: OakStr) {
        super::share(str_of(name), serde_json::from_str(str_of(json)).ok());
    }

    unsafe extern "C" fn signed_in() -> bool {
        crate::api::signed_in()
    }

    unsafe extern "C" fn vault_changed() {
        std::thread::spawn(|| {
            crate::api::vault_changed();
            if let Some(ctx) = super::context() {
                ctx.request_repaint();
            }
        });
    }

    unsafe extern "C" fn usage_local(since: i64, out: OakSink, out_user: *mut c_void) -> bool {
        let lines = crate::usage::local_records(|records| {
            let mut text = String::new();
            for r in records.filter(|r| r.t >= since) {
                text.push_str(&serde_json::to_string(r).unwrap_or_default());
                text.push('\n');
            }
            text
        });
        give_result(lines, out, out_user)
    }

    unsafe extern "C" fn usage_set_shared(source: OakStr, records: OakStr) {
        crate::usage::set_remote(text(source), serde_json::from_str(str_of(records)).unwrap_or_default());
    }

    unsafe extern "C" fn usage_add_shared(source: OakStr, records: OakStr) -> bool {
        let records: Vec<crate::usage::Rec> = serde_json::from_str(str_of(records)).unwrap_or_default();
        crate::usage::add_remote(str_of(source), &records)
    }

    unsafe extern "C" fn usage_show(source: OakStr, label: OakStr, detail: OakStr) {
        let source = str_of(source);
        crate::usage::show_shared((!source.is_empty()).then_some(source), str_of(label), str_of(detail));
    }

    unsafe extern "C" fn usage_discard_before(cutoff: i64, error: OakSink, error_user: *mut c_void) -> bool {
        give_result(crate::usage::discard_before(cutoff).map(|()| String::new()), error, error_user)
    }

    unsafe extern "C" fn cloud_name(key: OakStr, title: OakStr) {
        crate::cloud::name(str_of(key), str_of(title));
    }

    unsafe extern "C" fn cloud_forget(backend: OakStr) {
        crate::cloud::forget(str_of(backend));
    }

    unsafe extern "C" fn cloud_starting() -> bool {
        crate::cloud::starting()
    }

    unsafe extern "C" fn cloud_accounts(out: OakSink, out_user: *mut c_void) {
        give_text(out, out_user, &serde_json::to_string(&crate::api::cloud_accounts()).unwrap_or_default());
    }

    unsafe extern "C" fn image_block(bytes: OakBytes, out: OakSink, out_user: *mut c_void) -> bool {
        give_result(crate::images::from_bytes(bytes_of(bytes)).map(|i| i.block().to_string()), out, out_user)
    }

    unsafe extern "C" fn file_block(path: OakStr, bytes: OakBytes, out: OakSink, out_user: *mut c_void) -> bool {
        give_result(crate::transcript::file_block(std::path::Path::new(str_of(path)), bytes_of(bytes)).map(|b| b.to_string()), out, out_user)
    }

    // ---- the window

    unsafe extern "C" fn app_theme(out: *mut OakTheme) {
        let t = theme();
        let now = OakTheme {
            size: size_of::<OakTheme>() as u32,
            bg: col_out(t.bg),
            panel: col_out(t.panel),
            panel2: col_out(t.panel2),
            ink: col_out(t.ink),
            ink2: col_out(t.ink2),
            ink3: col_out(t.ink3),
            ink4: col_out(t.ink4),
            accent: col_out(t.accent),
            line: col_out(t.line),
            error: col_out(t.error),
            tool_bg: col_out(t.tool_bg),
            tool_hover: col_out(t.tool_hover),
        };
        // (As much of it as the plugin's build has room for.)
        write(out, &now);
    }

    unsafe extern "C" fn app_open(key: OakStr) {
        app(|a| a.open(text(key)));
    }

    unsafe extern "C" fn app_close_tabs(gone: OakAskKey, user: *mut c_void) {
        app(|a| a.plugin_close_tabs(&|key| gone(user, oak_sdk::rt::s(key))));
    }

    unsafe extern "C" fn app_shown(out: OakSink, out_user: *mut c_void) -> bool {
        match app(|a| a.plugin_shown()).flatten() {
            Some(key) => {
                give_text(out, out_user, &key);
                true
            }
            None => false,
        }
    }

    unsafe extern "C" fn app_go_home(tab: OakStr) {
        app(|a| a.plugin_go_home(str_of(tab)));
    }

    unsafe extern "C" fn app_go_new(kind: OakStr) {
        app(|a| a.plugin_go_new(str_of(kind)));
    }

    unsafe extern "C" fn app_hint(text: OakStr) {
        let text = str_of(text);
        app(|a| a.hint = (!text.is_empty()).then(|| text.to_string()));
    }

    unsafe extern "C" fn app_input(out: OakSink, out_user: *mut c_void) {
        if let Some(input) = app(|a| a.input.clone()) {
            give_text(out, out_user, &input);
        }
    }

    unsafe extern "C" fn app_set_input(text: OakStr) {
        app(|a| a.input = str_of(text).to_string());
    }

    unsafe extern "C" fn app_attachments(out: OakSink, out_user: *mut c_void) {
        let list = app(|a| Value::from(a.attachments.clone())).unwrap_or_else(|| json!([]));
        give_text(out, out_user, &list.to_string());
    }

    unsafe extern "C" fn app_set_attachments(json: OakStr) {
        if let Ok(list) = serde_json::from_str::<Vec<Value>>(str_of(json)) {
            app(|a| a.attachments = list);
        }
    }

    unsafe extern "C" fn app_conversations(out: OakSink, out_user: *mut c_void) {
        let list = app(|a| a.plugin_conversations()).unwrap_or_default();
        give_text(out, out_user, &serde_json::to_string(&list).unwrap_or_default());
    }

    unsafe extern "C" fn app_start_conversation(json: OakStr, out: OakSink, out_user: *mut c_void) -> bool {
        let Ok(new) = serde_json::from_str::<oak_sdk::host::NewConversation>(str_of(json)) else {
            return give_result(Err("That isn't a conversation to start.".into()), out, out_user);
        };
        give_result(app(|a| a.plugin_start_conversation(new)).unwrap_or_else(|| Err("Oak's window isn't there to start it in.".into())), out, out_user)
    }

    unsafe extern "C" fn app_local_form(ui: *mut OakUi) {
        let (ui, t) = (ui_of(ui), theme());
        app(|a| a.local_rows(ui, &t));
    }

    unsafe extern "C" fn app_transcript(ui: *mut OakUi, key: OakStr, version: u64, make: OakTextFn, make_user: *mut c_void, tail_sig: u64, tail: Option<OakBody>, tail_user: *mut c_void) {
        let (ui, key) = (ui_of(ui), str_of(key));
        app(|a| {
            a.plugin_transcript(ui, key, version, &mut || chat_from(&text_fn(make, make_user)), tail_sig, &mut |ui| {
                if let Some(tail) = tail {
                    run(tail, tail_user, ui);
                }
            })
        });
    }

    unsafe extern "C" fn app_cloud_list(ui: *mut OakUi, backend: OakStr) {
        let ui = ui_of(ui);
        app(|a| a.plugin_cloud_list(ui, str_of(backend)));
    }

    // ---- laying out

    unsafe extern "C" fn ui_row(ui: *mut OakUi, kind: u32, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let ui = ui_of(ui);
        let r = match kind {
            OAK_ROW => ui.horizontal(|ui| run(body, user, ui)).response,
            OAK_ROW_WRAPPED => ui.horizontal_wrapped(|ui| run(body, user, ui)).response,
            OAK_ROW_TOP => ui.horizontal_top(|ui| run(body, user, ui)).response,
            OAK_COLUMN => ui.vertical(|ui| run(body, user, ui)).response,
            OAK_COLUMN_CENTERED => ui.vertical_centered(|ui| run(body, user, ui)).response,
            OAK_COLUMN_CENTERED_JUSTIFIED => ui.vertical_centered_justified(|ui| run(body, user, ui)).response,
            OAK_INDENT => ui.indent("oak-indent", |ui| run(body, user, ui)).response,
            OAK_DISABLED => ui.add_enabled_ui(false, |ui| run(body, user, ui)).response,
            _ => ui.scope(|ui| run(body, user, ui)).response,
        };
        give(r, out);
    }

    unsafe extern "C" fn ui_layout(ui: *mut OakUi, l: *const OakLayout, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let r = ui_of(ui).with_layout(layout(&read(l)), |ui| run(body, user, ui)).response;
        give(r, out);
    }

    unsafe extern "C" fn ui_child(ui: *mut OakUi, child: *const OakChild, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let (ui, c) = (ui_of(ui), read(child));
        let own = (c.flags & OAK_CHILD_LAYOUT != 0 && !c.layout.is_null()).then(|| layout(&read(c.layout)));
        let r = if c.flags & OAK_CHILD_SIZE != 0 {
            let size = vec2(c.width, c.height);
            match own {
                Some(l) => ui.allocate_ui_with_layout(size, l, |ui| run(body, user, ui)).response,
                None => ui.allocate_ui(size, |ui| run(body, user, ui)).response,
            }
        } else {
            let mut b = egui::UiBuilder::new();
            if c.flags & OAK_CHILD_RECT != 0 {
                b = b.max_rect(rect(&c.rect));
            }
            if let Some(l) = own {
                b = b.layout(l);
            }
            if c.id != 0 {
                b = b.id_salt(c.id);
            }
            ui.scope_builder(b, |ui| run(body, user, ui)).response
        };
        give(r, out);
    }

    unsafe extern "C" fn ui_push_id(ui: *mut OakUi, id: u64, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let r = ui_of(ui).push_id(id, |ui| run(body, user, ui)).response;
        give(r, out);
    }

    unsafe extern "C" fn ui_scroll(ui: *mut OakUi, scroll: *const OakScroll, body: OakBody, user: *mut c_void) {
        let (ui, sc) = (ui_of(ui), read(scroll));
        let on = |flag: u32| sc.flags & flag != 0;
        let mut area = if on(OAK_SCROLL_BOTH) {
            egui::ScrollArea::both()
        } else if on(OAK_SCROLL_HORIZONTAL) {
            egui::ScrollArea::horizontal().stick_to_right(on(OAK_SCROLL_STICK_TO_END))
        } else {
            egui::ScrollArea::vertical().stick_to_bottom(on(OAK_SCROLL_STICK_TO_END))
        };
        if sc.id != 0 {
            area = area.id_salt(sc.id);
        }
        if sc.max_height > 0.0 {
            area = area.max_height(sc.max_height);
        }
        if sc.max_width > 0.0 {
            area = area.max_width(sc.max_width);
        }
        area.auto_shrink([on(OAK_SCROLL_SHRINK_X), on(OAK_SCROLL_SHRINK_Y)]).show(ui, |ui| run(body, user, ui));
    }

    unsafe extern "C" fn ui_frame(ui: *mut OakUi, frame: *const OakFrame, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let (ui, f) = (ui_of(ui), read(frame));
        let corner = |r: f32| r.round().clamp(0.0, 255.0) as u8;
        let radius = if f.flags & OAK_FRAME_CORNERS != 0 { egui::CornerRadius { nw: corner(f.radius), ne: corner(f.radius_ne), sw: corner(f.radius_sw), se: corner(f.radius_se) } } else { egui::CornerRadius::from(f.radius) };
        let mut frame = egui::Frame::new()
            .stroke(Stroke::new(f.stroke_width, col(f.stroke_color)))
            .corner_radius(radius)
            .inner_margin(egui::Margin::symmetric(f.pad_x as i8, f.pad_y as i8))
            .outer_margin(egui::Margin::symmetric(f.margin_x as i8, f.margin_y as i8));
        if let Some(fill) = col_opt(f.fill) {
            frame = frame.fill(fill);
        }
        let r = frame.show(ui, |ui| run(body, user, ui)).response;
        give(r, out);
    }

    unsafe extern "C" fn ui_grid(ui: *mut OakUi, grid: *const OakGrid, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let (ui, g) = (ui_of(ui), read(grid));
        let mut grid = egui::Grid::new(g.id).striped(g.flags & OAK_GRID_STRIPED != 0);
        if g.columns > 0 {
            grid = grid.num_columns(g.columns as usize);
        }
        if g.spacing_x >= 0.0 && g.spacing_y >= 0.0 {
            grid = grid.spacing(vec2(g.spacing_x, g.spacing_y));
        }
        if g.min_col_width > 0.0 {
            grid = grid.min_col_width(g.min_col_width);
        }
        let r = grid.show(ui, |ui| run(body, user, ui)).response;
        give(r, out);
    }

    unsafe extern "C" fn ui_end_row(ui: *mut OakUi) {
        ui_of(ui).end_row();
    }

    unsafe extern "C" fn ui_collapsing(ui: *mut OakUi, id: u64, title: *const OakText, default_open: bool, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32 {
        let shown = egui::CollapsingHeader::new(rich(title)).id_salt(id).default_open(default_open).show(ui_of(ui), |ui| run(body, user, ui));
        let open = if shown.body_returned.is_some() { OAK_OPEN } else { 0 };
        let flags = give(shown.header_response, out) | open;
        if !out.is_null() {
            (*out).flags = flags;
        }
        flags
    }

    unsafe extern "C" fn ui_combo(ui: *mut OakUi, id: u64, selected: *const OakText, width: f32, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32 {
        let mut combo = egui::ComboBox::from_id_salt(id).selected_text(rich(selected));
        if width > 0.0 {
            combo = combo.width(width);
        }
        let shown = combo.show_ui(ui_of(ui), |ui| run(body, user, ui));
        let open = if shown.inner.is_some() { OAK_OPEN } else { 0 };
        let flags = give(shown.response, out) | open;
        if !out.is_null() {
            (*out).flags = flags;
        }
        flags
    }

    unsafe extern "C" fn ui_menu_button(ui: *mut OakUi, text: *const OakText, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32 {
        let shown = ui_of(ui).menu_button(rich(text), |ui| run(body, user, ui));
        let open = if shown.inner.is_some() { OAK_OPEN } else { 0 };
        let flags = give(shown.response, out) | open;
        if !out.is_null() {
            (*out).flags = flags;
        }
        flags
    }

    unsafe extern "C" fn ui_close(ui: *mut OakUi) {
        ui_of(ui).close();
    }

    unsafe extern "C" fn ui_add_space(ui: *mut OakUi, amount: f32) {
        ui_of(ui).add_space(amount);
    }

    unsafe extern "C" fn ui_separator(ui: *mut OakUi, out: *mut OakResponse) {
        let r = ui_of(ui).separator();
        give(r, out);
    }

    unsafe extern "C" fn ui_get(ui: *mut OakUi, what: u32, out: *mut OakRect) {
        let r = if ui.is_null() {
            let Some(ctx) = super::context() else { return };
            match what {
                OAK_GET_SCREEN => rect_out(ctx.content_rect()),
                OAK_GET_PIXELS_PER_POINT => size_out(vec2(ctx.pixels_per_point(), 0.0)),
                _ => return,
            }
        } else {
            let ui = ui_of(ui);
            match what {
                OAK_GET_AVAILABLE => size_out(ui.available_size()),
                OAK_GET_MAX_RECT => rect_out(ui.max_rect()),
                OAK_GET_MIN_RECT => rect_out(ui.min_rect()),
                OAK_GET_CURSOR => rect_out(ui.cursor()),
                OAK_GET_NEXT_POSITION => {
                    let p = ui.next_widget_position();
                    OakRect { x0: p.x, y0: p.y, x1: p.x, y1: p.y }
                }
                OAK_GET_CLIP_RECT => rect_out(ui.clip_rect()),
                OAK_GET_AVAILABLE_RECT => rect_out(ui.available_rect_before_wrap()),
                OAK_GET_ITEM_SPACING => size_out(ui.spacing().item_spacing),
                OAK_GET_INTERACT_SIZE => size_out(ui.spacing().interact_size),
                OAK_GET_BUTTON_PADDING => size_out(ui.spacing().button_padding),
                OAK_GET_SCREEN => rect_out(ui.ctx().content_rect()),
                OAK_GET_ROW_HEIGHT => size_out(vec2(0.0, ui.text_style_height(&TextStyle::Body))),
                OAK_GET_INDENT => size_out(vec2(ui.spacing().indent, 0.0)),
                OAK_GET_PIXELS_PER_POINT => size_out(vec2(ui.ctx().pixels_per_point(), 0.0)),
                _ => return,
            }
        };
        *out = r;
    }

    unsafe extern "C" fn ui_set(ui: *mut OakUi, what: u32, a: f32, b: f32, c: f32, d: f32) {
        let ui = ui_of(ui);
        match what {
            OAK_SET_WIDTH => ui.set_width(a),
            OAK_SET_MAX_WIDTH => ui.set_max_width(a),
            OAK_SET_MIN_WIDTH => ui.set_min_width(a),
            OAK_SET_HEIGHT => ui.set_height(a),
            OAK_SET_MAX_HEIGHT => ui.set_max_height(a),
            OAK_SET_MIN_HEIGHT => ui.set_min_height(a),
            OAK_SET_ITEM_SPACING => ui.spacing_mut().item_spacing = vec2(a, b),
            OAK_SET_BUTTON_PADDING => ui.spacing_mut().button_padding = vec2(a, b),
            OAK_SET_INTERACT_SIZE => ui.spacing_mut().interact_size = vec2(a, b),
            OAK_SET_OPACITY => ui.multiply_opacity(a),
            OAK_SET_ENABLED => {
                if a == 0.0 {
                    ui.disable();
                }
            }
            OAK_SET_WRAP => {
                ui.style_mut().wrap_mode = Some(match a as u32 {
                    0 => TextWrapMode::Extend,
                    1 => TextWrapMode::Wrap,
                    _ => TextWrapMode::Truncate,
                })
            }
            OAK_SET_CLIP_RECT => ui.set_clip_rect(Rect::from_min_max(pos2(a, b), pos2(c, d))),
            OAK_SET_TEXT_COLOR => ui.visuals_mut().override_text_color = col_opt(a.to_bits()),
            OAK_SET_INDENT => ui.spacing_mut().indent = a,
            _ => {}
        }
    }

    unsafe extern "C" fn ui_id(ui: *mut OakUi) -> u64 {
        ui_of(ui).scope_id().value()
    }

    unsafe extern "C" fn ui_allocate(ui: *mut OakUi, width: f32, height: f32, s: u32, out: *mut OakResponse) -> u32 {
        let (_, r) = ui_of(ui).allocate_exact_size(vec2(width, height), sense(s));
        give(r, out)
    }

    unsafe extern "C" fn ui_allocate_rect(ui: *mut OakUi, area: *const OakRect, s: u32, out: *mut OakResponse) -> u32 {
        let r = ui_of(ui).allocate_rect(rect(&*area), sense(s));
        give(r, out)
    }

    unsafe extern "C" fn ui_interact(ui: *mut OakUi, area: *const OakRect, id: u64, s: u32, out: *mut OakResponse) -> u32 {
        let r = ui_of(ui).interact(rect(&*area), eid(id), sense(s));
        give(r, out)
    }

    unsafe extern "C" fn ui_response(response: u32, what: u32, value: u32, text: OakStr, body: Option<OakBody>, user: *mut c_void) -> u32 {
        let found = RESPONSES.with(|all| {
            let all = all.borrow();
            if response == 0 { all.last().cloned() } else { all.get(response as usize - 1).cloned() }
        });
        let Some(r) = found else { return 0 };
        let flags = flags_of(&r);
        match (what, body) {
            (OAK_RESPONSE_HOVER_TEXT, _) => drop(r.on_hover_text(str_of(text))),
            (OAK_RESPONSE_DISABLED_HOVER_TEXT, _) => drop(r.on_disabled_hover_text(str_of(text))),
            (OAK_RESPONSE_HOVER_UI, Some(body)) => drop(r.on_hover_ui(|ui| run(body, user, ui))),
            (OAK_RESPONSE_CONTEXT_MENU, Some(body)) => drop(r.context_menu(|ui| run(body, user, ui))),
            (OAK_RESPONSE_CURSOR, _) => {
                let icon = match value {
                    OAK_CURSOR_POINTING_HAND => egui::CursorIcon::PointingHand,
                    OAK_CURSOR_TEXT => egui::CursorIcon::Text,
                    OAK_CURSOR_GRAB => egui::CursorIcon::Grab,
                    OAK_CURSOR_GRABBING => egui::CursorIcon::Grabbing,
                    OAK_CURSOR_RESIZE_HORIZONTAL => egui::CursorIcon::ResizeHorizontal,
                    OAK_CURSOR_RESIZE_VERTICAL => egui::CursorIcon::ResizeVertical,
                    OAK_CURSOR_NOT_ALLOWED => egui::CursorIcon::NotAllowed,
                    OAK_CURSOR_WAIT => egui::CursorIcon::Wait,
                    _ => egui::CursorIcon::Default,
                };
                drop(r.on_hover_cursor(icon));
            }
            (OAK_RESPONSE_REQUEST_FOCUS, _) => r.request_focus(),
            (OAK_RESPONSE_SURRENDER_FOCUS, _) => r.surrender_focus(),
            (OAK_RESPONSE_SCROLL_TO, _) => r.scroll_to_me(match value {
                0 => Some(Align::Min),
                1 => Some(Align::Center),
                2 => Some(Align::Max),
                _ => None,
            }),
            (OAK_RESPONSE_HIGHLIGHT, _) => drop(r.highlight()),
            _ => {}
        }
        flags
    }

    // ---- widgets

    unsafe extern "C" fn ui_label(ui: *mut OakUi, text: *const OakText, out: *mut OakResponse) -> u32 {
        let flags = read(text).flags;
        let mut label = egui::Label::new(rich(text));
        if flags & OAK_TEXT_WRAP != 0 {
            label = label.wrap();
        }
        if flags & OAK_TEXT_TRUNCATE != 0 {
            label = label.truncate();
        }
        if flags & OAK_TEXT_EXTEND != 0 {
            label = label.extend();
        }
        if flags & OAK_TEXT_NO_SELECT != 0 {
            label = label.selectable(false);
        }
        if flags & OAK_TEXT_LEFT != 0 {
            label = label.halign(Align::LEFT);
        }
        if flags & OAK_TEXT_CLICK != 0 {
            label = label.sense(Sense::click());
        }
        give(ui_of(ui).add_enabled(flags & OAK_TEXT_DISABLED == 0, label), out)
    }

    unsafe extern "C" fn ui_button(ui: *mut OakUi, button: *const OakButton, out: *mut OakResponse) -> u32 {
        let (ui, b) = (ui_of(ui), read(button));
        let on = |flag: u32| b.flags & flag != 0;
        let text = rich(b.text);
        if on(OAK_BUTTON_LINK) {
            return give(ui.link(text), out);
        }
        let mut w = match texture(b.image) {
            Some(t) => egui::Button::image_and_text(egui::Image::from_texture(egui::load::SizedTexture::new(t, vec2(b.image_width, b.image_height))), text),
            None if on(OAK_BUTTON_SELECTABLE) => egui::Button::selectable(on(OAK_BUTTON_SELECTED), text),
            None => egui::Button::new(text),
        };
        if on(OAK_BUTTON_SMALL) {
            w = w.small();
        }
        if on(OAK_BUTTON_WRAP) {
            w = w.wrap();
        }
        if let Some(fill) = col_opt(b.fill) {
            w = w.fill(fill);
        }
        if b.stroke_width > 0.0 {
            w = w.stroke(Stroke::new(b.stroke_width, col(b.stroke_color)));
        }
        if b.min_width > 0.0 || b.min_height > 0.0 {
            w = w.min_size(vec2(b.min_width, b.min_height));
        }
        if on(OAK_BUTTON_NO_FRAME) {
            w = w.frame(false);
        }
        let w = w.selected(on(OAK_BUTTON_SELECTED));
        give(ui.add_enabled(!on(OAK_BUTTON_DISABLED), w), out)
    }

    unsafe extern "C" fn ui_hyperlink(ui: *mut OakUi, text: *const OakText, url: OakStr, out: *mut OakResponse) -> u32 {
        let enabled = read(text).flags & OAK_TEXT_DISABLED == 0;
        give(ui_of(ui).add_enabled(enabled, egui::Hyperlink::from_label_and_url(rich(text), str_of(url))), out)
    }

    unsafe extern "C" fn ui_checkbox(ui: *mut OakUi, value: *mut bool, text: *const OakText, out: *mut OakResponse) -> u32 {
        let enabled = read(text).flags & OAK_TEXT_DISABLED == 0;
        give(ui_of(ui).add_enabled(enabled, egui::Checkbox::new(&mut *value, rich(text))), out)
    }

    unsafe extern "C" fn ui_spinner(ui: *mut OakUi, size: f32, color: u32, out: *mut OakResponse) -> u32 {
        let mut spinner = egui::Spinner::new();
        if size > 0.0 {
            spinner = spinner.size(size);
        }
        if let Some(c) = col_opt(color) {
            spinner = spinner.color(c);
        }
        give(ui_of(ui).add(spinner), out)
    }

    unsafe extern "C" fn ui_drag(ui: *mut OakUi, value: *mut f64, drag: *const OakDrag, out: *mut OakResponse) -> u32 {
        let d = read(drag);
        let mut w = egui::DragValue::new(&mut *value);
        if d.prefix.len > 0 {
            w = w.prefix(text(d.prefix));
        }
        if d.suffix.len > 0 {
            w = w.suffix(text(d.suffix));
        }
        if d.flags & OAK_DRAG_RANGE != 0 {
            w = w.range(d.min..=d.max);
        }
        if d.speed > 0.0 {
            w = w.speed(d.speed);
        }
        if d.flags & OAK_DRAG_DECIMALS != 0 {
            w = w.fixed_decimals(d.decimals as usize);
        }
        if d.flags & OAK_DRAG_MAX_DECIMALS != 0 {
            w = w.max_decimals(d.decimals as usize);
        }
        if let Some(format) = d.format {
            let user = d.user;
            w = w.custom_formatter(move |n, _| text_from(|out, out_user| format(user, n, out, out_user)).1);
        }
        give(ui_of(ui).add(w), out)
    }

    unsafe extern "C" fn ui_text_edit(ui: *mut OakUi, edit: *const OakTextEdit, text: OakStr, changed: OakSink, user: *mut c_void, out: *mut OakResponse) -> u32 {
        let (ui, e, before) = (ui_of(ui), &read(edit), str_of(text));
        let mut now = before.to_string();
        let w = field(&mut now, e);
        let r = if e.flags & OAK_EDIT_MENU != 0 { text_menu::show(ui, w, e.flags & OAK_EDIT_SECRET != 0).response.response } else { ui.add(w) };
        if e.flags & OAK_EDIT_FOCUS != 0 {
            r.request_focus();
        }
        if now != before {
            give_text(changed, user, &now);
        }
        give(r, out)
    }

    unsafe extern "C" fn ui_image(ui: *mut OakUi, image: *const OakImage, out: *mut OakResponse) -> u32 {
        let (ui, i) = (ui_of(ui), read(image));
        let Some(t) = texture(i.texture) else { return 0 };
        let mut w = egui::Image::from_texture(egui::load::SizedTexture::new(t, vec2(i.width, i.height))).sense(sense(i.sense)).corner_radius(i.radius);
        if let Some(tint) = col_opt(i.tint) {
            w = w.tint(tint);
        }
        give(ui.add(w), out)
    }

    // ---- painting

    unsafe extern "C" fn paint_rect(ui: *mut OakUi, area: *const OakRect, radius: f32, fill: u32, stroke_width: f32, stroke_color: u32, stroke_kind: u32, clip: *const OakRect) {
        painter(ui, clip).rect(rect(&*area), radius, col(fill), Stroke::new(stroke_width, col(stroke_color)), stroke_kind_of(stroke_kind));
    }

    unsafe extern "C" fn paint_rect_corners(ui: *mut OakUi, area: *const OakRect, nw: f32, ne: f32, sw: f32, se: f32, fill: u32, stroke_width: f32, stroke_color: u32, stroke_kind: u32, clip: *const OakRect) {
        let corner = |r: f32| r.round().clamp(0.0, 255.0) as u8;
        let radius = egui::CornerRadius { nw: corner(nw), ne: corner(ne), sw: corner(sw), se: corner(se) };
        painter(ui, clip).rect(rect(&*area), radius, col(fill), Stroke::new(stroke_width, col(stroke_color)), stroke_kind_of(stroke_kind));
    }

    unsafe extern "C" fn paint_line(ui: *mut OakUi, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: u32, clip: *const OakRect) {
        painter(ui, clip).line_segment([pos2(x0, y0), pos2(x1, y1)], Stroke::new(width, col(color)));
    }

    unsafe extern "C" fn paint_circle(ui: *mut OakUi, x: f32, y: f32, radius: f32, fill: u32, stroke_width: f32, stroke_color: u32, clip: *const OakRect) {
        painter(ui, clip).circle(pos2(x, y), radius, col(fill), Stroke::new(stroke_width, col(stroke_color)));
    }

    unsafe extern "C" fn paint_path(ui: *mut OakUi, points: *const f32, count: usize, width: f32, color: u32, fill: u32, closed: bool, clip: *const OakRect) {
        if points.is_null() || count == 0 {
            return;
        }
        let flat = std::slice::from_raw_parts(points, count * 2);
        let points: Vec<Pos2> = flat.chunks_exact(2).map(|p| pos2(p[0], p[1])).collect();
        let stroke = Stroke::new(width, col(color));
        let shape = match (col_opt(fill), closed) {
            (Some(fill), _) => egui::Shape::convex_polygon(points, fill, stroke),
            (None, true) => egui::Shape::closed_line(points, stroke),
            (None, false) => egui::Shape::line(points, stroke),
        };
        painter(ui, clip).add(shape);
    }

    unsafe extern "C" fn paint_text(ui: *mut OakUi, x: f32, y: f32, anchor_at: u32, text: *const OakText, wrap_width: f32, out: *mut OakRect, clip: *const OakRect) {
        let ink = ui_of(ui).visuals().text_color();
        // Text of one font and size on one line is laid out as egui's
        // painter lays its text out.
        let simple = (!text.is_null()).then(|| read(text)).filter(|t| wrap_width <= 0.0 && t.points > 0.0 && t.flags & !OAK_TEXT_MONOSPACE == 0);
        let g = match simple {
            Some(t) => {
                let family = match t.font {
                    OAK_FONT_CODE => FontFamily::Name(look::CODE.into()),
                    OAK_FONT_ICONS => FontFamily::Name(look::ICONS.into()),
                    _ if t.flags & OAK_TEXT_MONOSPACE != 0 => FontFamily::Monospace,
                    _ => FontFamily::Proportional,
                };
                ui_of(ui).painter().layout_no_wrap(str_of(t.text).to_string(), FontId::new(t.points, family), col_opt(t.color).unwrap_or(ink))
            }
            None => galley(ui_of(ui), text, wrap_width),
        };
        let area = anchor(anchor_at).anchor_size(pos2(x, y), g.size());
        painter(ui, clip).galley(area.min, g, simple.and_then(|t| col_opt(t.color)).unwrap_or(ink));
        if !out.is_null() {
            *out = rect_out(area);
        }
    }

    unsafe extern "C" fn paint_image(ui: *mut OakUi, tex: u64, area: *const OakRect, uv: *const OakRect, tint: u32, radius: f32, clip: *const OakRect) {
        let Some(t) = texture(tex) else { return };
        let uv = if uv.is_null() { Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)) } else { rect(&*uv) };
        painter(ui, clip).add(egui::epaint::RectShape::filled(rect(&*area), radius, col_opt(tint).unwrap_or(Color32::WHITE)).with_texture(t, uv));
    }

    unsafe extern "C" fn text_size(ui: *mut OakUi, text: *const OakText, wrap_width: f32, out: *mut OakRect) {
        *out = size_out(galley(ui_of(ui), text, wrap_width).size());
    }

    unsafe extern "C" fn text_clip(ui: *mut OakUi, text: *const OakText, width: f32, out: OakSink, out_user: *mut c_void) {
        if text.is_null() {
            return;
        }
        let (ui, t) = (ui_of(ui), read(text));
        // The longest start that fits with the ellipsis, by bisection.
        let chars: Vec<char> = str_of(t.text).chars().collect();
        let fits = |n: usize, dots: bool| {
            let cut: String = chars[..n].iter().collect::<String>() + if dots { "…" } else { "" };
            let piece = OakText { text: oak_sdk::rt::s(&cut), ..t };
            galley(ui, &piece, 0.0).size().x <= width
        };
        let clipped = if fits(chars.len(), false) {
            chars.iter().collect::<String>()
        } else {
            let (mut lo, mut hi) = (0, chars.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if fits(mid, true) { lo = mid } else { hi = mid - 1 }
            }
            chars[..lo].iter().collect::<String>() + "…"
        };
        give_text(out, out_user, &clipped);
    }

    // ---- input and the window

    unsafe extern "C" fn input_key(key: u32, flags: u32) -> bool {
        let (Some(ctx), Some(k)) = (super::context(), key_of(key)) else { return false };
        let mods = modifiers(flags);
        ctx.input_mut(|i| {
            if flags & OAK_KEY_ANY_MODS != 0 {
                let pressed = i.key_pressed(k);
                if pressed && flags & OAK_KEY_CONSUME != 0 {
                    i.consume_key(i.modifiers, k);
                }
                pressed
            } else if flags & OAK_KEY_CONSUME != 0 {
                i.consume_key(mods, k)
            } else {
                i.key_pressed(k) && i.modifiers.matches_logically(mods)
            }
        })
    }

    unsafe extern "C" fn input_get(what: u32, out: *mut OakRect) -> bool {
        let Some(ctx) = super::context() else { return false };
        let point = |p: Option<Pos2>| p.map(|p| OakRect { x0: p.x, y0: p.y, x1: p.x, y1: p.y });
        let (r, on) = ctx.input(|i| match what {
            OAK_INPUT_POINTER => (point(i.pointer.interact_pos()), true),
            OAK_INPUT_PRESS_ORIGIN => (point(i.pointer.press_origin()), true),
            OAK_INPUT_POINTER_DOWN => (Some(OakRect::default()), i.pointer.primary_down()),
            OAK_INPUT_MODIFIERS => (Some(OakRect { x0: modifiers_out(i.modifiers) as f32, ..Default::default() }), true),
            OAK_INPUT_SCROLL => (Some(OakRect { x0: i.smooth_scroll_delta().x, y0: i.smooth_scroll_delta().y, ..Default::default() }), true),
            _ => (None, false),
        });
        match r {
            Some(r) => {
                if !out.is_null() {
                    *out = r;
                }
                on
            }
            None => false,
        }
    }

    unsafe extern "C" fn input_time() -> f64 {
        super::context().map_or(0.0, |ctx| ctx.input(|i| i.time))
    }

    unsafe extern "C" fn focus(id: u64, request: bool) -> bool {
        let Some(ctx) = super::context() else { return false };
        if request {
            ctx.memory_mut(|m| m.request_focus(eid(id)));
        }
        ctx.memory(|m| m.has_focus(eid(id)))
    }

    unsafe extern "C" fn animate(id: u64, on: bool) -> f32 {
        super::context().map_or(on as u8 as f32, |ctx| ctx.animate_bool(eid(id), on))
    }

    unsafe extern "C" fn copy_text(text: OakStr) {
        if let Some(ctx) = super::context() {
            ctx.copy_text(str_of(text).to_string());
        }
    }

    unsafe extern "C" fn open_url(url: OakStr) {
        let _ = open::that(str_of(url));
    }

    unsafe extern "C" fn window(window: *const OakWindow, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32 {
        let Some(ctx) = super::context() else { return 0 };
        let w = read(window);
        let on = |flag: u32| w.flags & flag != 0;
        let title = text(w.title);
        let id = if w.id == 0 { Id::unique(("plugin-window", &title)) } else { eid(w.id) };
        let (at, size) = (pos2(w.x, w.y), vec2(w.width, w.height));
        if on(OAK_WINDOW_AREA) {
            let mut area = egui::Area::new(id).order(order(w.order)).interactable(!on(OAK_WINDOW_PASSIVE));
            if on(OAK_WINDOW_AT) {
                area = area.fixed_pos(at).pivot(anchor(w.pivot));
            }
            let r = area.show(&ctx, |ui| run(body, user, ui)).response;
            let flags = give(r, out) | OAK_OPEN;
            if !out.is_null() {
                (*out).flags = flags;
            }
            return flags;
        }
        let mut open = true;
        let mut win = egui::Window::new(title).id(id).order(order(w.order)).title_bar(!on(OAK_WINDOW_NO_TITLE)).collapsible(!on(OAK_WINDOW_NO_COLLAPSE));
        if on(OAK_WINDOW_CLOSABLE) {
            win = win.open(&mut open);
        }
        if on(OAK_WINDOW_AT) {
            win = if on(OAK_WINDOW_PINNED) { win.fixed_pos(at) } else { win.default_pos(at) };
        }
        win = match (on(OAK_WINDOW_SIZE), on(OAK_WINDOW_FIXED)) {
            (true, true) => win.fixed_size(size),
            (true, false) => win.default_size(size),
            (false, true) => win.resizable(false),
            (false, false) => win,
        };
        let shown = win.show(&ctx, |ui| run(body, user, ui));
        let mut flags = shown.map_or(0, |inner| give(inner.response, out));
        if open {
            flags |= OAK_OPEN;
        }
        if !out.is_null() {
            (*out).flags = flags;
        }
        flags
    }

    unsafe extern "C" fn texture_new(name: OakStr, width: u32, height: u32, rgba: *const u8, flags: u32) -> u64 {
        if rgba.is_null() || width == 0 || height == 0 {
            return 0;
        }
        let pixels = std::slice::from_raw_parts(rgba, width as usize * height as usize * 4);
        keep_texture(str_of(name), egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], pixels), flags)
    }

    unsafe extern "C" fn texture_decode(name: OakStr, bytes: OakBytes, flags: u32, size: *mut OakRect) -> u64 {
        let Ok(image) = image::load_from_memory(bytes_of(bytes)) else { return 0 };
        let image = image.to_rgba8();
        let (w, h) = (image.width() as usize, image.height() as usize);
        if !size.is_null() {
            *size = size_out(vec2(w as f32, h as f32));
        }
        keep_texture(str_of(name), egui::ColorImage::from_rgba_unmultiplied([w, h], image.as_raw()), flags)
    }

    unsafe extern "C" fn texture_free(tex: u64) {
        if let Some(all) = TEXTURES.lock().unwrap().as_mut() {
            all.remove(&tex);
        }
    }

    // ---- Oak's own look

    unsafe extern "C" fn oak_markdown(ui: *mut OakUi, text: OakStr, key_a: u64, key_b: u64, flags: u32) {
        let (ui, t, key) = (ui_of(ui), theme(), (key_a as usize, key_b as usize));
        if flags & OAK_MARKDOWN_HIGHLIGHT != 0 {
            crate::transcript::markdown_highlighted(ui, &t, str_of(text), key);
        } else {
            crate::transcript::markdown(ui, &t, str_of(text), key);
        }
    }

    unsafe extern "C" fn oak_message_bar(ui: *mut OakUi, message: *const OakRect, id: u64, copy: OakTextFn, copy_user: *mut c_void, at: i64) {
        crate::transcript::message_bar(ui_of(ui), &theme(), rect(&*message), eid(id), || text_fn(copy, copy_user), (at != 0).then_some(at), None);
    }

    unsafe extern "C" fn oak_image(ui: *mut OakUi, block: OakStr, max_height: f32, out: *mut OakResponse) -> bool {
        let Ok(block) = serde_json::from_str::<Value>(str_of(block)) else { return false };
        match crate::images::show(ui_of(ui), &block, max_height) {
            Some(r) => {
                give(r, out);
                true
            }
            None => false,
        }
    }

    unsafe extern "C" fn oak_copy_menu(ui: *mut OakUi, id: u64, area: *const OakRect, message: Option<OakTextFn>, message_user: *mut c_void, extra: Option<OakBody>, extra_user: *mut c_void) {
        let message = message.map(|f| move || text_fn(f, message_user));
        let extra = extra.map(|f| move |ui: &mut Ui| run(f, extra_user, ui));
        crate::copy_menu::show(ui_of(ui), eid(id), rect(&*area), message.as_ref().map(|f| f as &dyn Fn() -> String), extra.as_ref().map(|f| f as &dyn Fn(&mut Ui)));
    }

    unsafe extern "C" fn oak_chat_scroll(ui: *mut OakUi, id: u64, gap: f32, body: OakBody, user: *mut c_void) {
        crate::composer::chat_scroll(ui_of(ui), &theme(), eid(id), gap, |ui| run(body, user, ui));
    }

    unsafe extern "C" fn oak_list_rows(ui: *mut OakUi, id: OakStr, count: usize, empty: OakStr, row: OakRow, user: *mut c_void) {
        look::list_rows(ui_of(ui), &theme(), str_of(id), count, str_of(empty), |ui, i, area, r| {
            let r = answer(r);
            row(user, ui_ptr(ui), i, &rect_out(area), &r);
        });
    }

    unsafe extern "C" fn oak_corner_width(area: *const OakRect) -> f32 {
        look::corner_width(rect(&*area)).unwrap_or(0.0)
    }

    unsafe extern "C" fn oak_corner_area(name: OakStr, area: *const OakRect, body: OakBody, user: *mut c_void) {
        if let Some(ctx) = super::context() {
            look::corner_area(&ctx, str_of(name), rect(&*area), |ui| run(body, user, ui));
        }
    }

    unsafe extern "C" fn oak_panel_box(ui: *mut OakUi, width: f32, pad_y: f32, gap: f32, body: OakBody, user: *mut c_void) {
        look::panel_box(ui_of(ui), &theme(), width, pad_y as i8, gap, |ui| run(body, user, ui));
    }

    unsafe extern "C" fn oak_kv(ui: *mut OakUi, key: OakStr, value: OakStr, color: u32, out: *mut OakResponse) {
        let t = theme();
        let r = look::kv(ui_of(ui), &t, str_of(key), str_of(value), col_opt(color).unwrap_or(t.ink2));
        give(r, out);
    }

    unsafe extern "C" fn oak_meter(ui: *mut OakUi, width: f32, fraction: f32, color: u32) {
        let t = theme();
        look::meter(ui_of(ui), &t, width, fraction, col_opt(color).unwrap_or(t.accent));
    }

    unsafe extern "C" fn oak_spinner(ui: *mut OakUi, area: *const OakRect, color: u32) {
        look::paint_spinner(ui_of(ui).painter(), rect(&*area), col_opt(color).unwrap_or(theme().ink3));
    }

    unsafe extern "C" fn oak_caption(ui: *mut OakUi, text: OakStr, color: u32, out: *mut OakResponse) -> u32 {
        let r = widgets::caption(ui_of(ui), str_of(text), col_opt(color).unwrap_or(theme().ink3));
        give(r, out)
    }

    unsafe extern "C" fn oak_center_note(ui: *mut OakUi, area: *const OakRect, text: OakStr) {
        widgets::center_note(ui_of(ui), &theme(), rect(&*area), str_of(text));
    }

    unsafe extern "C" fn oak_working(ui: *mut OakUi, what: OakStr) {
        widgets::working(ui_of(ui), &theme(), str_of(what));
    }

    unsafe extern "C" fn oak_loading(ui: *mut OakUi, error: OakStr) {
        let error = str_of(error);
        widgets::loading(ui_of(ui), &theme(), (!error.is_empty()).then_some(error));
    }

    unsafe extern "C" fn oak_error_note(ui: *mut OakUi, error: OakStr) {
        widgets::error_note(ui_of(ui), &theme(), str_of(error));
    }

    unsafe extern "C" fn oak_inset(ui: *mut OakUi, fill: u32, margin: f32, body: OakBody, user: *mut c_void, out: *mut OakResponse) {
        let t = theme();
        let r = egui::Frame::new()
            .fill(col_opt(fill).unwrap_or(Color32::TRANSPARENT))
            .stroke(Stroke::new(1.0, t.line))
            .inner_margin(egui::Margin::same(margin as i8))
            .show(ui_of(ui), |ui| {
                ui.set_width(ui.available_width());
                run(body, user, ui);
            })
            .response;
        give(r, out);
    }

    unsafe extern "C" fn oak_switch(ui: *mut OakUi, on: bool, width: f32, height: f32, out: *mut OakResponse) -> u32 {
        give(widgets::switch(ui_of(ui), &theme(), on, vec2(width, height)), out)
    }

    unsafe extern "C" fn oak_state_dot(ui: *mut OakUi, x: f32, y: f32, paused: bool) {
        widgets::state_dot(ui_of(ui).painter(), &theme(), pos2(x, y), paused);
    }

    unsafe extern "C" fn oak_drop_head(ui: *mut OakUi, id: u64, height: f32, title: OakStr, count: OakStr, right: OakStr, right_color: u32, enabled: bool, open: bool, out: *mut OakResponse) -> u32 {
        let (ui, t) = (ui_of(ui), theme());
        // What is open is the plugin's to keep: the row is drawn as it says.
        widgets::set_open(ui, eid(id), open);
        let (_, r) = widgets::drop_head(ui, &t, eid(id), height, str_of(title), str_of(count), (str_of(right), col_opt(right_color).unwrap_or(t.ink3)), enabled);
        give(r, out)
    }

    unsafe extern "C" fn oak_secret_edit(ui: *mut OakUi, id: OakStr, edit: *const OakTextEdit, text: OakStr, changed: OakSink, user: *mut c_void, out: *mut OakResponse) -> u32 {
        let (ui, e, before) = (ui_of(ui), &read(edit), str_of(text));
        let mut now = before.to_string();
        let width = (e.width > 0.0 && e.width.is_finite()).then_some(e.width);
        let r = widgets::secret_edit(ui, &theme(), str_of(id), field(&mut now, e), e.flags & OAK_EDIT_SMALL != 0, width);
        if now != before {
            give_text(changed, user, &now);
        }
        give(r, out)
    }

    unsafe extern "C" fn oak_dialog(id: OakStr, body: OakBody, user: *mut c_void) -> u32 {
        let Some(ctx) = super::context() else { return 0 };
        let modal = widgets::dialog(str_of(id), &theme()).show(&ctx, |ui| run(body, user, ui));
        if modal.should_close() { OAK_CANCELLED } else { 0 }
    }

    unsafe extern "C" fn oak_confirm_dialog(id: OakStr, title: OakStr, body: Option<OakBody>, user: *mut c_void, error: OakStr, action: OakStr, running: bool) -> u32 {
        let Some(ctx) = super::context() else { return 0 };
        let error = str_of(error);
        let (act, cancel) = widgets::confirm_dialog(
            &ctx,
            &theme(),
            str_of(id),
            str_of(title),
            |ui| {
                if let Some(body) = body {
                    run(body, user, ui);
                }
            },
            (!error.is_empty()).then_some(error),
            str_of(action),
            running,
        );
        (if act { OAK_CONFIRMED } else { 0 }) | (if cancel { OAK_CANCELLED } else { 0 })
    }
}

fn stroke_kind_of(kind: u32) -> StrokeKind {
    match kind {
        OAK_STROKE_INSIDE => StrokeKind::Inside,
        OAK_STROKE_OUTSIDE => StrokeKind::Outside,
        _ => StrokeKind::Middle,
    }
}

/// `ui`'s painter, clipped to `clip` too when one is given.
unsafe fn painter(ui: *mut OakUi, clip: *const OakRect) -> egui::Painter {
    let p = ui_of(ui).painter();
    if clip.is_null() { p.clone() } else { p.with_clip_rect(rect(&*clip)) }
}

#[cfg(test)]
mod tests {
    use super::super::{Parts, REGISTERING, Response as HostResponse, keep_answer, take_answer, with_answer};
    use super::*;
    use crate::app_hover_test::{Headless, click, painted, rect_of, run_frame, test_ctx};
    use eframe::egui::PointerButton::Primary;
    use oak_sdk::egui as sdk;
    use std::sync::Mutex;

    /// Draws a frame of `page` through the SDK's wrappers, which call
    /// Oak's table in this process as a loaded plugin's do; what it painted.
    fn frame(ctx: &egui::Context, t: &Theme, time: &mut f64, events: Vec<egui::Event>, mut page: impl FnMut(&mut sdk::Ui)) -> Vec<(String, Rect)> {
        oak_sdk::rt::attach(&TABLE);
        let out = run_frame(ctx, vec2(800.0, 600.0), time, 0.05, events, |ui| {
            crate::plugins::begin_frame(ui.ctx(), t);
            page(&mut unsafe { sdk::Ui::from_raw(ui_ptr(ui)) });
        });
        painted(&out)
    }

    fn texts(shown: &[(String, Rect)]) -> String {
        shown.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().join("|")
    }

    /// A plugin's ids are egui's own, and its colors Oak's: what one side
    /// makes, the other reads back.
    #[test]
    fn ids_and_colors_are_the_same_on_both_sides() {
        let id = sdk::Id::new("a field").with(7);
        assert_eq!(eid(id.value()).value(), id.value());
        assert_eq!(eid(0), Id::NULL);
        for c in [Color32::from_rgb(1, 2, 3), Color32::from_rgba_unmultiplied(200, 100, 50, 128), Color32::WHITE] {
            assert_eq!(col(sdk::color_in(c)), c);
            assert_eq!(sdk::color_out(col_out(c)), c);
        }
        assert_eq!(col_opt(0), None, "0 is the default, not a color");
    }

    #[derive(Default)]
    struct Page {
        clicks: u32,
        name: String,
        open: bool,
        field: Option<sdk::Id>,
        clipped: String,
        width: f32,
    }

    fn page(ui: &mut sdk::Ui, t: &Theme, s: &mut Page) {
        ui.label(sdk::RichText::new("A heading").size(18.0).color(t.accent));
        ui.horizontal(|ui| {
            if ui.button("Press").on_hover_text("It counts").clicked() {
                s.clicks += 1;
            }
            ui.label(format!("pressed {}", s.clicks));
        });
        let edit = ui.add(sdk::TextEdit::singleline(&mut s.name).id(sdk::Id::new("name")).hint_text("Name"));
        s.field = Some(edit.id);
        sdk::ScrollArea::vertical().id_salt("list").max_height(120.0).show(ui, |ui| {
            sdk::Grid::new("grid").num_columns(2).show(ui, |ui| {
                ui.label("left");
                ui.label("right");
                ui.end_row();
            });
        });
        sdk::Frame::new().fill(t.panel).inner_margin(8.0).show(ui, |ui| ui.small("framed"));
        if ui.checkbox(&mut s.open, "More").changed() {
            ui.ctx().request_repaint();
        }
        if s.open {
            ui.indent("more", |ui| ui.monospace("the rest"));
        }
        let (area, _) = ui.allocate_exact_size(sdk::vec2(120.0, 20.0), sdk::Sense::hover());
        ui.painter().rect_filled(area, 2.0, t.panel2);
        ui.painter().text(area.center(), sdk::Align2::CENTER_CENTER, "painted", sdk::FontId::proportional(12.0), t.ink);
        let long = sdk::RichText::new("a line that is far too long for sixty points");
        s.clipped = sdk::text_clip(ui, &long, 60.0);
        s.width = sdk::text_size(ui, &sdk::RichText::new(s.clipped.clone()), 0.0).x;
    }

    /// The wrappers a Rust plugin is written with draw through the table:
    /// text, rows, buttons that take clicks, a field that takes typing,
    /// containers, painting and measuring.
    #[test]
    fn a_page_drawn_through_the_table() {
        let (ctx, t) = test_ctx(true, true);
        let (mut time, mut s) = (0.0, Page { name: "oak".into(), ..Default::default() });
        let mut draw = |s: &mut Page, events: Vec<egui::Event>| {
            frame(&ctx, &t, &mut time, events, |ui| page(ui, &t, s));
            frame(&ctx, &t, &mut time, vec![], |ui| page(ui, &t, s))
        };
        let shown = draw(&mut s, vec![]);
        for want in ["A heading", "Press", "pressed 0", "oak", "left", "right", "framed", "More", "painted"] {
            assert!(shown.iter().any(|(text, _)| text == want), "{want} isn't drawn: {}", texts(&shown));
        }
        assert!(!texts(&shown).contains("the rest"));
        assert!(s.clipped.ends_with('…') && s.clipped.len() < 30 && s.width <= 60.0, "{} ({} wide)", s.clipped, s.width);
        // A row lays its widgets out side by side.
        let (button, count) = (rect_of(&shown, "Press"), rect_of(&shown, "pressed 0"));
        assert!(count.left() > button.right() && (count.center().y - button.center().y).abs() < 4.0);

        let shown = draw(&mut s, click(button.center(), Primary));
        assert_eq!(s.clicks, 1);
        assert!(texts(&shown).contains("pressed 1"));
        let shown = draw(&mut s, click(rect_of(&shown, "More").center(), Primary));
        assert!(s.open && texts(&shown).contains("the rest"));

        // The field's id is the one the plugin gave it, and focus goes by it.
        assert_eq!(s.field, Some(sdk::Id::new("name")));
        let shown = draw(&mut s, click(rect_of(&shown, "oak").center(), Primary));
        assert!(unsafe { Oak::focus(sdk::Id::new("name").value(), false) });
        draw(&mut s, vec![egui::Event::Text("!".into())]);
        assert!(s.name.contains('!') && s.name.len() == 4, "{}", s.name);
        drop(shown);
    }

    /// An answer goes over the table by its handle, either way, and its
    /// handle is gone once it was taken or dropped.
    #[test]
    fn http_answers_cross_by_handle() {
        oak_sdk::rt::attach(&TABLE);
        let made = oak_sdk::Response::new(201, vec![("x-kind".into(), "made".into())], b"a body".to_vec());
        let handle = made.into_handle();
        let got = take_answer(handle).expect("the answer the plugin made");
        assert_eq!((got.status, got.headers.clone()), (201, vec![("x-kind".to_string(), "made".to_string())]));
        assert_eq!(got.text().unwrap(), "a body");
        assert!(take_answer(handle).is_none());

        let handle = keep_answer(HostResponse { status: 404, headers: vec![("Content-Type".into(), "text/plain".into())], body: Box::new(std::io::Cursor::new(b"not there".to_vec())) });
        let held = oak_sdk::Response::from_handle(handle);
        assert_eq!((held.status, held.ok(), held.header("content-type")), (404, false, Some("text/plain")));
        assert_eq!(held.text().unwrap(), "not there");
        assert!(with_answer(handle, |_| ()).is_none(), "read and dropped: closed");
        // One handed straight back isn't copied: the same handle.
        let handle = keep_answer(HostResponse { status: 200, headers: vec![], body: Box::new(std::io::Cursor::new(vec![1, 2, 3])) });
        assert_eq!(oak_sdk::Response::from_handle(handle).into_handle(), handle);
        assert_eq!(take_answer(handle).unwrap().bytes().unwrap(), vec![1, 2, 3]);
    }

    /// What plugins tell each other.
    #[test]
    fn the_blackboard() {
        oak_sdk::rt::attach(&TABLE);
        let host = oak_sdk::host();
        assert_eq!(host.shared("api-test"), None);
        host.share("api-test", Some(json!({"url": "http://x", "n": 3})));
        assert_eq!(host.shared("api-test"), Some(json!({"url": "http://x", "n": 3})));
        host.share("api-test", None);
        assert_eq!(host.shared("api-test"), None);
        assert_eq!(host.version(), crate::update::VERSION);
        assert_eq!(host.dir(), crate::store::dir());
    }

    /// What the test plugin's parts were asked, in order.
    static ASKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn asked(what: impl Into<String>) {
        ASKED.lock().unwrap().push(what.into());
    }

    struct Everything;
    struct Tab;
    struct Kind;
    struct Start;
    struct Backend;
    struct Kit;
    struct Colors;
    struct Face;
    struct Safe;
    struct Life;

    impl oak_sdk::Plugin for Everything {
        fn info(&self) -> oak_sdk::Info {
            oak_sdk::Info::new("everything", "Everything", "1.0", "A part of each kind.")
        }

        fn register(&self, r: &mut oak_sdk::Registrar) {
            r.home_tab(Tab);
            r.tab_kind(Kind);
            r.new_kind(Start);
            r.cloud_backend(Backend);
            r.session_kit("far", Kit);
            r.theme(Colors);
            r.font(Face);
            r.vault(Safe);
            r.hooks(Life);
            r.role(oak_sdk::Role { id: "terse".into(), name: "Terse".into(), text: "Be brief.".into(), offered: true, icon: "t".into() });
        }
    }

    impl oak_sdk::HomeTab for Tab {
        fn id(&self) -> String {
            "tab".into()
        }

        fn label(&self) -> String {
            "Tab".into()
        }

        fn icon(&self) -> String {
            "T".into()
        }

        fn icon_is_text(&self) -> bool {
            true
        }

        fn order(&self) -> i32 {
            7
        }

        fn shown(&self) -> bool {
            false
        }

        fn show(&self, ui: &mut sdk::Ui, app: &mut dyn oak_sdk::App) {
            let t = app.theme();
            ui.label(sdk::RichText::new(format!("{} conversations", app.conversations().len())).color(t.ink2));
            app.input().push_str(" and more");
            app.attachments().push(json!({"type": "text", "text": "attached"}));
            app.hint(Some("said by the plugin".into()));
            app.go_home("cloud");
            // (The box is asked for again after the window was told to go.)
            assert_eq!(app.input().as_str(), "typed and more");
        }
    }

    impl oak_sdk::TabKind for Kind {
        fn owns(&self, key: &str) -> bool {
            key.starts_with("t:")
        }

        fn exists(&self, key: &str) -> bool {
            key != "t:gone"
        }

        fn look(&self, key: &str) -> oak_sdk::TabLook {
            oak_sdk::TabLook { title: format!("Tab {key}"), busy: true, icon: "x".into(), icon_is_text: true }
        }

        fn show(&self, ui: &mut sdk::Ui, _app: &mut dyn oak_sdk::App, key: &str) {
            ui.label(format!("the tab {key}"));
        }

        fn composer(&self, _key: &str) -> bool {
            false
        }

        fn send(&self, app: &mut dyn oak_sdk::App, key: &str, text: String, files: Vec<Value>) -> Result<(), String> {
            if text.is_empty() {
                return Err("Nothing to send.".into());
            }
            asked(format!("send {key} {text} {}", files.len()));
            app.hint(Some("sent".into()));
            Ok(())
        }
    }

    impl oak_sdk::NewKind for Start {
        fn id(&self) -> String {
            "start".into()
        }

        fn label(&self) -> String {
            "Start".into()
        }

        fn heading(&self) -> String {
            "Start one".into()
        }

        fn form(&self, ui: &mut sdk::Ui, _app: &mut dyn oak_sdk::App) {
            ui.label("its form");
        }

        fn start(&self, _app: &mut dyn oak_sdk::App, text: String, _files: Vec<Value>) -> Result<(), String> {
            Err(format!("can't start {text}"))
        }
    }

    impl oak_sdk::CloudBackend for Backend {
        fn id(&self) -> String {
            "far".into()
        }

        fn name(&self) -> String {
            "Far".into()
        }

        fn icon(&self) -> String {
            "F".into()
        }

        fn about(&self) -> String {
            "Sessions far away.".into()
        }

        fn accounts(&self) -> Vec<oak_sdk::cloud::Account> {
            vec![oak_sdk::cloud::Account { rid: "a1".into(), label: "First".into(), blocked_until: 99 }]
        }

        fn unavailable(&self) -> Option<String> {
            Some("Not connected.".into())
        }

        fn note_limit(&self, rid: &str, until: i64) {
            asked(format!("limit {rid} {until}"));
        }

        fn request(&self, rid: &str, call: oak_sdk::cloud::Call) -> Result<oak_sdk::Response, String> {
            if call.path == "/fail" {
                return Err("no route".into());
            }
            let said = json!({"rid": rid, "method": call.method, "beta": call.beta, "body": call.body, "timeout": call.timeout.as_secs()});
            Ok(oak_sdk::Response::new(202, vec![("x-path".into(), call.path)], said.to_string().into_bytes()))
        }

        fn problem(&self, status: u16) -> Option<String> {
            (status == 402).then(|| "Pay first.".into())
        }
    }

    impl oak_sdk::SessionKit for Kit {
        fn ready(&self) -> bool {
            false
        }

        fn server(&self) -> Result<(String, String), String> {
            Ok(("http://far".into(), "secret".into()))
        }

        fn server_name(&self) -> String {
            "far-server".into()
        }

        fn environment_name(&self) -> String {
            "Far".into()
        }

        fn setup_script(&self, server: &str) -> String {
            format!("curl {server}")
        }

        fn variables(&self, server: &str, token: &str) -> Value {
            json!({"FAR_URL": server, "FAR_TOKEN": token})
        }

        fn setup_block(&self) -> String {
            "<far-setup>".into()
        }

        fn cli(&self) -> String {
            "far".into()
        }

        fn tags(&self) -> oak_sdk::Tags {
            oak_sdk::Tags::of("far")
        }

        fn notes(&self) -> Vec<(String, String, String)> {
            vec![("<a>".into(), "</a>".into(), "A note".into())]
        }

        fn raw(&self, method: &str, path: &str, body: Option<(Vec<u8>, String)>, timeout: Duration) -> Result<oak_sdk::Response, String> {
            let (data, kind) = body.unwrap_or_default();
            Ok(oak_sdk::Response::new(200, vec![("x-said".into(), format!("{method} {path} {kind} {}", timeout.as_millis()))], data))
        }
    }

    impl oak_sdk::ThemeDef for Colors {
        fn id(&self) -> String {
            "warm".into()
        }

        fn name(&self) -> String {
            "Warm".into()
        }

        fn palette(&self) -> oak_sdk::theme::Palette {
            oak_sdk::theme::Palette { bg: 0x141413, panel: 0x1e1d1b, panel2: 0x2a2926, ink: 0xfaf9f5, ink2: 0xb0aea5, ink3: 0x87867f, ink4: 0x5e5d59, accent: 0xd97757, error: 0xe5735f, line_alpha: 0x29 }
        }
    }

    impl oak_sdk::FontDef for Face {
        fn id(&self) -> String {
            "face".into()
        }

        fn name(&self) -> String {
            "Face".into()
        }

        fn faces(&self) -> Option<Vec<Vec<u8>>> {
            Some(vec![vec![1], vec![2, 2], vec![3, 3, 3], vec![4; 4]])
        }

        fn fetch(&self) -> Result<(), String> {
            Err("offline".into())
        }
    }

    impl oak_sdk::Vault for Safe {
        fn name(&self) -> String {
            "the safe".into()
        }

        fn connected(&self) -> bool {
            true
        }

        fn takes_new(&self) -> bool {
            false
        }

        fn failed(&self, error: &str) -> bool {
            error.starts_with("safe:")
        }

        fn unreachable(&self, error: &str) -> bool {
            error.ends_with("unreachable")
        }

        fn list(&self, kind: &str) -> Result<Vec<Value>, String> {
            if kind == "none" { Err("No such kind.".into()) } else { Ok(vec![json!({"id": "1", "kind": kind})]) }
        }

        fn add(&self, kind: &str, login: &Value) -> Result<Value, String> {
            Ok(json!({"kind": kind, "login": login}))
        }

        fn remove(&self, kind: &str, id: &str) -> Result<(), String> {
            asked(format!("remove {kind} {id}"));
            Ok(())
        }

        fn token(&self, kind: &str, id: &str, refused: Option<&str>) -> Result<Value, String> {
            Ok(json!({"kind": kind, "id": id, "refused": refused}))
        }
    }

    impl oak_sdk::Hooks for Life {
        fn frame(&self, _ctx: &sdk::Context, app: &mut dyn oak_sdk::App) {
            asked(format!("frame {}", app.shown().is_some()));
        }

        fn exiting(&self) {
            asked("exiting");
        }
    }

    /// A plugin with a part of each kind, written with the SDK and
    /// registered through the table: Oak's side of each part asks what the
    /// plugin's side answers, texts, JSON, answers by handle and all.
    #[test]
    fn every_kind_of_part_answers_across_the_table() {
        oak_sdk::rt::attach(&TABLE);
        *REGISTERING.lock().unwrap() = Some(Parts::default());
        oak_sdk::rt::register(&Everything);
        let p = REGISTERING.lock().unwrap().take().unwrap();
        assert_eq!((p.home_tabs.len(), p.tab_kinds.len(), p.new_kinds.len(), p.cloud_backends.len(), p.session_kits.len()), (1, 1, 1, 1, 1));
        assert_eq!((p.themes.len(), p.fonts.len(), p.roles.len(), p.hooks.len(), p.vault.is_some()), (1, 1, 1, 1, true));

        let tab = &p.home_tabs[0];
        assert_eq!((tab.id(), tab.label(), tab.icon(), tab.icon_is_text(), tab.tip(), tab.order(), tab.shown()), ("tab".into(), "Tab".into(), "T".into(), true, String::new(), 7, false));

        let kind = &p.tab_kinds[0];
        assert!(kind.owns("t:1") && !kind.owns("u:1"));
        assert!(kind.exists("t:1") && !kind.exists("t:gone") && !kind.composer("t:1"));
        let look = kind.look("t:1");
        assert_eq!((look.title.as_str(), look.busy, look.icon.as_str(), look.icon_is_text), ("Tab t:1", true, "x", true));

        let new = &p.new_kinds[0];
        assert_eq!((new.id(), new.label(), new.heading()), ("start".into(), "Start".into(), "Start one".into()));

        let far = &p.cloud_backends[0];
        assert_eq!((far.id(), far.name(), far.icon(), far.icon_is_text(), far.about()), ("far".into(), "Far".into(), "F".into(), false, "Sessions far away.".into()));
        assert_eq!(far.accounts(), vec![oak_sdk::cloud::Account { rid: "a1".into(), label: "First".into(), blocked_until: 99 }]);
        assert_eq!(far.unavailable().as_deref(), Some("Not connected."));
        far.note_limit("a1", 1234);
        assert_eq!((far.problem(402).as_deref(), far.problem(500)), (Some("Pay first."), None));
        let call = |path: &str| oak_sdk::cloud::Call { method: "POST".into(), path: path.into(), beta: Some("b".into()), org: None, body: Some(json!({"k": 1})), timeout: Duration::from_secs(9) };
        let answer = far.request("a1", call("/v1/sessions")).unwrap();
        assert_eq!((answer.status, answer.headers.clone()), (202, vec![("x-path".to_string(), "/v1/sessions".to_string())]));
        assert_eq!(answer.json().unwrap(), json!({"rid": "a1", "method": "POST", "beta": "b", "body": {"k": 1}, "timeout": 9}));
        assert_eq!(far.request("a1", call("/fail")).err().as_deref(), Some("no route"));

        let (backend, kit) = &p.session_kits[0];
        assert_eq!(backend, "far");
        assert!(!kit.ready());
        assert_eq!(kit.server().unwrap(), ("http://far".to_string(), "secret".to_string()));
        assert_eq!((kit.server_name(), kit.environment_name(), kit.setup_block(), kit.cli()), ("far-server".into(), "Far".into(), "<far-setup>".into(), "far".into()));
        assert_eq!(kit.setup_script("http://far"), "curl http://far");
        assert_eq!(kit.variables("u", "t"), json!({"FAR_URL": "u", "FAR_TOKEN": "t"}));
        let (tags, want) = (kit.tags(), oak_sdk::Tags::of("far"));
        assert_eq!((tags.setup, tags.files, tags.carry), (want.setup, want.files, want.carry));
        assert_eq!(kit.notes(), vec![("<a>".to_string(), "</a>".to_string(), "A note".to_string())]);
        let raw = kit.raw("PUT", "/files/1", Some((vec![9, 8, 7], "application/zip".into())), Duration::from_millis(1500)).unwrap();
        assert_eq!(raw.headers, vec![("x-said".to_string(), "PUT /files/1 application/zip 1500".to_string())]);
        assert_eq!(raw.bytes().unwrap(), vec![9, 8, 7]);

        let theme = &p.themes[0];
        assert_eq!((theme.id(), theme.name()), ("warm".into(), "Warm".into()));
        assert_eq!(theme.theme().accent, crate::app_hover_test::warm_theme().accent);
        assert_eq!(theme.theme().line, crate::app_hover_test::warm_theme().line);

        let font = &p.fonts[0];
        assert_eq!((font.id(), font.name()), ("face".into(), "Face".into()));
        assert_eq!(font.faces(), Some(vec![vec![1], vec![2, 2], vec![3, 3, 3], vec![4; 4]]));
        assert_eq!(font.fetch().err().as_deref(), Some("offline"));

        let vault = p.vault.as_ref().unwrap();
        assert_eq!((vault.name().as_str(), vault.connected(), vault.takes_new()), ("the safe", true, false));
        assert!(vault.failed("safe: broke") && !vault.failed("broke") && vault.unreachable("it is unreachable") && !vault.cannot_keep("x"));
        assert_eq!(vault.list("claude").unwrap(), vec![json!({"id": "1", "kind": "claude"})]);
        assert_eq!(vault.list("none").err().as_deref(), Some("No such kind."));
        assert_eq!(vault.add("codex", &json!({"token": "t"})).unwrap(), json!({"kind": "codex", "login": {"token": "t"}}));
        vault.remove("claude", "1").unwrap();
        assert_eq!(vault.token("claude", "1", Some("old")).unwrap(), json!({"kind": "claude", "id": "1", "refused": "old"}));
        assert_eq!(vault.token("claude", "1", None).unwrap()["refused"], Value::Null);

        let role = &p.roles[0];
        assert_eq!((role.id.as_str(), role.name.as_str(), role.text.as_str(), role.offered, role.icon.as_str()), ("terse", "Terse", "Be brief.", true, "t"));

        // The parts that are given the window: what they do to it lands.
        let (dir, h, mut app) = Headless::start("plugin-parts", &[]);
        app.input = "typed".into();
        let ctx = h.ctx.clone();
        let t = Theme::oak();
        let drawn = |app: &mut App, draw: &dyn Fn(&mut Ui, &mut App)| {
            let input = egui::RawInput { screen_rect: Some(h.screen), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                crate::plugins::begin_frame(ui.ctx(), &t);
                draw(ui, app);
            });
            out.textures_delta.clear();
            painted(&out)
        };
        // (The scratch folder is whichever test set it last: count what is there.)
        let n = app.plugin_conversations().len();
        let shown = drawn(&mut app, &|ui, app| tab.show(ui, app));
        assert!(texts(&shown).contains(&format!("{n} conversations")), "{}", texts(&shown));
        assert_eq!(app.input, "typed and more");
        assert_eq!(app.attachments, vec![json!({"type": "text", "text": "attached"})]);
        assert_eq!(app.hint.as_deref(), Some("said by the plugin"));
        assert!(app.page == crate::Page::Home && app.home_list == "cloud");
        let shown = drawn(&mut app, &|ui, app| {
            kind.show(ui, app, "t:1");
            new.form(ui, app);
        });
        assert!(texts(&shown).contains("the tab t:1") && texts(&shown).contains("its form"), "{}", texts(&shown));
        assert_eq!(kind.send(&mut app, "t:1", String::new(), vec![]).err().as_deref(), Some("Nothing to send."));
        kind.send(&mut app, "t:1", "hello".into(), vec![json!({})]).unwrap();
        assert_eq!(app.hint.as_deref(), Some("sent"));
        assert_eq!(new.start(&mut app, "this".into(), vec![]).err().as_deref(), Some("can't start this"));
        p.hooks[0].frame(&ctx, &mut app);
        p.hooks[0].exiting();
        // Outside a part that was given the window, there is none to change.
        oak_sdk::App::hint(&mut oak_sdk::host::Window::default(), Some("nowhere".into()));
        assert_eq!(app.hint.as_deref(), Some("sent"));

        let asked = ASKED.lock().unwrap().clone();
        assert_eq!(asked, ["limit a1 1234", "remove claude 1", "send t:1 hello 1", "frame false", "exiting"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What a frame painted, shape by shape, as text: what it is, where,
    /// and how it looks (a text's layout job included).
    fn shapes(out: &egui::FullOutput) -> Vec<String> {
        fn at(r: Rect) -> String {
            format!("[{:.1} {:.1} {:.1} {:.1}]", r.min.x, r.min.y, r.max.x, r.max.y)
        }
        fn line(s: &egui::Stroke) -> String {
            if s.width == 0.0 { "no line".into() } else { format!("{} {:?}", s.width, s.color) }
        }
        fn describe(shape: &egui::Shape, clip: Rect, all: &mut Vec<String>) {
            use egui::Shape;
            let text = match shape {
                Shape::Noop => return,
                Shape::Vec(list) => return list.iter().for_each(|s| describe(s, clip, all)),
                Shape::Text(t) => format!("text {:?} {} {:?} {:?} {:?}", t.galley.text(), at(t.visual_bounding_rect()), t.fallback_color, t.override_text_color, t.galley.job),
                Shape::Rect(r) if r.fill == Color32::TRANSPARENT && r.stroke.width == 0.0 => return,
                Shape::Rect(r) => format!("rect {} {:?} fill {:?} {} {}", at(r.rect), r.corner_radius, r.fill, line(&r.stroke), if r.stroke.width == 0.0 { String::new() } else { format!("{:?}", r.stroke_kind) }),
                Shape::Circle(c) => format!("circle {:?} {} fill {:?} {}", c.center, c.radius, c.fill, line(&c.stroke)),
                Shape::LineSegment { points, stroke } => format!("line {points:?} {}", line(stroke)),
                other => format!("{other:?}"),
            };
            all.push(format!("{text} in {}", at(clip)));
        }
        let mut all = vec![];
        for s in &out.shapes {
            describe(&s.shape, s.clip_rect, &mut all);
        }
        all
    }

    #[derive(Clone, Default)]
    struct Fields {
        name: String,
        note: String,
        runs: u32,
        share: f64,
        on: bool,
        pick: usize,
    }

    /// One page, written once and compiled twice: against egui itself,
    /// and against the SDK's `egui` (which is Oak's table underneath).
    macro_rules! page_in {
        ($name:ident, $egui:ident) => {
            fn $name(ui: &mut $egui::Ui, t: &Theme, s: &mut Fields) {
                use $egui::{Align, Align2, Button, FontFamily, FontId, Label, Layout, Margin, RichText, Sense, Stroke, StrokeKind, TextEdit, UiBuilder};
                ui.label("Plain text");
                ui.heading("A heading");
                ui.small("small");
                ui.strong("strong");
                ui.weak("weak");
                ui.monospace("mono");
                ui.label(RichText::new("sized and colored").size(18.0).color(t.accent));
                ui.label(RichText::new("underlined").underline().italics());
                ui.label(RichText::new("\u{f0219}").font(FontId::new(14.0, FontFamily::Name("icons".into()))).color(t.ink3));
                ui.label(RichText::new("code face").font(FontId::new(12.0, FontFamily::Name("code".into()))));
                ui.add(Label::new(RichText::new("A long line that has to wrap because the column it is in is narrow, and then some more of it.").color(t.ink2)).wrap());
                ui.add(Label::new("cut short with an ellipsis because there is no room for all of it in so narrow a column as this one is today").truncate());
                ui.separator();
                ui.horizontal(|ui| {
                    let _ = ui.button("Button");
                    let _ = ui.small_button("small");
                    ui.add(Button::new("wide").min_size(vec2(120.0, 30.0)));
                    ui.add(Button::new(RichText::new("accent").color(t.accent)).fill(t.panel2).stroke(Stroke::new(1.0, t.line)));
                    ui.add(Button::new("bare").frame(false));
                    ui.add_enabled(false, Button::new("off"));
                    let _ = ui.link("a link");
                });
                ui.horizontal_wrapped(|ui| {
                    for word in ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"] {
                        ui.label(RichText::new(word).size(22.0));
                    }
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
                    let _ = ui.selectable_label(s.pick == 0, "first");
                    let _ = ui.selectable_label(s.pick == 1, "second");
                    ui.selectable_value(&mut s.pick, 2, "third");
                    ui.toggle_value(&mut s.on, "toggle");
                });
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 30.0), Layout::right_to_left(Align::Center), |ui| {
                    let _ = ui.button("at the right");
                    ui.label("before it");
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| ui.label("and at the left"));
                });
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.label("a column");
                        ui.label("of two");
                    });
                    ui.allocate_ui_with_layout(vec2(200.0, 0.0), Layout::top_down(Align::Max), |ui| {
                        ui.label("right in 200");
                        ui.add_sized([120.0, 28.0], Button::new("sized"));
                    });
                });
                ui.vertical_centered(|ui| ui.label("centered"));
                ui.indent("indented", |ui| {
                    ui.label("indented");
                    ui.checkbox(&mut s.on, "a checkbox");
                    ui.add_enabled(false, $egui::Checkbox::new(&mut s.on, "one that is off"));
                });
                $egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.line)).corner_radius(6.0).inner_margin(Margin::symmetric(12, 8)).outer_margin(4.0).show(ui, |ui| {
                    ui.set_max_width(260.0);
                    ui.label("in a frame");
                    ui.add(TextEdit::singleline(&mut s.name).hint_text("Name").desired_width(180.0));
                    ui.add(TextEdit::multiline(&mut s.note).hint_text("A note").desired_rows(2).desired_width(f32::INFINITY).font($egui::TextStyle::Small));
                });
                $egui::Frame::new().fill(t.panel2).corner_radius($egui::CornerRadius { nw: 18, ne: 5, sw: 18, se: 18 }).inner_margin(Margin::symmetric(14, 9)).show(ui, |ui| {
                    ui.add(Label::new(RichText::new("a bubble").color(t.ink)).wrap().halign(Align::LEFT));
                });
                $egui::Grid::new("grid").num_columns(3).spacing([14.0, 6.0]).striped(true).show(ui, |ui| {
                    for row in 0..3 {
                        ui.label(format!("row {row}"));
                        ui.add($egui::DragValue::new(&mut s.runs).range(1..=20));
                        ui.add($egui::DragValue::new(&mut s.share).range(0.0..=100.0).speed(0.5).max_decimals(0).suffix("%"));
                        ui.end_row();
                    }
                });
                ui.add($egui::DragValue::new(&mut s.runs).range(1..=20).custom_formatter(|n, _| if n == 1.0 { "1 run".into() } else { format!("{n} runs") }));
                $egui::ScrollArea::vertical().id_salt("list").max_height(60.0).auto_shrink([false, true]).show(ui, |ui| {
                    for row in 0..8 {
                        ui.label(format!("scrolled {row}"));
                    }
                });
                $egui::ComboBox::from_id_salt("pick").selected_text("chosen").width(140.0).show_ui(ui, |ui| {
                    let _ = ui.selectable_label(true, "chosen");
                });
                $egui::CollapsingHeader::new("More").default_open(true).show(ui, |ui| ui.label("what is under it"));
                ui.scope(|ui| {
                    ui.multiply_opacity(0.5);
                    ui.label("faded");
                });
                let (area, _) = ui.allocate_exact_size(vec2(220.0, 40.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(area, 4.0, t.panel2);
                p.rect_stroke(area, 4.0, Stroke::new(1.0, t.line), StrokeKind::Inside);
                p.rect_filled(area.shrink(12.0), $egui::CornerRadius { nw: 9, ne: 2, sw: 2, se: 9 }, t.accent);
                p.text(area.center(), Align2::CENTER_CENTER, "painted", FontId::proportional(12.0), t.ink);
                p.text(area.left_top(), Align2::LEFT_TOP, "\u{f0219}", FontId::new(13.0, FontFamily::Name("icons".into())), t.ink3);
                p.line_segment([area.left_top(), area.right_bottom()], Stroke::new(1.5, t.accent));
                p.hline(area.x_range(), area.bottom() - 2.0, Stroke::new(1.0, t.error));
                p.circle_filled(area.center(), 3.0, t.error);
                p.circle_stroke(area.right_center(), 5.0, Stroke::new(1.2, t.ink3));
                p.with_clip_rect(area.shrink(4.0)).rect_filled(area, 0.0, t.bg.gamma_multiply(0.3));
                let spot = egui::Rect::from_min_size(area.left_bottom() + vec2(0.0, 4.0), vec2(160.0, 22.0));
                ui.put(spot, Label::new("put in a place"));
                ui.scope_builder(UiBuilder::new().max_rect(spot.translate(vec2(180.0, 0.0))).layout(Layout::right_to_left(Align::Center)), |ui| ui.label("built at the right"));
                // (Last: a layout takes all the room that is left.)
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let _ = ui.button("in what is left");
                    ui.label("beside it");
                });
            }
        };
    }
    page_in!(page_in_egui, egui);
    page_in!(page_in_sdk, sdk);

    /// A plugin's page looks as it would drawn with egui itself: the same
    /// page through the SDK paints the same shapes, in the same places.
    #[test]
    fn the_sdk_draws_what_egui_does() {
        let fields = Fields { name: "oak".into(), note: String::new(), runs: 3, share: 42.0, on: true, pick: 1 };
        let run = |through_sdk: bool| {
            let (ctx, t) = test_ctx(true, true);
            let (mut time, mut s, mut out) = (0.0, fields.clone(), vec![]);
            // (A few frames: grids and wrapped rows settle in the second.)
            for _ in 0..3 {
                out = if through_sdk {
                    oak_sdk::rt::attach(&TABLE);
                    shapes(&run_frame(&ctx, vec2(640.0, 1400.0), &mut time, 0.05, vec![], |ui| {
                        crate::plugins::begin_frame(ui.ctx(), &t);
                        page_in_sdk(&mut unsafe { sdk::Ui::from_raw(ui_ptr(ui)) }, &t, &mut s);
                    }))
                } else {
                    shapes(&run_frame(&ctx, vec2(640.0, 1400.0), &mut time, 0.05, vec![], |ui| page_in_egui(ui, &t, &mut s)))
                };
            }
            out
        };
        let (real, ours) = (run(false), run(true));
        if std::env::var_os("OAK_SHOW_SHAPES").is_some() {
            let short = |s: &String| s.chars().take(100).collect::<String>();
            for i in 0..real.len().max(ours.len()) {
                let (a, b) = (real.get(i).map(short).unwrap_or_default(), ours.get(i).map(short).unwrap_or_default());
                eprintln!("{i:3} {} {a}", if real.get(i) == ours.get(i) { "  " } else { "!=" });
                if real.get(i) != ours.get(i) {
                    eprintln!("    -> {b}");
                }
            }
        }
        assert!(real.len() > 50, "the page drew {} shapes", real.len());
        for want in ["text \"3 runs\"", "text \"42\"", "text \"scrolled 2\"", "text \"what is under it\"", "nw: 18, ne: 5", "nw: 9, ne: 2", "text \"built at the right\"", "text \"off\"", "text \"second\"", "circle"] {
            assert!(real.iter().any(|s| s.contains(want)), "the page didn't draw {want}");
        }
        for (i, (a, b)) in real.iter().zip(&ours).enumerate() {
            assert_eq!(a, b, "shape {i} differs (egui's, then the SDK's)");
        }
        assert_eq!(real.len(), ours.len(), "egui drew {:?} more", real.get(ours.len()..));
        // (All of it was on the screen: nothing was left out as unseen.)
        assert!(real.iter().any(|s| s.contains("text \"in what is left\"")), "the page ran off the screen");
    }
}
