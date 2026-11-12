//! The plugin interface itself: C types and a table of C functions.
//!
//! This file is the single source of it. `include/oak_plugin.h` is written
//! from what is declared here (`header()`; a test keeps the file current),
//! and the rest of this crate is a Rust wrapper over it. A plugin in any
//! language that can export one C function and call C function pointers
//! works the same way.
//!
//! How it stays stable:
//!
//! - A plugin exports `oak_plugin`, which returns an `OakPlugin`: who it is
//!   and its `init`. Oak calls `init` with the `OakApi`, a table of
//!   functions, and the plugin registers its parts through it.
//! - The table only grows at its end (`OAK_API_VERSION` counts up). A
//!   plugin says which version it was built against; any Oak with that
//!   version or a later one runs it.
//! - Only C data crosses: integers, floats, `OakStr` (UTF-8, borrowed for
//!   the call), `OakRect`, colors as `0xRRGGBBAA`, opaque handles, and
//!   callbacks with a `user` pointer. What Oak hands back as text or bytes
//!   goes to an `OakSink` the caller gives. Anything structured (a list of
//!   accounts, a login, a request) is JSON text.
//! - Structs of options start with their own size and are all zeroes by
//!   default, so fields can be added at their end: each side reads only
//!   as much of a struct as the other says it has (`read`, `write`), and
//!   what is missing is zero. They hold one another by pointer, never by
//!   value. `OakStr`, `OakBytes`, `OakRect` and `OakResponse` have no
//!   size and never change.
//! - `abi/<version>.txt` records each version of the interface, and a
//!   test refuses a change that would break a plugin built for one.
//!
//! Threads: functions taking an `OakUi`, and those named `ui_`, `paint_`,
//! `input_`, `app_`, `oak_`, `window`, `texture_` work on the UI
//! thread, inside a callback Oak made. The rest work from any thread.

#![allow(clippy::missing_safety_doc, clippy::too_many_arguments)]

use std::ffi::{c_char, c_void};

/// Changes only if the interface is ever broken (never, by intent).
pub const OAK_ABI: u32 = 1;
/// Counts up whenever functions are added at the table's end.
pub const OAK_API_VERSION: u32 = 1;
/// The symbol a plugin exports: `const OakPlugin *oak_plugin(void)`.
pub const ENTRY_SYMBOL: &[u8] = b"oak_plugin\0";

/// An opaque place to draw: valid only inside the callback it came with.
#[repr(C)]
pub struct OakUi {
    _private: [u8; 0],
}

/// A declared struct, for the header.
pub struct Struct {
    pub name: &'static str,
    pub doc: &'static [&'static str],
    pub fields: &'static [Field],
}

pub struct Field {
    pub name: &'static str,
    pub ty: &'static str,
    pub doc: &'static [&'static str],
}

/// A declared function or callback type, for the header.
pub struct Function {
    pub name: &'static str,
    pub doc: &'static [&'static str],
    pub args: &'static [(&'static str, &'static str)],
    pub ret: &'static str,
}

pub struct Const {
    pub name: &'static str,
    pub doc: &'static [&'static str],
    pub value: u32,
}

/// A struct that starts with its own size, a `u32`, so that it can grow
/// at its end: whoever reads one made by another build takes only what
/// both know.
///
/// # Safety
/// The struct is `repr(C)`, its first field is `size: u32`, and all
/// zeroes is a valid value of it.
pub unsafe trait Growing: Copy + Default {}

macro_rules! growing {
    ($name:ident; size $($rest:ident)*) => { unsafe impl Growing for $name {} };
    ($name:ident; $($rest:ident)*) => {};
}

/// The struct at `p` as this build knows it: as much of it as its maker
/// says it has (its `size`), and zeroes for the rest. Null: all zeroes.
///
/// # Safety
/// `p` is null or points to at least as many bytes as its `size` says.
pub unsafe fn read<T: Growing>(p: *const T) -> T {
    let mut v = T::default();
    if !p.is_null() {
        let have = unsafe { (p as *const u32).read_unaligned() } as usize;
        let n = have.min(std::mem::size_of::<T>());
        unsafe { std::ptr::copy_nonoverlapping(p as *const u8, &mut v as *mut T as *mut u8, n) };
    }
    v
}

/// Fills the struct at `p` from `v`: as much as its owner made room for
/// (its `size`, which stays as it was).
///
/// # Safety
/// `p` points to at least as many bytes as its `size` says.
pub unsafe fn write<T: Growing>(p: *mut T, v: &T) {
    let room = unsafe { (p as *const u32).read_unaligned() } as usize;
    let n = room.min(std::mem::size_of::<T>());
    let skip = std::mem::size_of::<u32>();
    if n > skip {
        unsafe { std::ptr::copy_nonoverlapping((v as *const T as *const u8).add(skip), (p as *mut u8).add(skip), n - skip) };
    }
}

macro_rules! c_structs {
    ($( $(#[doc = $doc:expr])* pub struct $name:ident { $( $(#[doc = $fdoc:expr])* pub $f:ident : $t:ty, )* } )*) => {
        $(
            $(#[doc = $doc])*
            #[repr(C)]
            #[derive(Clone, Copy)]
            pub struct $name { $( $(#[doc = $fdoc])* pub $f: $t, )* }

            impl Default for $name {
                /// All zeroes: every field's default.
                fn default() -> Self {
                    // (Every field is a number, a pointer or an optional
                    // function pointer: zero is valid for each.)
                    unsafe { std::mem::zeroed() }
                }
            }

            growing!($name; $($f)*);
        )*
        pub const STRUCTS: &[Struct] = &[ $( Struct { name: stringify!($name), doc: &[$($doc),*], fields: &[ $( Field { name: stringify!($f), ty: stringify!($t), doc: &[$($fdoc),*] }, )* ] }, )* ];
    };
}

macro_rules! c_callbacks {
    ($( $(#[doc = $doc:expr])* pub type $name:ident = fn($($a:ident : $t:ty),* $(,)?) $(-> $r:ty)?; )*) => {
        $( $(#[doc = $doc])* pub type $name = unsafe extern "C" fn($($a: $t),*) $(-> $r)?; )*
        pub const CALLBACKS: &[Function] = &[ $( Function { name: stringify!($name), doc: &[$($doc),*], args: &[ $( (stringify!($a), stringify!($t)), )* ], ret: stringify!($($r)?) }, )* ];
    };
}

macro_rules! c_consts {
    ($( $(#[doc = $doc:expr])* pub const $name:ident = $v:expr; )*) => {
        $( $(#[doc = $doc])* pub const $name: u32 = $v; )*
        pub const CONSTS: &[Const] = &[ $( Const { name: stringify!($name), doc: &[$($doc),*], value: $v }, )* ];
    };
}

macro_rules! c_api {
    ($( $(#[doc = $doc:expr])* fn $name:ident($($a:ident : $t:ty),* $(,)?) $(-> $r:ty)?; )*) => {
        /// The functions Oak gives a plugin. `size` is the table's size in
        /// the Oak that made it; `api` its `OAK_API_VERSION`.
        #[repr(C)]
        pub struct OakApi {
            pub size: u32,
            pub abi: u32,
            pub api: u32,
            pub reserved: u32,
            $( $(#[doc = $doc])* pub $name: unsafe extern "C" fn($($a: $t),*) $(-> $r)?, )*
        }

        /// What Oak implements: one function per entry of the table.
        pub trait HostApi {
            $( unsafe extern "C" fn $name($($a: $t),*) $(-> $r)?; )*
        }

        /// The table of `H`'s functions.
        pub const fn table<H: HostApi>() -> OakApi {
            OakApi { size: std::mem::size_of::<OakApi>() as u32, abi: OAK_ABI, api: OAK_API_VERSION, reserved: 0, $( $name: H::$name, )* }
        }

        pub const FUNCTIONS: &[Function] = &[ $( Function { name: stringify!($name), doc: &[$($doc),*], args: &[ $( (stringify!($a), stringify!($t)), )* ], ret: stringify!($($r)?) }, )* ];
    };
}

c_callbacks! {
    /// Takes text or bytes Oak (or a plugin) hands back. May be called
    /// several times for one answer made of several parts.
    pub type OakSink = fn(user: *mut c_void, ptr: *const u8, len: usize);
    /// Draws into `ui`.
    pub type OakBody = fn(user: *mut c_void, ui: *mut OakUi);
    /// A question with a yes or no answer.
    pub type OakAsk = fn(user: *mut c_void) -> bool;
    /// Something to do.
    pub type OakDo = fn(user: *mut c_void);
    /// A question about a tab's key.
    pub type OakAskKey = fn(user: *mut c_void, key: OakStr) -> bool;
    /// Draws the tab `key` into `ui`.
    pub type OakDrawKey = fn(user: *mut c_void, ui: *mut OakUi, key: OakStr);
    /// Draws what belongs in the corner of the tab `key`, whose area is `area`.
    pub type OakCorner = fn(user: *mut c_void, key: OakStr, area: *const OakRect);
    /// Gives text to `out` (called when the text is wanted).
    pub type OakTextFn = fn(user: *mut c_void, out: OakSink, out_user: *mut c_void);
    /// Gives the text a number shows as to `out`.
    pub type OakNumberText = fn(user: *mut c_void, value: f64, out: OakSink, out_user: *mut c_void);
    /// Draws row `index` of a list in `rect`; `row` is its response.
    pub type OakRow = fn(user: *mut c_void, ui: *mut OakUi, index: usize, rect: *const OakRect, row: *const OakResponse);
    /// A part's other calls, by name: `args` is JSON (or empty), `data`
    /// bytes that go with it, and the answer goes to `out`. Returns 0 or
    /// more when it worked (what the number means is the method's), a
    /// negative number when it failed (`out` then got why), and
    /// `OAK_CALL_UNKNOWN` for a method it doesn't have (Oak then does what
    /// it does without it).
    pub type OakCall = fn(user: *mut c_void, method: OakStr, args: OakStr, data: OakBytes, out: OakSink, out_user: *mut c_void) -> i64;
    /// A plugin's start: registers its parts through `api`, which stays
    /// valid for good. Returns false when it can't run.
    pub type OakInit = fn(api: *const OakApi) -> bool;
}

/// What `OakCall` answers for a method it doesn't have.
pub const OAK_CALL_UNKNOWN: i64 = i64::MIN;

c_structs! {
    /// Text, borrowed for the call: UTF-8, `len` bytes, no terminator needed.
    pub struct OakStr {
        pub ptr: *const c_char,
        pub len: usize,
    }

    /// Bytes, borrowed for the call.
    pub struct OakBytes {
        pub ptr: *const u8,
        pub len: usize,
    }

    /// A rectangle in the window, in points: its top left and bottom right.
    pub struct OakRect {
        pub x0: f32,
        pub y0: f32,
        pub x1: f32,
        pub y1: f32,
    }

    /// What a widget answers. `flags` are `OAK_HOVERED`, `OAK_CLICKED`, ...;
    /// `id` names it for `ui_response` until the frame ends.
    pub struct OakResponse {
        pub rect: OakRect,
        pub flags: u32,
        pub id: u32,
        /// How far a drag moved it this frame.
        pub drag_x: f32,
        pub drag_y: f32,
        /// Where the pointer is while it is over or pressing the widget
        /// (`OAK_HAS_POINTER` says whether there is one).
        pub pointer_x: f32,
        pub pointer_y: f32,
        /// The widget's own id (what `focus` takes).
        pub widget: u64,
    }

    /// A plugin: what `oak_plugin` returns, valid for good.
    pub struct OakPlugin {
        /// `sizeof(OakPlugin)`.
        pub size: u32,
        /// `OAK_ABI`.
        pub abi: u32,
        /// The `OAK_API_VERSION` it was built against.
        pub api: u32,
        pub reserved: u32,
        /// Lower case, no spaces. Two plugins with one id don't both load.
        pub id: OakStr,
        /// As Settings shows it.
        pub name: OakStr,
        pub version: OakStr,
        /// A line about what it does.
        pub about: OakStr,
        /// The ids of the plugins it can't do without, separated by
        /// commas: they start first, and it doesn't start without them.
        pub needs: OakStr,
        pub init: Option<OakInit>,
    }

    /// Text and how it looks. Zero is the default everywhere: the theme's
    /// text color, the body size, the text font.
    pub struct OakText {
        /// `sizeof(OakText)`.
        pub size: u32,
        /// `OAK_TEXT_STRONG`, ...
        pub flags: u32,
        pub text: OakStr,
        /// `0xRRGGBBAA`; 0: the default.
        pub color: u32,
        /// In points; 0: the default.
        pub points: f32,
        /// `OAK_FONT_TEXT`, `OAK_FONT_CODE`, `OAK_FONT_ICONS`.
        pub font: u32,
    }

    /// A button.
    pub struct OakButton {
        pub size: u32,
        /// `OAK_BUTTON_SMALL`, ...
        pub flags: u32,
        pub text: *const OakText,
        /// Its background, `0xRRGGBBAA`; 0: the default.
        pub fill: u32,
        /// Its outline; width 0: the default.
        pub stroke_width: f32,
        pub stroke_color: u32,
        /// At least this large; 0: as its text needs.
        pub min_width: f32,
        pub min_height: f32,
        /// A texture before the text (`texture_new`); 0: none.
        pub image: u64,
        pub image_width: f32,
        pub image_height: f32,
    }

    /// A text field. Its text is the caller's: Oak is given the current
    /// text and hands back the new one when it changed.
    pub struct OakTextEdit {
        pub size: u32,
        /// `OAK_EDIT_MULTILINE`, ...
        pub flags: u32,
        /// Its id, the same wherever it is drawn (0: by its position).
        pub id: u64,
        /// Shown while it is empty.
        pub hint: OakStr,
        /// 0: the default width; `OAK_FILL` (infinity): all there is.
        pub width: f32,
        /// Rows of a multiline field; 0: the default.
        pub rows: u32,
        /// The most characters it takes; 0: no limit.
        pub char_limit: u32,
        /// `OAK_FONT_*`.
        pub font: u32,
        pub points: f32,
        /// 0: the default.
        pub text_color: u32,
    }

    /// How a container lays out what is drawn in it.
    pub struct OakLayout {
        pub size: u32,
        /// `OAK_DIR_RIGHT`, ...
        pub dir: u32,
        /// `OAK_ALIGN_MIN`, `OAK_ALIGN_CENTER`, `OAK_ALIGN_MAX`, along the direction.
        pub main_align: u32,
        /// Across it.
        pub cross_align: u32,
        /// `OAK_LAYOUT_WRAP`, ...
        pub flags: u32,
    }

    /// A part of `ui` to draw in.
    pub struct OakChild {
        pub size: u32,
        /// `OAK_CHILD_RECT`, `OAK_CHILD_SIZE`, `OAK_CHILD_LAYOUT`: which of
        /// the fields below are given.
        pub flags: u32,
        /// Exactly here, without moving `ui`'s cursor.
        pub rect: OakRect,
        /// Or this much of `ui`, at its cursor.
        pub width: f32,
        pub height: f32,
        pub layout: *const OakLayout,
        /// Tells it from its siblings (0: by position).
        pub id: u64,
    }

    /// A scrolling area.
    pub struct OakScroll {
        pub size: u32,
        /// `OAK_SCROLL_HORIZONTAL`, ...
        pub flags: u32,
        pub id: u64,
        /// 0: as tall (wide) as there is room.
        pub max_height: f32,
        pub max_width: f32,
    }

    /// A box around what is drawn in it.
    pub struct OakFrame {
        pub size: u32,
        pub flags: u32,
        /// 0: none.
        pub fill: u32,
        pub stroke_width: f32,
        pub stroke_color: u32,
        /// How round its corners are (with `OAK_FRAME_CORNERS`: the top
        /// left one).
        pub radius: f32,
        /// Inside the box: left and right, top and bottom.
        pub pad_x: f32,
        pub pad_y: f32,
        /// Outside it.
        pub margin_x: f32,
        pub margin_y: f32,
        /// With `OAK_FRAME_CORNERS`: the other corners, top right, bottom
        /// left and bottom right.
        pub radius_ne: f32,
        pub radius_sw: f32,
        pub radius_se: f32,
    }

    /// A grid: `ui_end_row` ends each row.
    pub struct OakGrid {
        pub size: u32,
        /// `OAK_GRID_STRIPED`.
        pub flags: u32,
        pub id: u64,
        /// 0: as many as a row has.
        pub columns: u32,
        /// Negative: the default.
        pub spacing_x: f32,
        pub spacing_y: f32,
        pub min_col_width: f32,
    }

    /// A number dragged or typed.
    pub struct OakDrag {
        pub size: u32,
        /// `OAK_DRAG_RANGE`: `min` and `max` bound it.
        pub flags: u32,
        pub min: f64,
        pub max: f64,
        /// How much a point of dragging changes it; 0: the default.
        pub speed: f64,
        pub prefix: OakStr,
        pub suffix: OakStr,
        /// With `OAK_DRAG_DECIMALS`: exactly this many decimals; with
        /// `OAK_DRAG_MAX_DECIMALS`: at most.
        pub decimals: u32,
        /// How the number shows (null: as a number), given `user`.
        pub format: Option<OakNumberText>,
        pub user: *mut c_void,
    }

    /// A window, a floating area or a dialog over the page.
    pub struct OakWindow {
        pub size: u32,
        /// `OAK_WINDOW_NO_TITLE`, ...
        pub flags: u32,
        pub id: u64,
        pub title: OakStr,
        /// With `OAK_WINDOW_AT`: where it is (an area), or starts (a window).
        pub x: f32,
        pub y: f32,
        /// With `OAK_WINDOW_SIZE`: its size.
        pub width: f32,
        pub height: f32,
        /// `OAK_ORDER_*`.
        pub order: u32,
        /// Which of its points is at (`x`, `y`): an `OAK_ANCHOR_*`.
        pub pivot: u32,
    }

    /// A picture in the layout.
    pub struct OakImage {
        pub size: u32,
        /// `OAK_SENSE_*`.
        pub sense: u32,
        pub texture: u64,
        pub width: f32,
        pub height: f32,
        /// 0: untinted.
        pub tint: u32,
        pub radius: f32,
    }

    /// The colors of a theme, `0xRRGGBBAA` each (`app_theme`).
    pub struct OakTheme {
        pub size: u32,
        pub bg: u32,
        pub panel: u32,
        pub panel2: u32,
        pub ink: u32,
        pub ink2: u32,
        pub ink3: u32,
        pub ink4: u32,
        pub accent: u32,
        pub line: u32,
        pub error: u32,
        pub tool_bg: u32,
        pub tool_hover: u32,
    }

    /// An HTTP request.
    pub struct OakRequest {
        pub size: u32,
        /// `OAK_HTTP_NO_REDIRECTS`, `OAK_HTTP_DIRECT`.
        pub flags: u32,
        /// "GET", "POST", ...
        pub method: OakStr,
        pub url: OakStr,
        /// One `Name: value` per line.
        pub headers: OakStr,
        pub body: OakBytes,
        /// Bounds the whole request; 0: 30 seconds.
        pub timeout_ms: u64,
    }

    /// A tab on Home.
    pub struct OakHomeTab {
        pub size: u32,
        /// `OAK_ICON_IS_TEXT`.
        pub flags: u32,
        pub user: *mut c_void,
        /// What `app_go_home` takes.
        pub id: OakStr,
        pub label: OakStr,
        /// A Nerd Font symbol before the label (empty: none).
        pub icon: OakStr,
        /// What hovering it says.
        pub tip: OakStr,
        /// Lower first. Oak's Conversations is 0 and Cloud 10.
        pub order: i32,
        /// Whether the tab is there, asked every frame (none: always).
        pub shown: Option<OakAsk>,
        /// Draws the cell.
        pub show: Option<OakBody>,
    }

    /// Tabs of the plugin's own, by their keys.
    pub struct OakTabKind {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        /// Whether `key` is one of this kind's.
        pub owns: Option<OakAskKey>,
        /// Draws the tab, over the message box.
        pub show: Option<OakDrawKey>,
        /// Draws the tab's box in its top left corner (`oak_corner_area`).
        pub corner: Option<OakCorner>,
        /// "exists" {key}: 1 or 0 (0: Oak closes the tab).
        /// "look" {key}: JSON {title, busy, icon, icon_is_text}.
        /// "composer" {key}: 1 when the tab has the message box.
        /// "send" {key, text, files}: the message box was sent there.
        pub call: Option<OakCall>,
    }

    /// A choice on the New screen.
    pub struct OakNewKind {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        /// What `app_go_new` takes.
        pub id: OakStr,
        pub label: OakStr,
        pub icon: OakStr,
        pub tip: OakStr,
        /// The screen's heading while it is picked.
        pub heading: OakStr,
        /// Draws its rows.
        pub form: Option<OakBody>,
        /// "start" {text, files}: the message box was sent with it picked.
        pub call: Option<OakCall>,
    }

    /// Another service cloud sessions run through.
    pub struct OakCloudBackend {
        pub size: u32,
        /// `OAK_ICON_IS_TEXT`.
        pub flags: u32,
        pub user: *mut c_void,
        /// Its accounts' ids start with it and "~".
        pub id: OakStr,
        pub name: OakStr,
        pub icon: OakStr,
        pub tip: OakStr,
        /// What the New screen says under it.
        pub about: OakStr,
        /// Draws on the New screen while it is picked (none: nothing).
        pub form: Option<OakBody>,
        /// "accounts": JSON [{rid, label, blocked_until}].
        /// "unavailable": why no session can start now (nothing: one can).
        /// "note_limit" {rid, until}.
        /// "request" {rid, method, path, beta, org, body, timeout_ms}:
        /// returns an answer's handle (`http_send`, `http_make`).
        /// "problem" {status}: what to say about that error status.
        pub call: Option<OakCall>,
    }

    /// What a backend's cloud sessions are given.
    pub struct OakSessionKit {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        /// The backend's id (empty: Oak's own sessions).
        pub backend: OakStr,
        /// "ready": 1 or 0. "server": JSON {url, token}.
        /// "server_name", "environment_name", "setup_block", "cli": text.
        /// "setup_script" {server}: text. "variables" {server, token}: a
        /// JSON object. "tags": JSON {setup, files, carry}, each [open,
        /// close]. "notes": JSON [[open, close, note]].
        /// "raw" {method, path, content_type, timeout_ms} with the body as
        /// data: returns an answer's handle.
        pub call: Option<OakCall>,
    }

    /// A theme for Settings to offer: nine colors as `0xRRGGBB`.
    pub struct OakThemeDef {
        pub size: u32,
        /// `line` is the ink at this opacity, 0 to 255.
        pub line_alpha: u32,
        pub id: OakStr,
        pub name: OakStr,
        pub bg: u32,
        pub panel: u32,
        pub panel2: u32,
        pub ink: u32,
        pub ink2: u32,
        pub ink3: u32,
        pub ink4: u32,
        pub accent: u32,
        pub error: u32,
    }

    /// A font for Settings to offer.
    pub struct OakFontDef {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        pub id: OakStr,
        pub name: OakStr,
        /// "faces": hands `out` its regular, bold, italic and bold italic
        /// files, one call each, and returns 4; 0 when they aren't on
        /// this machine yet. "fetch": gets them (a download).
        pub call: Option<OakCall>,
    }

    /// A system prompt.
    pub struct OakRole {
        pub size: u32,
        /// `OAK_ROLE_OFFERED`: the New screen offers it.
        pub flags: u32,
        pub id: OakStr,
        pub name: OakStr,
        /// `{{tools}}` marks where Oak's tool instructions go.
        pub text: OakStr,
        /// A Nerd Font symbol for its conversations' tabs.
        pub icon: OakStr,
    }

    /// Somewhere else logins and keys are kept. Kinds are "claude",
    /// "codex", "openrouter", "nodeclub" and "freebuff".
    pub struct OakVault {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        /// "name": text. "connected", "takes_new": 1 or 0.
        /// "failed" {error}, "unreachable" {error}, "cannot_keep" {error}: 1 or 0.
        /// "list" {kind}: a JSON list. "add" {kind, login}: JSON.
        /// "remove" {kind, id}. "token" {kind, id, refused}: JSON, or null.
        pub call: Option<OakCall>,
    }

    /// Rows on the Settings page, under a heading.
    pub struct OakSettings {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        pub title: OakStr,
        /// Lower first, after Oak's own rows.
        pub order: i32,
        pub show: Option<OakBody>,
    }

    /// A screen that comes before everything else.
    pub struct OakGate {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        /// Whether it is shut now: Oak then shows only `show`.
        pub shut: Option<OakAsk>,
        pub show: Option<OakBody>,
    }

    /// Calls at the turns of Oak's life (each may be none).
    pub struct OakHooks {
        pub size: u32,
        pub flags: u32,
        pub user: *mut c_void,
        /// The window is up, before its first frame.
        pub started: Option<OakDo>,
        /// Every frame, before anything is drawn.
        pub frame: Option<OakDo>,
        /// Draws at the top of Home's Providers column.
        pub providers_top: Option<OakBody>,
        /// Oak is closing.
        pub exiting: Option<OakDo>,
    }
}

c_consts! {
    /// A response's flags.
    pub const OAK_HOVERED = 1 << 0;
    pub const OAK_CLICKED = 1 << 1;
    pub const OAK_SECONDARY_CLICKED = 1 << 2;
    pub const OAK_DOUBLE_CLICKED = 1 << 3;
    pub const OAK_MIDDLE_CLICKED = 1 << 4;
    /// Its value changed (a text field's text, a checkbox).
    pub const OAK_CHANGED = 1 << 5;
    pub const OAK_DRAGGED = 1 << 6;
    pub const OAK_DRAG_STARTED = 1 << 7;
    pub const OAK_DRAG_STOPPED = 1 << 8;
    pub const OAK_HAS_FOCUS = 1 << 9;
    pub const OAK_GAINED_FOCUS = 1 << 10;
    pub const OAK_LOST_FOCUS = 1 << 11;
    pub const OAK_ENABLED = 1 << 12;
    pub const OAK_CONTAINS_POINTER = 1 << 13;
    pub const OAK_POINTER_DOWN_ON = 1 << 14;
    pub const OAK_CLICKED_ELSEWHERE = 1 << 15;
    /// `pointer_x` and `pointer_y` are set.
    pub const OAK_HAS_POINTER = 1 << 16;
    /// A container that opens (a window, a collapsing header, a menu) drew its body.
    pub const OAK_OPEN = 1 << 17;
    /// A dialog's first button was pressed; its Cancel (or Escape).
    pub const OAK_CONFIRMED = 1 << 18;
    pub const OAK_CANCELLED = 1 << 19;

    /// What a widget senses.
    pub const OAK_SENSE_HOVER = 0;
    pub const OAK_SENSE_CLICK = 1;
    pub const OAK_SENSE_DRAG = 2;
    pub const OAK_SENSE_CLICK_AND_DRAG = 3;

    /// `OakText.flags`.
    pub const OAK_TEXT_STRONG = 1 << 0;
    pub const OAK_TEXT_ITALIC = 1 << 1;
    pub const OAK_TEXT_SMALL = 1 << 2;
    pub const OAK_TEXT_WEAK = 1 << 3;
    pub const OAK_TEXT_UNDERLINE = 1 << 4;
    pub const OAK_TEXT_STRIKE = 1 << 5;
    pub const OAK_TEXT_HEADING = 1 << 6;
    /// Wrap at the width there is (a label's default is the layout's).
    pub const OAK_TEXT_WRAP = 1 << 7;
    /// One line, cut short with an ellipsis.
    pub const OAK_TEXT_TRUNCATE = 1 << 8;
    /// One line, as long as it is.
    pub const OAK_TEXT_EXTEND = 1 << 9;
    /// A label whose text can't be selected.
    pub const OAK_TEXT_NO_SELECT = 1 << 10;
    /// A label that senses clicks.
    pub const OAK_TEXT_CLICK = 1 << 11;
    /// egui's monospace style (code without the code font's size).
    pub const OAK_TEXT_MONOSPACE = 1 << 12;
    /// A label's lines start at the left, also where the layout puts
    /// widgets at the right (wrapped text in a message's bubble).
    pub const OAK_TEXT_LEFT = 1 << 13;
    /// A label, checkbox or link that is greyed and takes no clicks.
    pub const OAK_TEXT_DISABLED = 1 << 14;

    /// `OakText.font`.
    pub const OAK_FONT_TEXT = 0;
    pub const OAK_FONT_CODE = 1;
    /// Nerd Font symbols.
    pub const OAK_FONT_ICONS = 2;

    /// `OakButton.flags`.
    pub const OAK_BUTTON_SMALL = 1 << 0;
    pub const OAK_BUTTON_SELECTED = 1 << 1;
    pub const OAK_BUTTON_NO_FRAME = 1 << 2;
    pub const OAK_BUTTON_DISABLED = 1 << 3;
    /// A link: text that is clicked.
    pub const OAK_BUTTON_LINK = 1 << 4;
    pub const OAK_BUTTON_WRAP = 1 << 5;
    /// A choice among others (in a menu, in a row): framed only when it
    /// is `OAK_BUTTON_SELECTED` or under the pointer.
    pub const OAK_BUTTON_SELECTABLE = 1 << 6;

    /// `OakTextEdit.flags`.
    pub const OAK_EDIT_MULTILINE = 1 << 0;
    /// Dots instead of the text.
    pub const OAK_EDIT_PASSWORD = 1 << 1;
    /// Oak's right-click menu (Cut, Copy, Paste, Select all).
    pub const OAK_EDIT_MENU = 1 << 2;
    /// With `OAK_EDIT_MENU`: nothing of the text is offered for copying.
    pub const OAK_EDIT_SECRET = 1 << 3;
    pub const OAK_EDIT_NO_FRAME = 1 << 4;
    pub const OAK_EDIT_DISABLED = 1 << 5;
    /// Tab types a tab instead of moving on.
    pub const OAK_EDIT_LOCK_FOCUS = 1 << 6;
    /// Takes the keyboard when it is drawn.
    pub const OAK_EDIT_FOCUS = 1 << 7;
    /// Text in the small style (and a small Show button on a secret's field).
    pub const OAK_EDIT_SMALL = 1 << 8;
    /// Enter doesn't make a newline in a multiline field (Shift+Enter does).
    pub const OAK_EDIT_ENTER_SENDS = 1 << 9;

    /// `OakLayout.dir`: where the next widget goes.
    pub const OAK_DIR_RIGHT = 0;
    pub const OAK_DIR_LEFT = 1;
    pub const OAK_DIR_DOWN = 2;
    pub const OAK_DIR_UP = 3;
    pub const OAK_ALIGN_MIN = 0;
    pub const OAK_ALIGN_CENTER = 1;
    pub const OAK_ALIGN_MAX = 2;
    /// `OakLayout.flags`.
    pub const OAK_LAYOUT_WRAP = 1 << 0;
    pub const OAK_LAYOUT_CROSS_JUSTIFY = 1 << 1;
    pub const OAK_LAYOUT_MAIN_JUSTIFY = 1 << 2;

    /// `ui_row`'s kinds: a row, one that wraps, one aligned to its top;
    /// a column, one centered; a scope (the same layout, its own style);
    /// one indented.
    pub const OAK_ROW = 0;
    pub const OAK_ROW_WRAPPED = 1;
    pub const OAK_ROW_TOP = 2;
    pub const OAK_COLUMN = 3;
    pub const OAK_COLUMN_CENTERED = 4;
    pub const OAK_SCOPE = 5;
    pub const OAK_INDENT = 6;
    /// What is drawn in it is disabled.
    pub const OAK_DISABLED = 7;
    pub const OAK_COLUMN_CENTERED_JUSTIFIED = 8;

    /// `OakChild.flags`.
    pub const OAK_CHILD_RECT = 1 << 0;
    pub const OAK_CHILD_SIZE = 1 << 1;
    pub const OAK_CHILD_LAYOUT = 1 << 2;

    /// `OakScroll.flags`.
    pub const OAK_SCROLL_HORIZONTAL = 1 << 0;
    pub const OAK_SCROLL_BOTH = 1 << 1;
    /// As small as its content, across (along) its direction.
    pub const OAK_SCROLL_SHRINK_X = 1 << 2;
    pub const OAK_SCROLL_SHRINK_Y = 1 << 3;
    /// Stays at its end while content is added there.
    pub const OAK_SCROLL_STICK_TO_END = 1 << 4;

    pub const OAK_GRID_STRIPED = 1 << 0;
    pub const OAK_DRAG_RANGE = 1 << 0;
    pub const OAK_DRAG_DECIMALS = 1 << 1;
    pub const OAK_DRAG_MAX_DECIMALS = 1 << 2;

    /// `OakFrame.flags`: each corner has a radius of its own.
    pub const OAK_FRAME_CORNERS = 1 << 0;

    /// `OakWindow.flags`.
    pub const OAK_WINDOW_NO_TITLE = 1 << 0;
    pub const OAK_WINDOW_AT = 1 << 1;
    pub const OAK_WINDOW_SIZE = 1 << 2;
    pub const OAK_WINDOW_FIXED = 1 << 3;
    /// With a close button: `OAK_OPEN` is unset in the answer once it was closed.
    pub const OAK_WINDOW_CLOSABLE = 1 << 4;
    pub const OAK_WINDOW_NO_COLLAPSE = 1 << 5;
    /// A bare floating area: no frame, no title.
    pub const OAK_WINDOW_AREA = 1 << 6;
    /// A window that stays at (`x`, `y`): it can't be moved.
    pub const OAK_WINDOW_PINNED = 1 << 7;
    /// It takes no clicks: what is under it does.
    pub const OAK_WINDOW_PASSIVE = 1 << 8;
    pub const OAK_ORDER_MIDDLE = 0;
    pub const OAK_ORDER_BACKGROUND = 1;
    pub const OAK_ORDER_FOREGROUND = 2;
    pub const OAK_ORDER_TOOLTIP = 3;

    /// `ui_get`: what is asked of a `ui`. Sizes come as `x1`, `y1`.
    pub const OAK_GET_AVAILABLE = 0;
    pub const OAK_GET_MAX_RECT = 1;
    pub const OAK_GET_MIN_RECT = 2;
    /// Where the next widget goes (`x0`, `y0`).
    pub const OAK_GET_CURSOR = 3;
    pub const OAK_GET_CLIP_RECT = 4;
    pub const OAK_GET_AVAILABLE_RECT = 5;
    pub const OAK_GET_ITEM_SPACING = 6;
    pub const OAK_GET_INTERACT_SIZE = 7;
    pub const OAK_GET_BUTTON_PADDING = 8;
    /// The window's whole area.
    pub const OAK_GET_SCREEN = 9;
    /// A line's height in the body font (`y1`).
    pub const OAK_GET_ROW_HEIGHT = 10;
    pub const OAK_GET_INDENT = 11;
    /// How many pixels a point is (`x1`).
    pub const OAK_GET_PIXELS_PER_POINT = 12;
    /// Where the next widget's top left corner goes, as the layout places it (`x0`, `y0`).
    pub const OAK_GET_NEXT_POSITION = 13;

    /// `ui_set`: what is set on a `ui`, for what is drawn in it after.
    pub const OAK_SET_WIDTH = 0;
    pub const OAK_SET_MAX_WIDTH = 1;
    pub const OAK_SET_MIN_WIDTH = 2;
    pub const OAK_SET_HEIGHT = 3;
    pub const OAK_SET_MAX_HEIGHT = 4;
    pub const OAK_SET_MIN_HEIGHT = 5;
    pub const OAK_SET_ITEM_SPACING = 6;
    pub const OAK_SET_BUTTON_PADDING = 7;
    pub const OAK_SET_INTERACT_SIZE = 8;
    /// Multiplies the opacity by `a`.
    pub const OAK_SET_OPACITY = 9;
    /// Disables it when `a` is 0.
    pub const OAK_SET_ENABLED = 10;
    /// `a`: 0 extend, 1 wrap, 2 truncate.
    pub const OAK_SET_WRAP = 11;
    /// The rectangle `a`, `b`, `c`, `d`.
    pub const OAK_SET_CLIP_RECT = 12;
    /// The text color `a` (a color as a number), for widgets with no color of their own.
    pub const OAK_SET_TEXT_COLOR = 13;
    pub const OAK_SET_INDENT = 14;

    /// `ui_response`: what to do with a widget's answer.
    /// What hovering it says.
    pub const OAK_RESPONSE_HOVER_TEXT = 0;
    /// What hovering it shows: `body` draws it.
    pub const OAK_RESPONSE_HOVER_UI = 1;
    /// The pointer over it: `value` is an `OAK_CURSOR_*`.
    pub const OAK_RESPONSE_CURSOR = 2;
    /// Its right-click menu: `body` draws it.
    pub const OAK_RESPONSE_CONTEXT_MENU = 3;
    pub const OAK_RESPONSE_REQUEST_FOCUS = 4;
    /// Scrolls it into view: `value` is an `OAK_ALIGN_*` (or 3: just enough).
    pub const OAK_RESPONSE_SCROLL_TO = 5;
    pub const OAK_RESPONSE_HIGHLIGHT = 6;
    /// What hovering it says while it is disabled.
    pub const OAK_RESPONSE_DISABLED_HOVER_TEXT = 7;
    pub const OAK_RESPONSE_SURRENDER_FOCUS = 8;

    pub const OAK_CURSOR_DEFAULT = 0;
    pub const OAK_CURSOR_POINTING_HAND = 1;
    pub const OAK_CURSOR_TEXT = 2;
    pub const OAK_CURSOR_GRAB = 3;
    pub const OAK_CURSOR_GRABBING = 4;
    pub const OAK_CURSOR_RESIZE_HORIZONTAL = 5;
    pub const OAK_CURSOR_RESIZE_VERTICAL = 6;
    pub const OAK_CURSOR_NOT_ALLOWED = 7;
    pub const OAK_CURSOR_WAIT = 8;

    /// `paint_text`'s anchor: which point of the text is at the position.
    pub const OAK_ANCHOR_LEFT_TOP = 0;
    pub const OAK_ANCHOR_CENTER_TOP = 1;
    pub const OAK_ANCHOR_RIGHT_TOP = 2;
    pub const OAK_ANCHOR_LEFT_CENTER = 3;
    pub const OAK_ANCHOR_CENTER_CENTER = 4;
    pub const OAK_ANCHOR_RIGHT_CENTER = 5;
    pub const OAK_ANCHOR_LEFT_BOTTOM = 6;
    pub const OAK_ANCHOR_CENTER_BOTTOM = 7;
    pub const OAK_ANCHOR_RIGHT_BOTTOM = 8;

    /// `paint_rect`'s outline: inside the rectangle, on its edge, outside.
    pub const OAK_STROKE_INSIDE = 0;
    pub const OAK_STROKE_MIDDLE = 1;
    pub const OAK_STROKE_OUTSIDE = 2;

    /// Keys, for `input_key`. Letters and digits are their ASCII codes
    /// ('A' to 'Z', '0' to '9').
    pub const OAK_KEY_ENTER = 256;
    pub const OAK_KEY_ESCAPE = 257;
    pub const OAK_KEY_TAB = 258;
    pub const OAK_KEY_BACKSPACE = 259;
    pub const OAK_KEY_DELETE = 260;
    pub const OAK_KEY_SPACE = 261;
    pub const OAK_KEY_LEFT = 262;
    pub const OAK_KEY_RIGHT = 263;
    pub const OAK_KEY_UP = 264;
    pub const OAK_KEY_DOWN = 265;
    pub const OAK_KEY_HOME = 266;
    pub const OAK_KEY_END = 267;
    pub const OAK_KEY_PAGE_UP = 268;
    pub const OAK_KEY_PAGE_DOWN = 269;
    /// Modifiers held with a key. `OAK_MOD_COMMAND` is Ctrl, or Cmd on a Mac.
    pub const OAK_MOD_SHIFT = 1 << 0;
    pub const OAK_MOD_CTRL = 1 << 1;
    pub const OAK_MOD_ALT = 1 << 2;
    pub const OAK_MOD_COMMAND = 1 << 3;
    /// `input_key`: any modifiers will do; take the key so nothing else sees it.
    pub const OAK_KEY_ANY_MODS = 1 << 8;
    pub const OAK_KEY_CONSUME = 1 << 9;

    /// `input_get`: the pointer's position (`x0`, `y0`; false when it is
    /// outside the window), whether its main button is down, the modifiers
    /// held (as `x0`), how far the wheel scrolled this frame.
    pub const OAK_INPUT_POINTER = 0;
    pub const OAK_INPUT_POINTER_DOWN = 1;
    pub const OAK_INPUT_MODIFIERS = 2;
    pub const OAK_INPUT_SCROLL = 3;
    /// Where the main button last went down.
    pub const OAK_INPUT_PRESS_ORIGIN = 4;

    pub const OAK_HTTP_NO_REDIRECTS = 1 << 0;
    /// No proxy: straight to the address (a service on this machine).
    pub const OAK_HTTP_DIRECT = 1 << 1;

    pub const OAK_ICON_IS_TEXT = 1 << 0;
    pub const OAK_ROLE_OFFERED = 1 << 0;

    /// `oak_markdown`: code blocks colored for their language.
    pub const OAK_MARKDOWN_HIGHLIGHT = 1 << 0;
    /// `texture_new`: smooth when scaled (not pixelated).
    pub const OAK_TEXTURE_LINEAR = 1 << 0;
}

/// A width that takes all there is (`OakTextEdit.width`).
pub const OAK_FILL: f32 = f32::INFINITY;

c_api! {
    // Registering parts: only inside `init`.

    /// A tab on Home.
    fn add_home_tab(part: *const OakHomeTab);
    /// Tabs of the plugin's own.
    fn add_tab_kind(part: *const OakTabKind);
    /// A choice on the New screen.
    fn add_new_kind(part: *const OakNewKind);
    /// A service cloud sessions run through.
    fn add_cloud_backend(part: *const OakCloudBackend);
    /// What a backend's cloud sessions are given.
    fn add_session_kit(part: *const OakSessionKit);
    fn add_theme(part: *const OakThemeDef);
    fn add_font(part: *const OakFontDef);
    fn add_role(part: *const OakRole);
    /// Where logins are kept besides this machine (one at most).
    fn add_vault(part: *const OakVault);
    fn add_settings(part: *const OakSettings);
    fn add_gate(part: *const OakGate);
    fn add_hooks(part: *const OakHooks);

    // Oak's services: any thread.

    /// Oak's version ("0.2.214").
    fn version(out: OakSink, out_user: *mut c_void);
    /// Oak's folder (settings, logins, conversations). A plugin keeps its
    /// files there, under names of its own.
    fn dir(out: OakSink, out_user: *mut c_void);
    /// Appends `text` to crash.log.
    fn log_crash(text: OakStr);
    /// The plugin `id` can't go on (it panicked): Oak calls none of its
    /// parts again and says `why` on Settings.
    fn plugin_failed(id: OakStr, why: OakStr);
    /// Draw the window again soon: a background job changed what it shows.
    fn repaint();
    fn repaint_after(ms: u64);
    /// Now, in milliseconds since 1970.
    fn now_ms() -> i64;
    /// Sends an HTTP request with Oak's client (its proxy settings, TLS
    /// roots and user agent) and waits for the answer's head. Returns the
    /// answer's handle (an error status is an answer too), or 0 when none
    /// came (`error` got why). Close every handle.
    fn http_send(request: *const OakRequest, error: OakSink, error_user: *mut c_void) -> u64;
    fn http_status(answer: u64) -> u32;
    /// The answer's headers, one `name: value` per line, names in lower case.
    fn http_headers(answer: u64, out: OakSink, out_user: *mut c_void);
    /// Reads up to `capacity` bytes of the body as they arrive. Returns
    /// how many (0: the end), or a negative number when reading failed.
    fn http_read(answer: u64, buffer: *mut u8, capacity: usize) -> i64;
    fn http_close(answer: u64);
    /// An answer made here, for a part that returns one.
    fn http_make(status: u32, headers: OakStr, body: OakBytes) -> u64;
    /// Writes a file only its owner can read, whole or not at all.
    fn write_private(path: OakStr, data: OakBytes, error: OakSink, error_user: *mut c_void) -> bool;
    /// A random UUID.
    fn uuid(out: OakSink, out_user: *mut c_void);
    /// What plugins tell each other: the JSON under `name`. False: nothing is there.
    fn shared(name: OakStr, out: OakSink, out_user: *mut c_void) -> bool;
    /// Puts JSON under `name` (empty: takes it away).
    fn share(name: OakStr, json: OakStr);
    /// Whether any login or key is on this machine, or a vault is connected.
    fn signed_in() -> bool;
    /// The vault connected or disconnected, or its logins changed.
    fn vault_changed();
    /// This machine's usage records from `since` (ms since 1970) on,
    /// oldest first, as JSON lines. False: the log couldn't be saved
    /// first (`out` got why).
    fn usage_local(since: i64, out: OakSink, out_user: *mut c_void) -> bool;
    /// The records other machines made (a JSON list), from `source`.
    fn usage_set_shared(source: OakStr, records: OakStr);
    /// More of them. False when Oak holds another source's.
    fn usage_add_shared(source: OakStr, records: OakStr) -> bool;
    /// Which source's shared records the Usage page shows (empty: this
    /// machine's only), and what it says about the sharing.
    fn usage_show(source: OakStr, label: OakStr, detail: OakStr);
    /// Drops every record from before `cutoff`, on disk too.
    fn usage_discard_before(cutoff: i64, error: OakSink, error_user: *mut c_void) -> bool;
    /// Names the cloud session behind tab `key` before it is opened.
    fn cloud_name(key: OakStr, title: OakStr);
    /// Forgets what Oak holds of a backend's sessions.
    fn cloud_forget(backend: OakStr);
    /// A cloud session is being started.
    fn cloud_starting() -> bool;
    /// The accounts Oak's own cloud sessions run on: JSON [{rid, label, blocked_until}].
    fn cloud_accounts(out: OakSink, out_user: *mut c_void);
    /// A picture's bytes as an image block (JSON) for a message.
    fn image_block(bytes: OakBytes, out: OakSink, out_user: *mut c_void) -> bool;
    /// A file's bytes as the block the message box would attach.
    fn file_block(path: OakStr, bytes: OakBytes, out: OakSink, out_user: *mut c_void) -> bool;

    // The window: the UI thread, inside a callback.

    /// The theme in use.
    fn app_theme(out: *mut OakTheme);
    /// Opens (or shows) the tab `key`.
    fn app_open(key: OakStr);
    /// Closes the tabs `gone` says to.
    fn app_close_tabs(gone: OakAskKey, user: *mut c_void);
    /// The key of the tab shown. False: none is (Home, the New screen).
    fn app_shown(out: OakSink, out_user: *mut c_void) -> bool;
    /// Shows Home on tab `tab` (a Home tab's id, "local", "cloud").
    fn app_go_home(tab: OakStr);
    /// Shows the New screen with `kind` picked.
    fn app_go_new(kind: OakStr);
    /// Says `text` over the message box (empty: clears it).
    fn app_hint(text: OakStr);
    /// The message box's text.
    fn app_input(out: OakSink, out_user: *mut c_void);
    fn app_set_input(text: OakStr);
    /// What is attached to the message: a JSON list of blocks.
    fn app_attachments(out: OakSink, out_user: *mut c_void);
    fn app_set_attachments(json: OakStr);
    /// This machine's conversations: JSON [{id, title, folder, model, role, at, busy}].
    fn app_conversations(out: OakSink, out_user: *mut c_void);
    /// Starts a conversation here from JSON {title, role, text, files} and
    /// opens its tab. `out` gets its id, or why it couldn't (false).
    fn app_start_conversation(json: OakStr, out: OakSink, out_user: *mut c_void) -> bool;
    /// The New screen's rows for a conversation here: folder, model, effort.
    fn app_local_form(ui: *mut OakUi);
    /// Draws a transcript the plugin has, as a conversation's is drawn.
    /// `version` changes when it does: `make` is asked for it only then,
    /// as JSON {messages, marks: [{at, failed, text, time}], busy, status}.
    /// `tail` draws what follows the last message; `tail_sig` changes
    /// when that would draw otherwise.
    fn app_transcript(ui: *mut OakUi, key: OakStr, version: u64, make: OakTextFn, make_user: *mut c_void, tail_sig: u64, tail: Option<OakBody>, tail_user: *mut c_void);
    /// The list of a backend's cloud sessions, as Home's Cloud tab shows Oak's own.
    fn app_cloud_list(ui: *mut OakUi, backend: OakStr);

    // Laying out.

    /// A row, a column, a scope (`OAK_ROW`, ...): `body` draws in it.
    fn ui_row(ui: *mut OakUi, kind: u32, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    /// A container with a layout of its own.
    fn ui_layout(ui: *mut OakUi, layout: *const OakLayout, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    /// A part of `ui`: a rectangle, or a size at its cursor.
    fn ui_child(ui: *mut OakUi, child: *const OakChild, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    /// What is drawn in `body` has `id` mixed into its ids.
    fn ui_push_id(ui: *mut OakUi, id: u64, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    fn ui_scroll(ui: *mut OakUi, scroll: *const OakScroll, body: OakBody, user: *mut c_void);
    fn ui_frame(ui: *mut OakUi, frame: *const OakFrame, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    fn ui_grid(ui: *mut OakUi, grid: *const OakGrid, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    fn ui_end_row(ui: *mut OakUi);
    /// A heading that opens and closes what `body` draws under it.
    fn ui_collapsing(ui: *mut OakUi, id: u64, title: *const OakText, default_open: bool, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32;
    /// A drop-down: `selected` is shown, `body` draws the choices.
    fn ui_combo(ui: *mut OakUi, id: u64, selected: *const OakText, width: f32, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32;
    /// A button that opens a menu.
    fn ui_menu_button(ui: *mut OakUi, text: *const OakText, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32;
    /// Closes the menu or popup `ui` is in.
    fn ui_close(ui: *mut OakUi);
    fn ui_add_space(ui: *mut OakUi, amount: f32);
    fn ui_separator(ui: *mut OakUi, out: *mut OakResponse);
    /// Asks `ui` for one of `OAK_GET_*`.
    fn ui_get(ui: *mut OakUi, what: u32, out: *mut OakRect);
    /// Sets one of `OAK_SET_*`.
    fn ui_set(ui: *mut OakUi, what: u32, a: f32, b: f32, c: f32, d: f32);
    /// `ui`'s id: a base for ids of the plugin's own.
    fn ui_id(ui: *mut OakUi) -> u64;
    /// Takes `width` by `height` at the cursor and senses there.
    fn ui_allocate(ui: *mut OakUi, width: f32, height: f32, sense: u32, out: *mut OakResponse) -> u32;
    /// Takes `rect` and senses there.
    fn ui_allocate_rect(ui: *mut OakUi, rect: *const OakRect, sense: u32, out: *mut OakResponse) -> u32;
    /// Senses in `rect` under `id`, taking no room.
    fn ui_interact(ui: *mut OakUi, rect: *const OakRect, id: u64, sense: u32, out: *mut OakResponse) -> u32;
    /// Does one of `OAK_RESPONSE_*` with the answer `response` (an
    /// `OakResponse.id` of this frame; 0: the last widget's). Returns its flags.
    fn ui_response(response: u32, what: u32, value: u32, text: OakStr, body: Option<OakBody>, user: *mut c_void) -> u32;

    // Widgets. Each returns its response's flags; `out` (may be null) gets all of it.

    fn ui_label(ui: *mut OakUi, text: *const OakText, out: *mut OakResponse) -> u32;
    fn ui_button(ui: *mut OakUi, button: *const OakButton, out: *mut OakResponse) -> u32;
    /// A link to `url`, opened in the browser.
    fn ui_hyperlink(ui: *mut OakUi, text: *const OakText, url: OakStr, out: *mut OakResponse) -> u32;
    fn ui_checkbox(ui: *mut OakUi, value: *mut bool, text: *const OakText, out: *mut OakResponse) -> u32;
    fn ui_spinner(ui: *mut OakUi, size: f32, color: u32, out: *mut OakResponse) -> u32;
    fn ui_drag(ui: *mut OakUi, value: *mut f64, drag: *const OakDrag, out: *mut OakResponse) -> u32;
    /// A text field: `text` is what it holds; when it was edited, `changed`
    /// gets the new text.
    fn ui_text_edit(ui: *mut OakUi, edit: *const OakTextEdit, text: OakStr, changed: OakSink, user: *mut c_void, out: *mut OakResponse) -> u32;
    fn ui_image(ui: *mut OakUi, image: *const OakImage, out: *mut OakResponse) -> u32;

    // Painting, at positions in the window. What is painted shows only
    // inside `ui`'s clip rectangle, and inside `clip` when one is given
    // (null: none).

    fn paint_rect(ui: *mut OakUi, rect: *const OakRect, radius: f32, fill: u32, stroke_width: f32, stroke_color: u32, stroke_kind: u32, clip: *const OakRect);
    /// `paint_rect` with a radius for each corner: top left, top right,
    /// bottom left, bottom right.
    fn paint_rect_corners(ui: *mut OakUi, rect: *const OakRect, nw: f32, ne: f32, sw: f32, se: f32, fill: u32, stroke_width: f32, stroke_color: u32, stroke_kind: u32, clip: *const OakRect);
    fn paint_line(ui: *mut OakUi, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: u32, clip: *const OakRect);
    fn paint_circle(ui: *mut OakUi, x: f32, y: f32, radius: f32, fill: u32, stroke_width: f32, stroke_color: u32, clip: *const OakRect);
    /// A line through `count` points (`points`: x, y, x, y, ...), `closed`
    /// back to its start or not; filled when `fill` isn't 0.
    fn paint_path(ui: *mut OakUi, points: *const f32, count: usize, width: f32, color: u32, fill: u32, closed: bool, clip: *const OakRect);
    /// Text with its `anchor` point at (`x`, `y`), wrapped at `wrap_width`
    /// (0: one line). `out` (may be null) gets where it went.
    fn paint_text(ui: *mut OakUi, x: f32, y: f32, anchor: u32, text: *const OakText, wrap_width: f32, out: *mut OakRect, clip: *const OakRect);
    /// A texture in `rect`; `uv` (may be null) is the part of it, 0 to 1.
    fn paint_image(ui: *mut OakUi, texture: u64, rect: *const OakRect, uv: *const OakRect, tint: u32, radius: f32, clip: *const OakRect);
    /// How large `text` is, wrapped at `wrap_width` (0: one line): `x1`, `y1`.
    fn text_size(ui: *mut OakUi, text: *const OakText, wrap_width: f32, out: *mut OakRect);
    /// `text` cut short with an ellipsis to fit `width`.
    fn text_clip(ui: *mut OakUi, text: *const OakText, width: f32, out: OakSink, out_user: *mut c_void);

    // Input and the window.

    /// Whether `key` (an `OAK_KEY_*`, or a letter) was pressed this frame
    /// with exactly the modifiers in `flags` (`OAK_MOD_*`, and
    /// `OAK_KEY_ANY_MODS`, `OAK_KEY_CONSUME`).
    fn input_key(key: u32, flags: u32) -> bool;
    /// One of `OAK_INPUT_*`.
    fn input_get(what: u32, out: *mut OakRect) -> bool;
    /// Seconds since Oak started, for animation.
    fn input_time() -> f64;
    /// Whether the widget `id` has the keyboard; `request` gives it to it.
    fn focus(id: u64, request: bool) -> bool;
    /// 0 to 1, moving toward `on` over a moment: an animation's state under `id`.
    fn animate(id: u64, on: bool) -> f32;
    fn copy_text(text: OakStr);
    /// Opens `url` in the browser.
    fn open_url(url: OakStr);
    /// A window (a floating area with `OAK_WINDOW_AREA`) over the page.
    fn window(window: *const OakWindow, body: OakBody, user: *mut c_void, out: *mut OakResponse) -> u32;
    /// A texture from `width` by `height` RGBA pixels (not premultiplied).
    /// Free it when done; it can be made from any thread once the window is up.
    fn texture_new(name: OakStr, width: u32, height: u32, rgba: *const u8, flags: u32) -> u64;
    /// A texture from an image file's bytes (PNG, JPEG, GIF, WebP); 0 when
    /// they can't be read. `size` (may be null) gets its size as `x1`, `y1`.
    fn texture_decode(name: OakStr, bytes: OakBytes, flags: u32, size: *mut OakRect) -> u64;
    fn texture_free(texture: u64);

    // Oak's own look.

    /// Markdown, as a transcript shows it; `key_a` and `key_b` tell this
    /// text from the others on the page.
    fn oak_markdown(ui: *mut OakUi, text: OakStr, key_a: u64, key_b: u64, flags: u32);
    /// The bar under a message in `message`: Copy (of what `copy` gives),
    /// and when it was sent (`at`, ms since 1970; 0: not said).
    fn oak_message_bar(ui: *mut OakUi, message: *const OakRect, id: u64, copy: OakTextFn, copy_user: *mut c_void, at: i64);
    /// An image block (JSON), at most `max_height` tall; a click opens it large.
    fn oak_image(ui: *mut OakUi, block: OakStr, max_height: f32, out: *mut OakResponse) -> bool;
    /// The right-click menu of a message in `rect`: Copy (the selection,
    /// or what `message` gives), and what `extra` draws.
    fn oak_copy_menu(ui: *mut OakUi, id: u64, rect: *const OakRect, message: Option<OakTextFn>, message_user: *mut c_void, extra: Option<OakBody>, extra_user: *mut c_void);
    /// A chat drawn as widgets in the conversation's column, kept scrolled
    /// to the newest: `body` lays it out top down, its rows `gap` apart.
    fn oak_chat_scroll(ui: *mut OakUi, id: u64, gap: f32, body: OakBody, user: *mut c_void);
    /// One of Home's lists: `count` rows, a line between them, the one
    /// under the pointer lit; `empty` is said when there are none.
    fn oak_list_rows(ui: *mut OakUi, id: OakStr, count: usize, empty: OakStr, row: OakRow, user: *mut c_void);
    /// How wide a box in a chat's top left corner is, in the chat's
    /// `area`; 0 when the window is too narrow for one.
    fn oak_corner_width(area: *const OakRect) -> f32;
    /// The chat's top left corner, over the transcript: what `body` draws there.
    fn oak_corner_area(name: OakStr, area: *const OakRect, body: OakBody, user: *mut c_void);
    /// A box in that corner, `width` wide.
    fn oak_panel_box(ui: *mut OakUi, width: f32, pad_y: f32, gap: f32, body: OakBody, user: *mut c_void);
    /// A caption and its value, at the row's two ends.
    fn oak_kv(ui: *mut OakUi, key: OakStr, value: OakStr, color: u32, out: *mut OakResponse);
    /// A thin bar `fraction` full.
    fn oak_meter(ui: *mut OakUi, width: f32, fraction: f32, color: u32);
    /// A spinner painted in `rect`.
    fn oak_spinner(ui: *mut OakUi, rect: *const OakRect, color: u32);
    /// Small text in `color` (0: the theme's faint one).
    fn oak_caption(ui: *mut OakUi, text: OakStr, color: u32, out: *mut OakResponse) -> u32;
    /// A note in the middle of `rect`.
    fn oak_center_note(ui: *mut OakUi, rect: *const OakRect, text: OakStr);
    /// A spinner and what is being done.
    fn oak_working(ui: *mut OakUi, what: OakStr);
    /// A spinner, or `error` when there is one.
    fn oak_loading(ui: *mut OakUi, error: OakStr);
    fn oak_error_note(ui: *mut OakUi, error: OakStr);
    /// A filled box with `margin` around what `body` draws.
    fn oak_inset(ui: *mut OakUi, fill: u32, margin: f32, body: OakBody, user: *mut c_void, out: *mut OakResponse);
    /// An on/off switch.
    fn oak_switch(ui: *mut OakUi, on: bool, width: f32, height: f32, out: *mut OakResponse) -> u32;
    /// The dot of something running (or paused), at (`x`, `y`).
    fn oak_state_dot(ui: *mut OakUi, x: f32, y: f32, paused: bool);
    /// A section's heading that opens and closes it: `count` after the
    /// title, `right` at its other end. Returns `OAK_CLICKED` when pressed.
    fn oak_drop_head(ui: *mut OakUi, id: u64, height: f32, title: OakStr, count: OakStr, right: OakStr, right_color: u32, enabled: bool, open: bool, out: *mut OakResponse) -> u32;
    /// A text field for a secret, with Oak's menu: masked until its Show
    /// button is pressed. `id` names the field for that state.
    fn oak_secret_edit(ui: *mut OakUi, id: OakStr, edit: *const OakTextEdit, text: OakStr, changed: OakSink, user: *mut c_void, out: *mut OakResponse) -> u32;
    /// A dialog over the dimmed page: `body` draws it. Returns
    /// `OAK_CANCELLED` when it should close (Escape, a click outside).
    fn oak_dialog(id: OakStr, body: OakBody, user: *mut c_void) -> u32;
    /// A dialog that asks before doing something: `title`, what `body`
    /// draws, `error` when the last try failed, a button named `action`
    /// (a spinner while `running`) and Cancel. Returns `OAK_CONFIRMED` or
    /// `OAK_CANCELLED`, or 0.
    fn oak_confirm_dialog(id: OakStr, title: OakStr, body: Option<OakBody>, user: *mut c_void, error: OakStr, action: OakStr, running: bool) -> u32;
}

/// The C header: everything declared above, in C.
pub fn header() -> String {
    let mut h = String::new();
    h.push_str(
        "/* Oak's plugin interface. Written from sdk/src/abi.rs (cargo test -p oak-sdk);\n * don't edit it here. docs/PLUGINS.md says how it is used.\n *\n * A plugin is a dynamic library that exports\n *\n *     const OakPlugin *oak_plugin(void);\n *\n * Oak calls its init with the OakApi, a table of functions that only ever\n * grows at its end, so a plugin built once runs in every later Oak.\n */\n#ifndef OAK_PLUGIN_H\n#define OAK_PLUGIN_H\n\n#include <stdbool.h>\n#include <stddef.h>\n#include <stdint.h>\n#include <string.h>\n\n#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n#if defined(_WIN32)\n#define OAK_EXPORT __declspec(dllexport)\n#else\n#define OAK_EXPORT __attribute__((visibility(\"default\")))\n#endif\n\n",
    );
    h.push_str(&format!("#define OAK_ABI {OAK_ABI}\n#define OAK_API_VERSION {OAK_API_VERSION}\n#define OAK_CALL_UNKNOWN INT64_MIN\n#define OAK_FILL ((float)INFINITY)\n\n"));
    h.push_str("typedef struct OakUi OakUi;\ntypedef struct OakApi OakApi;\n");
    for s in STRUCTS {
        h.push_str(&format!("typedef struct {0} {0};\n", s.name));
    }
    h.push('\n');
    // The two the callbacks take by value come before them.
    let early = ["OakStr", "OakBytes"];
    let write_struct = |h: &mut String, s: &Struct| {
        comment(h, s.doc, "");
        h.push_str(&format!("struct {} {{\n", s.name));
        for f in s.fields {
            comment(h, f.doc, "    ");
            h.push_str(&format!("    {};\n", declare(f.ty, f.name)));
        }
        h.push_str("};\n\n");
    };
    for s in STRUCTS.iter().filter(|s| early.contains(&s.name)) {
        write_struct(&mut h, s);
    }
    for c in CALLBACKS {
        comment(&mut h, c.doc, "");
        h.push_str(&format!("typedef {} (*{})({});\n\n", c_type(c.ret), c.name, arguments(c.args)));
    }
    for s in STRUCTS.iter().filter(|s| !early.contains(&s.name)) {
        write_struct(&mut h, s);
    }
    for c in CONSTS {
        comment(&mut h, c.doc, "");
        h.push_str(&format!("#define {} {}u\n", c.name, c.value));
    }
    h.push_str("\n/* The functions Oak gives a plugin. */\nstruct OakApi {\n    /* This table's size in the Oak that made it, OAK_ABI, and its OAK_API_VERSION. */\n    uint32_t size;\n    uint32_t abi;\n    uint32_t api;\n    uint32_t reserved;\n");
    for f in FUNCTIONS {
        h.push('\n');
        comment(&mut h, f.doc, "    ");
        h.push_str(&format!("    {} (*{})({});\n", c_type(f.ret), f.name, arguments(f.args)));
    }
    h.push_str("};\n\n");
    h.push_str(
        "/* Text from a C string, and from a literal. */\nstatic inline OakStr oak_str(const char *s) { OakStr r = { s, s ? strlen(s) : 0 }; return r; }\n#define OAK_S(literal) ((OakStr){ (literal), sizeof(literal) - 1 })\n\n/* What a plugin exports. */\nOAK_EXPORT const OakPlugin *oak_plugin(void);\n\n#ifdef __cplusplus\n}\n#endif\n\n#endif\n",
    );
    h.replace("#include <string.h>\n", "#include <math.h>\n#include <string.h>\n")
}

fn comment(h: &mut String, doc: &[&str], indent: &str) {
    match doc {
        [] => {}
        [one] => h.push_str(&format!("{indent}/*{} */\n", one.trim_end())),
        lines => {
            for (i, l) in lines.iter().enumerate() {
                h.push_str(&format!("{indent}{}{}\n", if i == 0 { "/*" } else { " *" }, l.trim_end()));
            }
            h.push_str(&format!("{indent} */\n"));
        }
    }
}

fn arguments(args: &[(&str, &str)]) -> String {
    if args.is_empty() {
        return "void".into();
    }
    args.iter().map(|(name, ty)| declare(ty, name)).collect::<Vec<_>>().join(", ")
}

/// `name` declared with the Rust type `ty`, in C.
fn declare(ty: &str, name: &str) -> String {
    let t = c_type(ty);
    if t.ends_with('*') { format!("{t}{name}") } else { format!("{t} {name}") }
}

/// The C spelling of a Rust type as `stringify!` gives it.
fn c_type(ty: &str) -> String {
    let t: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(inner) = t.strip_prefix("*const") {
        return format!("const {} *", c_type(inner).trim_end_matches(' '));
    }
    if let Some(inner) = t.strip_prefix("*mut") {
        let inner = c_type(inner);
        return if inner.ends_with('*') { format!("{inner}*") } else { format!("{inner} *") };
    }
    if let Some(inner) = t.strip_prefix("Option<").and_then(|r| r.strip_suffix('>')) {
        return c_type(inner);
    }
    match t.as_str() {
        "" => "void",
        "c_void" => "void",
        "c_char" => "char",
        "u8" => "uint8_t",
        "u32" => "uint32_t",
        "i32" => "int32_t",
        "u64" => "uint64_t",
        "i64" => "int64_t",
        "usize" => "size_t",
        "f32" => "float",
        "f64" => "double",
        "bool" => "bool",
        other => other,
    }
    .to_string()
}

/// The interface without its comments, one declaration a line: what
/// `abi/<version>.txt` records of each version, for the test that keeps
/// later versions from breaking plugins built for earlier ones.
pub fn shape() -> String {
    let mut out = format!("abi {OAK_ABI}\napi {OAK_API_VERSION}\n");
    let args = |f: &Function| f.args.iter().map(|(n, t)| format!("{n}: {}", t.replace(' ', ""))).collect::<Vec<_>>().join(", ");
    let ret = |f: &Function| if f.ret.is_empty() { String::new() } else { format!(" -> {}", f.ret.replace(' ', "")) };
    for s in STRUCTS {
        let grows = s.fields.first().is_some_and(|f| f.name == "size");
        out.push_str(&format!("struct {} {}\n", s.name, if grows { "grows" } else { "fixed" }));
        for f in s.fields {
            out.push_str(&format!("  {}: {}\n", f.name, f.ty.replace(' ', "")));
        }
    }
    for c in CALLBACKS {
        out.push_str(&format!("callback {}({}){}\n", c.name, args(c), ret(c)));
    }
    for c in CONSTS {
        out.push_str(&format!("const {} = {}\n", c.name, c.value));
    }
    for f in FUNCTIONS {
        out.push_str(&format!("fn {}({}){}\n", f.name, args(f), ret(f)));
    }
    out
}

/// Whether a plugin built for the interface `old` records still works
/// with `new`: Err says what would break it.
pub fn grew_from(old: &str, new: &str) -> Result<(), String> {
    /// A struct's line and its fields'.
    type Fields<'a> = (&'a str, Vec<&'a str>);
    /// The functions in order, the structs, and the other lines.
    fn parts(text: &str) -> (Vec<&str>, Vec<Fields<'_>>, Vec<&str>) {
        let (mut fns, mut structs, mut rest) = (vec![], Vec::<Fields>::new(), vec![]);
        for line in text.lines() {
            if line.starts_with("fn ") {
                fns.push(line);
            } else if line.starts_with("struct ") {
                structs.push((line, vec![]));
            } else if line.starts_with("  ") {
                structs.last_mut().expect("a field outside a struct").1.push(line);
            } else if line.starts_with("callback ") || line.starts_with("const ") || line.starts_with("abi ") {
                rest.push(line);
            }
        }
        (fns, structs, rest)
    }
    let ((old_fns, old_structs, old_rest), (new_fns, new_structs, new_rest)) = (parts(old), parts(new));
    for (i, f) in old_fns.iter().enumerate() {
        match new_fns.get(i) {
            Some(now) if now == f => {}
            Some(now) => return Err(format!("entry {i} of the table was `{f}` and is now `{now}`: functions are only added at the table's end")),
            None => return Err(format!("`{f}` is gone from the table")),
        }
    }
    for (head, fields) in &old_structs {
        let Some((_, now)) = new_structs.iter().find(|(h, _)| h == head) else {
            return Err(format!("`{head}` is gone or changed its kind"));
        };
        let grows = head.ends_with(" grows");
        if now.len() < fields.len() || now[..fields.len()] != fields[..] || (!grows && now.len() != fields.len()) {
            return Err(format!("`{head}` changed: {}", if grows { "fields are only added at its end" } else { "it has no size, so it can never change" }));
        }
    }
    for line in old_rest {
        if !new_rest.contains(&line) {
            return Err(format!("`{line}` is gone or changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `abi/<version>.txt` records every version of the interface. The
    /// newest is what this file declares, and it keeps every earlier
    /// one working: nothing a plugin was built against moves or changes.
    ///
    /// After adding to the interface: count `OAK_API_VERSION` up and run
    /// with `OAK_WRITE_ABI=1` to record the new version. (The same
    /// rewrites the newest record in place, which is only right for a
    /// version no Oak was released with yet.)
    #[test]
    fn no_version_breaks_an_earlier_one() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("abi");
        let newest = dir.join(format!("{OAK_API_VERSION}.txt"));
        if std::env::var_os("OAK_WRITE_ABI").is_some() {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&newest, shape()).unwrap();
        }
        let now = shape();
        assert!(std::fs::read_to_string(&newest).unwrap_or_default() == now, "sdk/abi/{OAK_API_VERSION}.txt isn't what abi.rs declares: the interface changed. Add to it only at the ends, count OAK_API_VERSION up, and run OAK_WRITE_ABI=1 cargo test -p oak-sdk no_version");
        for version in (1..=OAK_API_VERSION).filter(|v| *v != OAK_API_VERSION) {
            let old = std::fs::read_to_string(dir.join(format!("{version}.txt"))).unwrap_or_else(|_| panic!("sdk/abi/{version}.txt is missing"));
            if let Err(why) = grew_from(&old, &now) {
                panic!("a plugin built for version {version} of the interface would break: {why}");
            }
        }
    }

    #[test]
    fn what_breaks_a_plugin_is_caught() {
        let old = "abi 1\nstruct OakStr fixed\n  ptr: *constc_char\nstruct OakText grows\n  size: u32\n  flags: u32\ncallback OakBody(user: *mutc_void)\nconst OAK_ROW = 0\nfn a(x: u32)\nfn b()\n";
        assert_eq!(grew_from(old, old), Ok(()));
        // What is allowed: a function, a field of a struct with a size, a
        // struct, a constant and a callback more.
        let more = "abi 1\nstruct OakStr fixed\n  ptr: *constc_char\nstruct OakText grows\n  size: u32\n  flags: u32\n  color: u32\nstruct OakNew grows\n  size: u32\ncallback OakBody(user: *mutc_void)\ncallback OakMore()\nconst OAK_ROW = 0\nconst OAK_MORE = 1\nfn a(x: u32)\nfn b()\nfn c()\n";
        assert_eq!(grew_from(old, more), Ok(()));
        for (what, broken) in [
            ("a function moved", old.replace("fn a(x: u32)\nfn b()\n", "fn b()\nfn a(x: u32)\n")),
            ("a function was put before the end", old.replace("fn b()\n", "fn c()\nfn b()\n")),
            ("a function's arguments changed", old.replace("fn a(x: u32)", "fn a(x: u64)")),
            ("a function was removed", old.replace("fn b()\n", "")),
            ("a field was put first", old.replace("  flags: u32\n", "  color: u32\n  flags: u32\n")),
            ("a field changed its type", old.replace("  flags: u32", "  flags: u64")),
            ("a struct without a size grew", old.replace("  ptr: *constc_char\n", "  ptr: *constc_char\n  len: usize\n")),
            ("a callback changed", old.replace("callback OakBody(user: *mutc_void)", "callback OakBody(user: *mutc_void, ui: *mutOakUi)")),
            ("a constant changed", old.replace("const OAK_ROW = 0", "const OAK_ROW = 1")),
            ("the abi changed", old.replace("abi 1", "abi 2")),
        ] {
            assert!(grew_from(old, &broken).is_err(), "{what}, and it passed");
        }
    }

    /// A struct made by another build is read only as far as that build
    /// knew it, and filled only as far as it made room.
    #[test]
    fn structs_cross_between_builds_of_different_sizes() {
        // An older build's text: it ends before `color`. What lies after
        // it in memory isn't the struct's.
        let full = OakText { size: std::mem::size_of::<OakText>() as u32, flags: OAK_TEXT_STRONG, color: 0xFF00_00FF, points: 18.0, ..Default::default() };
        let mut older = full;
        older.size = std::mem::offset_of!(OakText, color) as u32;
        let got = unsafe { read(&older) };
        assert_eq!((got.flags, got.color, got.points), (OAK_TEXT_STRONG, 0, 0.0));
        // A newer build's: larger than this one knows; what it knows is read.
        let mut newer = [full, full];
        newer[0].size = 2 * std::mem::size_of::<OakText>() as u32;
        let got = unsafe { read(newer.as_ptr()) };
        assert_eq!((got.flags, got.color, got.points), (OAK_TEXT_STRONG, 0xFF00_00FF, 18.0));
        assert_eq!(unsafe { read::<OakText>(std::ptr::null()) }.flags, 0);

        // Filling a theme an older build made room for only up to `ink`.
        let theme = OakTheme { size: std::mem::size_of::<OakTheme>() as u32, bg: 1, panel: 2, panel2: 3, ink: 4, ink2: 5, ..Default::default() };
        let room = std::mem::offset_of!(OakTheme, ink2) as u32;
        let mut theirs = OakTheme { size: room, ink2: 99, ..Default::default() };
        unsafe { write(&mut theirs, &theme) };
        assert_eq!((theirs.size, theirs.bg, theirs.ink, theirs.ink2), (room, 1, 4, 99));
    }

    /// Every struct of options can grow: it starts with its size, and
    /// holds other structs that can only by pointer.
    #[test]
    fn structs_that_grow_are_held_by_pointer() {
        let grows = |name: &str| STRUCTS.iter().any(|s| s.name == name && s.fields.first().is_some_and(|f| f.name == "size"));
        for s in STRUCTS {
            assert!(grows(s.name) || ["OakStr", "OakBytes", "OakRect", "OakResponse"].contains(&s.name), "{} has no size", s.name);
            for f in s.fields {
                assert!(!grows(f.ty.trim()), "{}.{} holds a {} by value, which can grow", s.name, f.name, f.ty);
            }
        }
    }

    /// `include/oak_plugin.h` is what this file declares. Run with
    /// `OAK_WRITE_HEADER=1` to write it anew after a change here.
    #[test]
    fn the_header_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/oak_plugin.h");
        let want = header();
        if std::env::var_os("OAK_WRITE_HEADER").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(have == want, "sdk/include/oak_plugin.h is out of date: run OAK_WRITE_HEADER=1 cargo test -p oak-sdk the_header");
    }

    /// The table only grows at its end: what a version had stays where it was.
    #[test]
    fn the_table_starts_as_version_1_did() {
        let names: Vec<&str> = FUNCTIONS.iter().map(|f| f.name).collect();
        assert_eq!(&names[..4], ["add_home_tab", "add_tab_kind", "add_new_kind", "add_cloud_backend"]);
        assert_eq!(std::mem::offset_of!(OakApi, add_home_tab), 16);
        assert_eq!(std::mem::size_of::<OakApi>(), 16 + FUNCTIONS.len() * std::mem::size_of::<usize>());
    }
}
