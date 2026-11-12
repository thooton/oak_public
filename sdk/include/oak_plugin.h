/* Oak's plugin interface. Written from sdk/src/abi.rs (cargo test -p oak-sdk);
 * don't edit it here. docs/PLUGINS.md says how it is used.
 *
 * A plugin is a dynamic library that exports
 *
 *     const OakPlugin *oak_plugin(void);
 *
 * Oak calls its init with the OakApi, a table of functions that only ever
 * grows at its end, so a plugin built once runs in every later Oak.
 */
#ifndef OAK_PLUGIN_H
#define OAK_PLUGIN_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <math.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#define OAK_EXPORT __declspec(dllexport)
#else
#define OAK_EXPORT __attribute__((visibility("default")))
#endif

#define OAK_ABI 1
#define OAK_API_VERSION 1
#define OAK_CALL_UNKNOWN INT64_MIN
#define OAK_FILL ((float)INFINITY)

typedef struct OakUi OakUi;
typedef struct OakApi OakApi;
typedef struct OakStr OakStr;
typedef struct OakBytes OakBytes;
typedef struct OakRect OakRect;
typedef struct OakResponse OakResponse;
typedef struct OakPlugin OakPlugin;
typedef struct OakText OakText;
typedef struct OakButton OakButton;
typedef struct OakTextEdit OakTextEdit;
typedef struct OakLayout OakLayout;
typedef struct OakChild OakChild;
typedef struct OakScroll OakScroll;
typedef struct OakFrame OakFrame;
typedef struct OakGrid OakGrid;
typedef struct OakDrag OakDrag;
typedef struct OakWindow OakWindow;
typedef struct OakImage OakImage;
typedef struct OakTheme OakTheme;
typedef struct OakRequest OakRequest;
typedef struct OakHomeTab OakHomeTab;
typedef struct OakTabKind OakTabKind;
typedef struct OakNewKind OakNewKind;
typedef struct OakCloudBackend OakCloudBackend;
typedef struct OakSessionKit OakSessionKit;
typedef struct OakThemeDef OakThemeDef;
typedef struct OakFontDef OakFontDef;
typedef struct OakRole OakRole;
typedef struct OakVault OakVault;
typedef struct OakSettings OakSettings;
typedef struct OakGate OakGate;
typedef struct OakHooks OakHooks;

/* Text, borrowed for the call: UTF-8, `len` bytes, no terminator needed. */
struct OakStr {
    const char *ptr;
    size_t len;
};

/* Bytes, borrowed for the call. */
struct OakBytes {
    const uint8_t *ptr;
    size_t len;
};

/* Takes text or bytes Oak (or a plugin) hands back. May be called
 * several times for one answer made of several parts.
 */
typedef void (*OakSink)(void *user, const uint8_t *ptr, size_t len);

/* Draws into `ui`. */
typedef void (*OakBody)(void *user, OakUi *ui);

/* A question with a yes or no answer. */
typedef bool (*OakAsk)(void *user);

/* Something to do. */
typedef void (*OakDo)(void *user);

/* A question about a tab's key. */
typedef bool (*OakAskKey)(void *user, OakStr key);

/* Draws the tab `key` into `ui`. */
typedef void (*OakDrawKey)(void *user, OakUi *ui, OakStr key);

/* Draws what belongs in the corner of the tab `key`, whose area is `area`. */
typedef void (*OakCorner)(void *user, OakStr key, const OakRect *area);

/* Gives text to `out` (called when the text is wanted). */
typedef void (*OakTextFn)(void *user, OakSink out, void *out_user);

/* Gives the text a number shows as to `out`. */
typedef void (*OakNumberText)(void *user, double value, OakSink out, void *out_user);

/* Draws row `index` of a list in `rect`; `row` is its response. */
typedef void (*OakRow)(void *user, OakUi *ui, size_t index, const OakRect *rect, const OakResponse *row);

/* A part's other calls, by name: `args` is JSON (or empty), `data`
 * bytes that go with it, and the answer goes to `out`. Returns 0 or
 * more when it worked (what the number means is the method's), a
 * negative number when it failed (`out` then got why), and
 * `OAK_CALL_UNKNOWN` for a method it doesn't have (Oak then does what
 * it does without it).
 */
typedef int64_t (*OakCall)(void *user, OakStr method, OakStr args, OakBytes data, OakSink out, void *out_user);

/* A plugin's start: registers its parts through `api`, which stays
 * valid for good. Returns false when it can't run.
 */
typedef bool (*OakInit)(const OakApi *api);

/* A rectangle in the window, in points: its top left and bottom right. */
struct OakRect {
    float x0;
    float y0;
    float x1;
    float y1;
};

/* What a widget answers. `flags` are `OAK_HOVERED`, `OAK_CLICKED`, ...;
 * `id` names it for `ui_response` until the frame ends.
 */
struct OakResponse {
    OakRect rect;
    uint32_t flags;
    uint32_t id;
    /* How far a drag moved it this frame. */
    float drag_x;
    float drag_y;
    /* Where the pointer is while it is over or pressing the widget
     * (`OAK_HAS_POINTER` says whether there is one).
     */
    float pointer_x;
    float pointer_y;
    /* The widget's own id (what `focus` takes). */
    uint64_t widget;
};

/* A plugin: what `oak_plugin` returns, valid for good. */
struct OakPlugin {
    /* `sizeof(OakPlugin)`. */
    uint32_t size;
    /* `OAK_ABI`. */
    uint32_t abi;
    /* The `OAK_API_VERSION` it was built against. */
    uint32_t api;
    uint32_t reserved;
    /* Lower case, no spaces. Two plugins with one id don't both load. */
    OakStr id;
    /* As Settings shows it. */
    OakStr name;
    OakStr version;
    /* A line about what it does. */
    OakStr about;
    /* The ids of the plugins it can't do without, separated by
     * commas: they start first, and it doesn't start without them.
     */
    OakStr needs;
    OakInit init;
};

/* Text and how it looks. Zero is the default everywhere: the theme's
 * text color, the body size, the text font.
 */
struct OakText {
    /* `sizeof(OakText)`. */
    uint32_t size;
    /* `OAK_TEXT_STRONG`, ... */
    uint32_t flags;
    OakStr text;
    /* `0xRRGGBBAA`; 0: the default. */
    uint32_t color;
    /* In points; 0: the default. */
    float points;
    /* `OAK_FONT_TEXT`, `OAK_FONT_CODE`, `OAK_FONT_ICONS`. */
    uint32_t font;
};

/* A button. */
struct OakButton {
    uint32_t size;
    /* `OAK_BUTTON_SMALL`, ... */
    uint32_t flags;
    const OakText *text;
    /* Its background, `0xRRGGBBAA`; 0: the default. */
    uint32_t fill;
    /* Its outline; width 0: the default. */
    float stroke_width;
    uint32_t stroke_color;
    /* At least this large; 0: as its text needs. */
    float min_width;
    float min_height;
    /* A texture before the text (`texture_new`); 0: none. */
    uint64_t image;
    float image_width;
    float image_height;
};

/* A text field. Its text is the caller's: Oak is given the current
 * text and hands back the new one when it changed.
 */
struct OakTextEdit {
    uint32_t size;
    /* `OAK_EDIT_MULTILINE`, ... */
    uint32_t flags;
    /* Its id, the same wherever it is drawn (0: by its position). */
    uint64_t id;
    /* Shown while it is empty. */
    OakStr hint;
    /* 0: the default width; `OAK_FILL` (infinity): all there is. */
    float width;
    /* Rows of a multiline field; 0: the default. */
    uint32_t rows;
    /* The most characters it takes; 0: no limit. */
    uint32_t char_limit;
    /* `OAK_FONT_*`. */
    uint32_t font;
    float points;
    /* 0: the default. */
    uint32_t text_color;
};

/* How a container lays out what is drawn in it. */
struct OakLayout {
    uint32_t size;
    /* `OAK_DIR_RIGHT`, ... */
    uint32_t dir;
    /* `OAK_ALIGN_MIN`, `OAK_ALIGN_CENTER`, `OAK_ALIGN_MAX`, along the direction. */
    uint32_t main_align;
    /* Across it. */
    uint32_t cross_align;
    /* `OAK_LAYOUT_WRAP`, ... */
    uint32_t flags;
};

/* A part of `ui` to draw in. */
struct OakChild {
    uint32_t size;
    /* `OAK_CHILD_RECT`, `OAK_CHILD_SIZE`, `OAK_CHILD_LAYOUT`: which of
     * the fields below are given.
     */
    uint32_t flags;
    /* Exactly here, without moving `ui`'s cursor. */
    OakRect rect;
    /* Or this much of `ui`, at its cursor. */
    float width;
    float height;
    const OakLayout *layout;
    /* Tells it from its siblings (0: by position). */
    uint64_t id;
};

/* A scrolling area. */
struct OakScroll {
    uint32_t size;
    /* `OAK_SCROLL_HORIZONTAL`, ... */
    uint32_t flags;
    uint64_t id;
    /* 0: as tall (wide) as there is room. */
    float max_height;
    float max_width;
};

/* A box around what is drawn in it. */
struct OakFrame {
    uint32_t size;
    uint32_t flags;
    /* 0: none. */
    uint32_t fill;
    float stroke_width;
    uint32_t stroke_color;
    /* How round its corners are (with `OAK_FRAME_CORNERS`: the top
     * left one).
     */
    float radius;
    /* Inside the box: left and right, top and bottom. */
    float pad_x;
    float pad_y;
    /* Outside it. */
    float margin_x;
    float margin_y;
    /* With `OAK_FRAME_CORNERS`: the other corners, top right, bottom
     * left and bottom right.
     */
    float radius_ne;
    float radius_sw;
    float radius_se;
};

/* A grid: `ui_end_row` ends each row. */
struct OakGrid {
    uint32_t size;
    /* `OAK_GRID_STRIPED`. */
    uint32_t flags;
    uint64_t id;
    /* 0: as many as a row has. */
    uint32_t columns;
    /* Negative: the default. */
    float spacing_x;
    float spacing_y;
    float min_col_width;
};

/* A number dragged or typed. */
struct OakDrag {
    uint32_t size;
    /* `OAK_DRAG_RANGE`: `min` and `max` bound it. */
    uint32_t flags;
    double min;
    double max;
    /* How much a point of dragging changes it; 0: the default. */
    double speed;
    OakStr prefix;
    OakStr suffix;
    /* With `OAK_DRAG_DECIMALS`: exactly this many decimals; with
     * `OAK_DRAG_MAX_DECIMALS`: at most.
     */
    uint32_t decimals;
    /* How the number shows (null: as a number), given `user`. */
    OakNumberText format;
    void *user;
};

/* A window, a floating area or a dialog over the page. */
struct OakWindow {
    uint32_t size;
    /* `OAK_WINDOW_NO_TITLE`, ... */
    uint32_t flags;
    uint64_t id;
    OakStr title;
    /* With `OAK_WINDOW_AT`: where it is (an area), or starts (a window). */
    float x;
    float y;
    /* With `OAK_WINDOW_SIZE`: its size. */
    float width;
    float height;
    /* `OAK_ORDER_*`. */
    uint32_t order;
    /* Which of its points is at (`x`, `y`): an `OAK_ANCHOR_*`. */
    uint32_t pivot;
};

/* A picture in the layout. */
struct OakImage {
    uint32_t size;
    /* `OAK_SENSE_*`. */
    uint32_t sense;
    uint64_t texture;
    float width;
    float height;
    /* 0: untinted. */
    uint32_t tint;
    float radius;
};

/* The colors of a theme, `0xRRGGBBAA` each (`app_theme`). */
struct OakTheme {
    uint32_t size;
    uint32_t bg;
    uint32_t panel;
    uint32_t panel2;
    uint32_t ink;
    uint32_t ink2;
    uint32_t ink3;
    uint32_t ink4;
    uint32_t accent;
    uint32_t line;
    uint32_t error;
    uint32_t tool_bg;
    uint32_t tool_hover;
};

/* An HTTP request. */
struct OakRequest {
    uint32_t size;
    /* `OAK_HTTP_NO_REDIRECTS`, `OAK_HTTP_DIRECT`. */
    uint32_t flags;
    /* "GET", "POST", ... */
    OakStr method;
    OakStr url;
    /* One `Name: value` per line. */
    OakStr headers;
    OakBytes body;
    /* Bounds the whole request; 0: 30 seconds. */
    uint64_t timeout_ms;
};

/* A tab on Home. */
struct OakHomeTab {
    uint32_t size;
    /* `OAK_ICON_IS_TEXT`. */
    uint32_t flags;
    void *user;
    /* What `app_go_home` takes. */
    OakStr id;
    OakStr label;
    /* A Nerd Font symbol before the label (empty: none). */
    OakStr icon;
    /* What hovering it says. */
    OakStr tip;
    /* Lower first. Oak's Conversations is 0 and Cloud 10. */
    int32_t order;
    /* Whether the tab is there, asked every frame (none: always). */
    OakAsk shown;
    /* Draws the cell. */
    OakBody show;
};

/* Tabs of the plugin's own, by their keys. */
struct OakTabKind {
    uint32_t size;
    uint32_t flags;
    void *user;
    /* Whether `key` is one of this kind's. */
    OakAskKey owns;
    /* Draws the tab, over the message box. */
    OakDrawKey show;
    /* Draws the tab's box in its top left corner (`oak_corner_area`). */
    OakCorner corner;
    /* "exists" {key}: 1 or 0 (0: Oak closes the tab).
     * "look" {key}: JSON {title, busy, icon, icon_is_text}.
     * "composer" {key}: 1 when the tab has the message box.
     * "send" {key, text, files}: the message box was sent there.
     */
    OakCall call;
};

/* A choice on the New screen. */
struct OakNewKind {
    uint32_t size;
    uint32_t flags;
    void *user;
    /* What `app_go_new` takes. */
    OakStr id;
    OakStr label;
    OakStr icon;
    OakStr tip;
    /* The screen's heading while it is picked. */
    OakStr heading;
    /* Draws its rows. */
    OakBody form;
    /* "start" {text, files}: the message box was sent with it picked. */
    OakCall call;
};

/* Another service cloud sessions run through. */
struct OakCloudBackend {
    uint32_t size;
    /* `OAK_ICON_IS_TEXT`. */
    uint32_t flags;
    void *user;
    /* Its accounts' ids start with it and "~". */
    OakStr id;
    OakStr name;
    OakStr icon;
    OakStr tip;
    /* What the New screen says under it. */
    OakStr about;
    /* Draws on the New screen while it is picked (none: nothing). */
    OakBody form;
    /* "accounts": JSON [{rid, label, blocked_until}].
     * "unavailable": why no session can start now (nothing: one can).
     * "note_limit" {rid, until}.
     * "request" {rid, method, path, beta, org, body, timeout_ms}:
     * returns an answer's handle (`http_send`, `http_make`).
     * "problem" {status}: what to say about that error status.
     */
    OakCall call;
};

/* What a backend's cloud sessions are given. */
struct OakSessionKit {
    uint32_t size;
    uint32_t flags;
    void *user;
    /* The backend's id (empty: Oak's own sessions). */
    OakStr backend;
    /* "ready": 1 or 0. "server": JSON {url, token}.
     * "server_name", "environment_name", "setup_block", "cli": text.
     * "setup_script" {server}: text. "variables" {server, token}: a
     * JSON object. "tags": JSON {setup, files, carry}, each [open,
     * close]. "notes": JSON [[open, close, note]].
     * "raw" {method, path, content_type, timeout_ms} with the body as
     * data: returns an answer's handle.
     */
    OakCall call;
};

/* A theme for Settings to offer: nine colors as `0xRRGGBB`. */
struct OakThemeDef {
    uint32_t size;
    /* `line` is the ink at this opacity, 0 to 255. */
    uint32_t line_alpha;
    OakStr id;
    OakStr name;
    uint32_t bg;
    uint32_t panel;
    uint32_t panel2;
    uint32_t ink;
    uint32_t ink2;
    uint32_t ink3;
    uint32_t ink4;
    uint32_t accent;
    uint32_t error;
};

/* A font for Settings to offer. */
struct OakFontDef {
    uint32_t size;
    uint32_t flags;
    void *user;
    OakStr id;
    OakStr name;
    /* "faces": hands `out` its regular, bold, italic and bold italic
     * files, one call each, and returns 4; 0 when they aren't on
     * this machine yet. "fetch": gets them (a download).
     */
    OakCall call;
};

/* A system prompt. */
struct OakRole {
    uint32_t size;
    /* `OAK_ROLE_OFFERED`: the New screen offers it. */
    uint32_t flags;
    OakStr id;
    OakStr name;
    /* `{{tools}}` marks where Oak's tool instructions go. */
    OakStr text;
    /* A Nerd Font symbol for its conversations' tabs. */
    OakStr icon;
};

/* Somewhere else logins and keys are kept. Kinds are "claude",
 * "codex", "openrouter", "nodeclub" and "freebuff".
 */
struct OakVault {
    uint32_t size;
    uint32_t flags;
    void *user;
    /* "name": text. "connected", "takes_new": 1 or 0.
     * "failed" {error}, "unreachable" {error}, "cannot_keep" {error}: 1 or 0.
     * "list" {kind}: a JSON list. "add" {kind, login}: JSON.
     * "remove" {kind, id}. "token" {kind, id, refused}: JSON, or null.
     */
    OakCall call;
};

/* Rows on the Settings page, under a heading. */
struct OakSettings {
    uint32_t size;
    uint32_t flags;
    void *user;
    OakStr title;
    /* Lower first, after Oak's own rows. */
    int32_t order;
    OakBody show;
};

/* A screen that comes before everything else. */
struct OakGate {
    uint32_t size;
    uint32_t flags;
    void *user;
    /* Whether it is shut now: Oak then shows only `show`. */
    OakAsk shut;
    OakBody show;
};

/* Calls at the turns of Oak's life (each may be none). */
struct OakHooks {
    uint32_t size;
    uint32_t flags;
    void *user;
    /* The window is up, before its first frame. */
    OakDo started;
    /* Every frame, before anything is drawn. */
    OakDo frame;
    /* Draws at the top of Home's Providers column. */
    OakBody providers_top;
    /* Oak is closing. */
    OakDo exiting;
};

/* A response's flags. */
#define OAK_HOVERED 1u
#define OAK_CLICKED 2u
#define OAK_SECONDARY_CLICKED 4u
#define OAK_DOUBLE_CLICKED 8u
#define OAK_MIDDLE_CLICKED 16u
/* Its value changed (a text field's text, a checkbox). */
#define OAK_CHANGED 32u
#define OAK_DRAGGED 64u
#define OAK_DRAG_STARTED 128u
#define OAK_DRAG_STOPPED 256u
#define OAK_HAS_FOCUS 512u
#define OAK_GAINED_FOCUS 1024u
#define OAK_LOST_FOCUS 2048u
#define OAK_ENABLED 4096u
#define OAK_CONTAINS_POINTER 8192u
#define OAK_POINTER_DOWN_ON 16384u
#define OAK_CLICKED_ELSEWHERE 32768u
/* `pointer_x` and `pointer_y` are set. */
#define OAK_HAS_POINTER 65536u
/* A container that opens (a window, a collapsing header, a menu) drew its body. */
#define OAK_OPEN 131072u
/* A dialog's first button was pressed; its Cancel (or Escape). */
#define OAK_CONFIRMED 262144u
#define OAK_CANCELLED 524288u
/* What a widget senses. */
#define OAK_SENSE_HOVER 0u
#define OAK_SENSE_CLICK 1u
#define OAK_SENSE_DRAG 2u
#define OAK_SENSE_CLICK_AND_DRAG 3u
/* `OakText.flags`. */
#define OAK_TEXT_STRONG 1u
#define OAK_TEXT_ITALIC 2u
#define OAK_TEXT_SMALL 4u
#define OAK_TEXT_WEAK 8u
#define OAK_TEXT_UNDERLINE 16u
#define OAK_TEXT_STRIKE 32u
#define OAK_TEXT_HEADING 64u
/* Wrap at the width there is (a label's default is the layout's). */
#define OAK_TEXT_WRAP 128u
/* One line, cut short with an ellipsis. */
#define OAK_TEXT_TRUNCATE 256u
/* One line, as long as it is. */
#define OAK_TEXT_EXTEND 512u
/* A label whose text can't be selected. */
#define OAK_TEXT_NO_SELECT 1024u
/* A label that senses clicks. */
#define OAK_TEXT_CLICK 2048u
/* egui's monospace style (code without the code font's size). */
#define OAK_TEXT_MONOSPACE 4096u
/* A label's lines start at the left, also where the layout puts
 * widgets at the right (wrapped text in a message's bubble).
 */
#define OAK_TEXT_LEFT 8192u
/* A label, checkbox or link that is greyed and takes no clicks. */
#define OAK_TEXT_DISABLED 16384u
/* `OakText.font`. */
#define OAK_FONT_TEXT 0u
#define OAK_FONT_CODE 1u
/* Nerd Font symbols. */
#define OAK_FONT_ICONS 2u
/* `OakButton.flags`. */
#define OAK_BUTTON_SMALL 1u
#define OAK_BUTTON_SELECTED 2u
#define OAK_BUTTON_NO_FRAME 4u
#define OAK_BUTTON_DISABLED 8u
/* A link: text that is clicked. */
#define OAK_BUTTON_LINK 16u
#define OAK_BUTTON_WRAP 32u
/* A choice among others (in a menu, in a row): framed only when it
 * is `OAK_BUTTON_SELECTED` or under the pointer.
 */
#define OAK_BUTTON_SELECTABLE 64u
/* `OakTextEdit.flags`. */
#define OAK_EDIT_MULTILINE 1u
/* Dots instead of the text. */
#define OAK_EDIT_PASSWORD 2u
/* Oak's right-click menu (Cut, Copy, Paste, Select all). */
#define OAK_EDIT_MENU 4u
/* With `OAK_EDIT_MENU`: nothing of the text is offered for copying. */
#define OAK_EDIT_SECRET 8u
#define OAK_EDIT_NO_FRAME 16u
#define OAK_EDIT_DISABLED 32u
/* Tab types a tab instead of moving on. */
#define OAK_EDIT_LOCK_FOCUS 64u
/* Takes the keyboard when it is drawn. */
#define OAK_EDIT_FOCUS 128u
/* Text in the small style (and a small Show button on a secret's field). */
#define OAK_EDIT_SMALL 256u
/* Enter doesn't make a newline in a multiline field (Shift+Enter does). */
#define OAK_EDIT_ENTER_SENDS 512u
/* `OakLayout.dir`: where the next widget goes. */
#define OAK_DIR_RIGHT 0u
#define OAK_DIR_LEFT 1u
#define OAK_DIR_DOWN 2u
#define OAK_DIR_UP 3u
#define OAK_ALIGN_MIN 0u
#define OAK_ALIGN_CENTER 1u
#define OAK_ALIGN_MAX 2u
/* `OakLayout.flags`. */
#define OAK_LAYOUT_WRAP 1u
#define OAK_LAYOUT_CROSS_JUSTIFY 2u
#define OAK_LAYOUT_MAIN_JUSTIFY 4u
/* `ui_row`'s kinds: a row, one that wraps, one aligned to its top;
 * a column, one centered; a scope (the same layout, its own style);
 * one indented.
 */
#define OAK_ROW 0u
#define OAK_ROW_WRAPPED 1u
#define OAK_ROW_TOP 2u
#define OAK_COLUMN 3u
#define OAK_COLUMN_CENTERED 4u
#define OAK_SCOPE 5u
#define OAK_INDENT 6u
/* What is drawn in it is disabled. */
#define OAK_DISABLED 7u
#define OAK_COLUMN_CENTERED_JUSTIFIED 8u
/* `OakChild.flags`. */
#define OAK_CHILD_RECT 1u
#define OAK_CHILD_SIZE 2u
#define OAK_CHILD_LAYOUT 4u
/* `OakScroll.flags`. */
#define OAK_SCROLL_HORIZONTAL 1u
#define OAK_SCROLL_BOTH 2u
/* As small as its content, across (along) its direction. */
#define OAK_SCROLL_SHRINK_X 4u
#define OAK_SCROLL_SHRINK_Y 8u
/* Stays at its end while content is added there. */
#define OAK_SCROLL_STICK_TO_END 16u
#define OAK_GRID_STRIPED 1u
#define OAK_DRAG_RANGE 1u
#define OAK_DRAG_DECIMALS 2u
#define OAK_DRAG_MAX_DECIMALS 4u
/* `OakFrame.flags`: each corner has a radius of its own. */
#define OAK_FRAME_CORNERS 1u
/* `OakWindow.flags`. */
#define OAK_WINDOW_NO_TITLE 1u
#define OAK_WINDOW_AT 2u
#define OAK_WINDOW_SIZE 4u
#define OAK_WINDOW_FIXED 8u
/* With a close button: `OAK_OPEN` is unset in the answer once it was closed. */
#define OAK_WINDOW_CLOSABLE 16u
#define OAK_WINDOW_NO_COLLAPSE 32u
/* A bare floating area: no frame, no title. */
#define OAK_WINDOW_AREA 64u
/* A window that stays at (`x`, `y`): it can't be moved. */
#define OAK_WINDOW_PINNED 128u
/* It takes no clicks: what is under it does. */
#define OAK_WINDOW_PASSIVE 256u
#define OAK_ORDER_MIDDLE 0u
#define OAK_ORDER_BACKGROUND 1u
#define OAK_ORDER_FOREGROUND 2u
#define OAK_ORDER_TOOLTIP 3u
/* `ui_get`: what is asked of a `ui`. Sizes come as `x1`, `y1`. */
#define OAK_GET_AVAILABLE 0u
#define OAK_GET_MAX_RECT 1u
#define OAK_GET_MIN_RECT 2u
/* Where the next widget goes (`x0`, `y0`). */
#define OAK_GET_CURSOR 3u
#define OAK_GET_CLIP_RECT 4u
#define OAK_GET_AVAILABLE_RECT 5u
#define OAK_GET_ITEM_SPACING 6u
#define OAK_GET_INTERACT_SIZE 7u
#define OAK_GET_BUTTON_PADDING 8u
/* The window's whole area. */
#define OAK_GET_SCREEN 9u
/* A line's height in the body font (`y1`). */
#define OAK_GET_ROW_HEIGHT 10u
#define OAK_GET_INDENT 11u
/* How many pixels a point is (`x1`). */
#define OAK_GET_PIXELS_PER_POINT 12u
/* Where the next widget's top left corner goes, as the layout places it (`x0`, `y0`). */
#define OAK_GET_NEXT_POSITION 13u
/* `ui_set`: what is set on a `ui`, for what is drawn in it after. */
#define OAK_SET_WIDTH 0u
#define OAK_SET_MAX_WIDTH 1u
#define OAK_SET_MIN_WIDTH 2u
#define OAK_SET_HEIGHT 3u
#define OAK_SET_MAX_HEIGHT 4u
#define OAK_SET_MIN_HEIGHT 5u
#define OAK_SET_ITEM_SPACING 6u
#define OAK_SET_BUTTON_PADDING 7u
#define OAK_SET_INTERACT_SIZE 8u
/* Multiplies the opacity by `a`. */
#define OAK_SET_OPACITY 9u
/* Disables it when `a` is 0. */
#define OAK_SET_ENABLED 10u
/* `a`: 0 extend, 1 wrap, 2 truncate. */
#define OAK_SET_WRAP 11u
/* The rectangle `a`, `b`, `c`, `d`. */
#define OAK_SET_CLIP_RECT 12u
/* The text color `a` (a color as a number), for widgets with no color of their own. */
#define OAK_SET_TEXT_COLOR 13u
#define OAK_SET_INDENT 14u
/* `ui_response`: what to do with a widget's answer.
 * What hovering it says.
 */
#define OAK_RESPONSE_HOVER_TEXT 0u
/* What hovering it shows: `body` draws it. */
#define OAK_RESPONSE_HOVER_UI 1u
/* The pointer over it: `value` is an `OAK_CURSOR_*`. */
#define OAK_RESPONSE_CURSOR 2u
/* Its right-click menu: `body` draws it. */
#define OAK_RESPONSE_CONTEXT_MENU 3u
#define OAK_RESPONSE_REQUEST_FOCUS 4u
/* Scrolls it into view: `value` is an `OAK_ALIGN_*` (or 3: just enough). */
#define OAK_RESPONSE_SCROLL_TO 5u
#define OAK_RESPONSE_HIGHLIGHT 6u
/* What hovering it says while it is disabled. */
#define OAK_RESPONSE_DISABLED_HOVER_TEXT 7u
#define OAK_RESPONSE_SURRENDER_FOCUS 8u
#define OAK_CURSOR_DEFAULT 0u
#define OAK_CURSOR_POINTING_HAND 1u
#define OAK_CURSOR_TEXT 2u
#define OAK_CURSOR_GRAB 3u
#define OAK_CURSOR_GRABBING 4u
#define OAK_CURSOR_RESIZE_HORIZONTAL 5u
#define OAK_CURSOR_RESIZE_VERTICAL 6u
#define OAK_CURSOR_NOT_ALLOWED 7u
#define OAK_CURSOR_WAIT 8u
/* `paint_text`'s anchor: which point of the text is at the position. */
#define OAK_ANCHOR_LEFT_TOP 0u
#define OAK_ANCHOR_CENTER_TOP 1u
#define OAK_ANCHOR_RIGHT_TOP 2u
#define OAK_ANCHOR_LEFT_CENTER 3u
#define OAK_ANCHOR_CENTER_CENTER 4u
#define OAK_ANCHOR_RIGHT_CENTER 5u
#define OAK_ANCHOR_LEFT_BOTTOM 6u
#define OAK_ANCHOR_CENTER_BOTTOM 7u
#define OAK_ANCHOR_RIGHT_BOTTOM 8u
/* `paint_rect`'s outline: inside the rectangle, on its edge, outside. */
#define OAK_STROKE_INSIDE 0u
#define OAK_STROKE_MIDDLE 1u
#define OAK_STROKE_OUTSIDE 2u
/* Keys, for `input_key`. Letters and digits are their ASCII codes
 * ('A' to 'Z', '0' to '9').
 */
#define OAK_KEY_ENTER 256u
#define OAK_KEY_ESCAPE 257u
#define OAK_KEY_TAB 258u
#define OAK_KEY_BACKSPACE 259u
#define OAK_KEY_DELETE 260u
#define OAK_KEY_SPACE 261u
#define OAK_KEY_LEFT 262u
#define OAK_KEY_RIGHT 263u
#define OAK_KEY_UP 264u
#define OAK_KEY_DOWN 265u
#define OAK_KEY_HOME 266u
#define OAK_KEY_END 267u
#define OAK_KEY_PAGE_UP 268u
#define OAK_KEY_PAGE_DOWN 269u
/* Modifiers held with a key. `OAK_MOD_COMMAND` is Ctrl, or Cmd on a Mac. */
#define OAK_MOD_SHIFT 1u
#define OAK_MOD_CTRL 2u
#define OAK_MOD_ALT 4u
#define OAK_MOD_COMMAND 8u
/* `input_key`: any modifiers will do; take the key so nothing else sees it. */
#define OAK_KEY_ANY_MODS 256u
#define OAK_KEY_CONSUME 512u
/* `input_get`: the pointer's position (`x0`, `y0`; false when it is
 * outside the window), whether its main button is down, the modifiers
 * held (as `x0`), how far the wheel scrolled this frame.
 */
#define OAK_INPUT_POINTER 0u
#define OAK_INPUT_POINTER_DOWN 1u
#define OAK_INPUT_MODIFIERS 2u
#define OAK_INPUT_SCROLL 3u
/* Where the main button last went down. */
#define OAK_INPUT_PRESS_ORIGIN 4u
#define OAK_HTTP_NO_REDIRECTS 1u
/* No proxy: straight to the address (a service on this machine). */
#define OAK_HTTP_DIRECT 2u
#define OAK_ICON_IS_TEXT 1u
#define OAK_ROLE_OFFERED 1u
/* `oak_markdown`: code blocks colored for their language. */
#define OAK_MARKDOWN_HIGHLIGHT 1u
/* `texture_new`: smooth when scaled (not pixelated). */
#define OAK_TEXTURE_LINEAR 1u

/* The functions Oak gives a plugin. */
struct OakApi {
    /* This table's size in the Oak that made it, OAK_ABI, and its OAK_API_VERSION. */
    uint32_t size;
    uint32_t abi;
    uint32_t api;
    uint32_t reserved;

    /* A tab on Home. */
    void (*add_home_tab)(const OakHomeTab *part);

    /* Tabs of the plugin's own. */
    void (*add_tab_kind)(const OakTabKind *part);

    /* A choice on the New screen. */
    void (*add_new_kind)(const OakNewKind *part);

    /* A service cloud sessions run through. */
    void (*add_cloud_backend)(const OakCloudBackend *part);

    /* What a backend's cloud sessions are given. */
    void (*add_session_kit)(const OakSessionKit *part);

    void (*add_theme)(const OakThemeDef *part);

    void (*add_font)(const OakFontDef *part);

    void (*add_role)(const OakRole *part);

    /* Where logins are kept besides this machine (one at most). */
    void (*add_vault)(const OakVault *part);

    void (*add_settings)(const OakSettings *part);

    void (*add_gate)(const OakGate *part);

    void (*add_hooks)(const OakHooks *part);

    /* Oak's version ("0.2.214"). */
    void (*version)(OakSink out, void *out_user);

    /* Oak's folder (settings, logins, conversations). A plugin keeps its
     * files there, under names of its own.
     */
    void (*dir)(OakSink out, void *out_user);

    /* Appends `text` to crash.log. */
    void (*log_crash)(OakStr text);

    /* The plugin `id` can't go on (it panicked): Oak calls none of its
     * parts again and says `why` on Settings.
     */
    void (*plugin_failed)(OakStr id, OakStr why);

    /* Draw the window again soon: a background job changed what it shows. */
    void (*repaint)(void);

    void (*repaint_after)(uint64_t ms);

    /* Now, in milliseconds since 1970. */
    int64_t (*now_ms)(void);

    /* Sends an HTTP request with Oak's client (its proxy settings, TLS
     * roots and user agent) and waits for the answer's head. Returns the
     * answer's handle (an error status is an answer too), or 0 when none
     * came (`error` got why). Close every handle.
     */
    uint64_t (*http_send)(const OakRequest *request, OakSink error, void *error_user);

    uint32_t (*http_status)(uint64_t answer);

    /* The answer's headers, one `name: value` per line, names in lower case. */
    void (*http_headers)(uint64_t answer, OakSink out, void *out_user);

    /* Reads up to `capacity` bytes of the body as they arrive. Returns
     * how many (0: the end), or a negative number when reading failed.
     */
    int64_t (*http_read)(uint64_t answer, uint8_t *buffer, size_t capacity);

    void (*http_close)(uint64_t answer);

    /* An answer made here, for a part that returns one. */
    uint64_t (*http_make)(uint32_t status, OakStr headers, OakBytes body);

    /* Writes a file only its owner can read, whole or not at all. */
    bool (*write_private)(OakStr path, OakBytes data, OakSink error, void *error_user);

    /* A random UUID. */
    void (*uuid)(OakSink out, void *out_user);

    /* What plugins tell each other: the JSON under `name`. False: nothing is there. */
    bool (*shared)(OakStr name, OakSink out, void *out_user);

    /* Puts JSON under `name` (empty: takes it away). */
    void (*share)(OakStr name, OakStr json);

    /* Whether any login or key is on this machine, or a vault is connected. */
    bool (*signed_in)(void);

    /* The vault connected or disconnected, or its logins changed. */
    void (*vault_changed)(void);

    /* This machine's usage records from `since` (ms since 1970) on,
     * oldest first, as JSON lines. False: the log couldn't be saved
     * first (`out` got why).
     */
    bool (*usage_local)(int64_t since, OakSink out, void *out_user);

    /* The records other machines made (a JSON list), from `source`. */
    void (*usage_set_shared)(OakStr source, OakStr records);

    /* More of them. False when Oak holds another source's. */
    bool (*usage_add_shared)(OakStr source, OakStr records);

    /* Which source's shared records the Usage page shows (empty: this
     * machine's only), and what it says about the sharing.
     */
    void (*usage_show)(OakStr source, OakStr label, OakStr detail);

    /* Drops every record from before `cutoff`, on disk too. */
    bool (*usage_discard_before)(int64_t cutoff, OakSink error, void *error_user);

    /* Names the cloud session behind tab `key` before it is opened. */
    void (*cloud_name)(OakStr key, OakStr title);

    /* Forgets what Oak holds of a backend's sessions. */
    void (*cloud_forget)(OakStr backend);

    /* A cloud session is being started. */
    bool (*cloud_starting)(void);

    /* The accounts Oak's own cloud sessions run on: JSON [{rid, label, blocked_until}]. */
    void (*cloud_accounts)(OakSink out, void *out_user);

    /* A picture's bytes as an image block (JSON) for a message. */
    bool (*image_block)(OakBytes bytes, OakSink out, void *out_user);

    /* A file's bytes as the block the message box would attach. */
    bool (*file_block)(OakStr path, OakBytes bytes, OakSink out, void *out_user);

    /* The theme in use. */
    void (*app_theme)(OakTheme *out);

    /* Opens (or shows) the tab `key`. */
    void (*app_open)(OakStr key);

    /* Closes the tabs `gone` says to. */
    void (*app_close_tabs)(OakAskKey gone, void *user);

    /* The key of the tab shown. False: none is (Home, the New screen). */
    bool (*app_shown)(OakSink out, void *out_user);

    /* Shows Home on tab `tab` (a Home tab's id, "local", "cloud"). */
    void (*app_go_home)(OakStr tab);

    /* Shows the New screen with `kind` picked. */
    void (*app_go_new)(OakStr kind);

    /* Says `text` over the message box (empty: clears it). */
    void (*app_hint)(OakStr text);

    /* The message box's text. */
    void (*app_input)(OakSink out, void *out_user);

    void (*app_set_input)(OakStr text);

    /* What is attached to the message: a JSON list of blocks. */
    void (*app_attachments)(OakSink out, void *out_user);

    void (*app_set_attachments)(OakStr json);

    /* This machine's conversations: JSON [{id, title, folder, model, role, at, busy}]. */
    void (*app_conversations)(OakSink out, void *out_user);

    /* Starts a conversation here from JSON {title, role, text, files} and
     * opens its tab. `out` gets its id, or why it couldn't (false).
     */
    bool (*app_start_conversation)(OakStr json, OakSink out, void *out_user);

    /* The New screen's rows for a conversation here: folder, model, effort. */
    void (*app_local_form)(OakUi *ui);

    /* Draws a transcript the plugin has, as a conversation's is drawn.
     * `version` changes when it does: `make` is asked for it only then,
     * as JSON {messages, marks: [{at, failed, text, time}], busy, status}.
     * `tail` draws what follows the last message; `tail_sig` changes
     * when that would draw otherwise.
     */
    void (*app_transcript)(OakUi *ui, OakStr key, uint64_t version, OakTextFn make, void *make_user, uint64_t tail_sig, OakBody tail, void *tail_user);

    /* The list of a backend's cloud sessions, as Home's Cloud tab shows Oak's own. */
    void (*app_cloud_list)(OakUi *ui, OakStr backend);

    /* A row, a column, a scope (`OAK_ROW`, ...): `body` draws in it. */
    void (*ui_row)(OakUi *ui, uint32_t kind, OakBody body, void *user, OakResponse *out);

    /* A container with a layout of its own. */
    void (*ui_layout)(OakUi *ui, const OakLayout *layout, OakBody body, void *user, OakResponse *out);

    /* A part of `ui`: a rectangle, or a size at its cursor. */
    void (*ui_child)(OakUi *ui, const OakChild *child, OakBody body, void *user, OakResponse *out);

    /* What is drawn in `body` has `id` mixed into its ids. */
    void (*ui_push_id)(OakUi *ui, uint64_t id, OakBody body, void *user, OakResponse *out);

    void (*ui_scroll)(OakUi *ui, const OakScroll *scroll, OakBody body, void *user);

    void (*ui_frame)(OakUi *ui, const OakFrame *frame, OakBody body, void *user, OakResponse *out);

    void (*ui_grid)(OakUi *ui, const OakGrid *grid, OakBody body, void *user, OakResponse *out);

    void (*ui_end_row)(OakUi *ui);

    /* A heading that opens and closes what `body` draws under it. */
    uint32_t (*ui_collapsing)(OakUi *ui, uint64_t id, const OakText *title, bool default_open, OakBody body, void *user, OakResponse *out);

    /* A drop-down: `selected` is shown, `body` draws the choices. */
    uint32_t (*ui_combo)(OakUi *ui, uint64_t id, const OakText *selected, float width, OakBody body, void *user, OakResponse *out);

    /* A button that opens a menu. */
    uint32_t (*ui_menu_button)(OakUi *ui, const OakText *text, OakBody body, void *user, OakResponse *out);

    /* Closes the menu or popup `ui` is in. */
    void (*ui_close)(OakUi *ui);

    void (*ui_add_space)(OakUi *ui, float amount);

    void (*ui_separator)(OakUi *ui, OakResponse *out);

    /* Asks `ui` for one of `OAK_GET_*`. */
    void (*ui_get)(OakUi *ui, uint32_t what, OakRect *out);

    /* Sets one of `OAK_SET_*`. */
    void (*ui_set)(OakUi *ui, uint32_t what, float a, float b, float c, float d);

    /* `ui`'s id: a base for ids of the plugin's own. */
    uint64_t (*ui_id)(OakUi *ui);

    /* Takes `width` by `height` at the cursor and senses there. */
    uint32_t (*ui_allocate)(OakUi *ui, float width, float height, uint32_t sense, OakResponse *out);

    /* Takes `rect` and senses there. */
    uint32_t (*ui_allocate_rect)(OakUi *ui, const OakRect *rect, uint32_t sense, OakResponse *out);

    /* Senses in `rect` under `id`, taking no room. */
    uint32_t (*ui_interact)(OakUi *ui, const OakRect *rect, uint64_t id, uint32_t sense, OakResponse *out);

    /* Does one of `OAK_RESPONSE_*` with the answer `response` (an
     * `OakResponse.id` of this frame; 0: the last widget's). Returns its flags.
     */
    uint32_t (*ui_response)(uint32_t response, uint32_t what, uint32_t value, OakStr text, OakBody body, void *user);

    uint32_t (*ui_label)(OakUi *ui, const OakText *text, OakResponse *out);

    uint32_t (*ui_button)(OakUi *ui, const OakButton *button, OakResponse *out);

    /* A link to `url`, opened in the browser. */
    uint32_t (*ui_hyperlink)(OakUi *ui, const OakText *text, OakStr url, OakResponse *out);

    uint32_t (*ui_checkbox)(OakUi *ui, bool *value, const OakText *text, OakResponse *out);

    uint32_t (*ui_spinner)(OakUi *ui, float size, uint32_t color, OakResponse *out);

    uint32_t (*ui_drag)(OakUi *ui, double *value, const OakDrag *drag, OakResponse *out);

    /* A text field: `text` is what it holds; when it was edited, `changed`
     * gets the new text.
     */
    uint32_t (*ui_text_edit)(OakUi *ui, const OakTextEdit *edit, OakStr text, OakSink changed, void *user, OakResponse *out);

    uint32_t (*ui_image)(OakUi *ui, const OakImage *image, OakResponse *out);

    void (*paint_rect)(OakUi *ui, const OakRect *rect, float radius, uint32_t fill, float stroke_width, uint32_t stroke_color, uint32_t stroke_kind, const OakRect *clip);

    /* `paint_rect` with a radius for each corner: top left, top right,
     * bottom left, bottom right.
     */
    void (*paint_rect_corners)(OakUi *ui, const OakRect *rect, float nw, float ne, float sw, float se, uint32_t fill, float stroke_width, uint32_t stroke_color, uint32_t stroke_kind, const OakRect *clip);

    void (*paint_line)(OakUi *ui, float x0, float y0, float x1, float y1, float width, uint32_t color, const OakRect *clip);

    void (*paint_circle)(OakUi *ui, float x, float y, float radius, uint32_t fill, float stroke_width, uint32_t stroke_color, const OakRect *clip);

    /* A line through `count` points (`points`: x, y, x, y, ...), `closed`
     * back to its start or not; filled when `fill` isn't 0.
     */
    void (*paint_path)(OakUi *ui, const float *points, size_t count, float width, uint32_t color, uint32_t fill, bool closed, const OakRect *clip);

    /* Text with its `anchor` point at (`x`, `y`), wrapped at `wrap_width`
     * (0: one line). `out` (may be null) gets where it went.
     */
    void (*paint_text)(OakUi *ui, float x, float y, uint32_t anchor, const OakText *text, float wrap_width, OakRect *out, const OakRect *clip);

    /* A texture in `rect`; `uv` (may be null) is the part of it, 0 to 1. */
    void (*paint_image)(OakUi *ui, uint64_t texture, const OakRect *rect, const OakRect *uv, uint32_t tint, float radius, const OakRect *clip);

    /* How large `text` is, wrapped at `wrap_width` (0: one line): `x1`, `y1`. */
    void (*text_size)(OakUi *ui, const OakText *text, float wrap_width, OakRect *out);

    /* `text` cut short with an ellipsis to fit `width`. */
    void (*text_clip)(OakUi *ui, const OakText *text, float width, OakSink out, void *out_user);

    /* Whether `key` (an `OAK_KEY_*`, or a letter) was pressed this frame
     * with exactly the modifiers in `flags` (`OAK_MOD_*`, and
     * `OAK_KEY_ANY_MODS`, `OAK_KEY_CONSUME`).
     */
    bool (*input_key)(uint32_t key, uint32_t flags);

    /* One of `OAK_INPUT_*`. */
    bool (*input_get)(uint32_t what, OakRect *out);

    /* Seconds since Oak started, for animation. */
    double (*input_time)(void);

    /* Whether the widget `id` has the keyboard; `request` gives it to it. */
    bool (*focus)(uint64_t id, bool request);

    /* 0 to 1, moving toward `on` over a moment: an animation's state under `id`. */
    float (*animate)(uint64_t id, bool on);

    void (*copy_text)(OakStr text);

    /* Opens `url` in the browser. */
    void (*open_url)(OakStr url);

    /* A window (a floating area with `OAK_WINDOW_AREA`) over the page. */
    uint32_t (*window)(const OakWindow *window, OakBody body, void *user, OakResponse *out);

    /* A texture from `width` by `height` RGBA pixels (not premultiplied).
     * Free it when done; it can be made from any thread once the window is up.
     */
    uint64_t (*texture_new)(OakStr name, uint32_t width, uint32_t height, const uint8_t *rgba, uint32_t flags);

    /* A texture from an image file's bytes (PNG, JPEG, GIF, WebP); 0 when
     * they can't be read. `size` (may be null) gets its size as `x1`, `y1`.
     */
    uint64_t (*texture_decode)(OakStr name, OakBytes bytes, uint32_t flags, OakRect *size);

    void (*texture_free)(uint64_t texture);

    /* Markdown, as a transcript shows it; `key_a` and `key_b` tell this
     * text from the others on the page.
     */
    void (*oak_markdown)(OakUi *ui, OakStr text, uint64_t key_a, uint64_t key_b, uint32_t flags);

    /* The bar under a message in `message`: Copy (of what `copy` gives),
     * and when it was sent (`at`, ms since 1970; 0: not said).
     */
    void (*oak_message_bar)(OakUi *ui, const OakRect *message, uint64_t id, OakTextFn copy, void *copy_user, int64_t at);

    /* An image block (JSON), at most `max_height` tall; a click opens it large. */
    bool (*oak_image)(OakUi *ui, OakStr block, float max_height, OakResponse *out);

    /* The right-click menu of a message in `rect`: Copy (the selection,
     * or what `message` gives), and what `extra` draws.
     */
    void (*oak_copy_menu)(OakUi *ui, uint64_t id, const OakRect *rect, OakTextFn message, void *message_user, OakBody extra, void *extra_user);

    /* A chat drawn as widgets in the conversation's column, kept scrolled
     * to the newest: `body` lays it out top down, its rows `gap` apart.
     */
    void (*oak_chat_scroll)(OakUi *ui, uint64_t id, float gap, OakBody body, void *user);

    /* One of Home's lists: `count` rows, a line between them, the one
     * under the pointer lit; `empty` is said when there are none.
     */
    void (*oak_list_rows)(OakUi *ui, OakStr id, size_t count, OakStr empty, OakRow row, void *user);

    /* How wide a box in a chat's top left corner is, in the chat's
     * `area`; 0 when the window is too narrow for one.
     */
    float (*oak_corner_width)(const OakRect *area);

    /* The chat's top left corner, over the transcript: what `body` draws there. */
    void (*oak_corner_area)(OakStr name, const OakRect *area, OakBody body, void *user);

    /* A box in that corner, `width` wide. */
    void (*oak_panel_box)(OakUi *ui, float width, float pad_y, float gap, OakBody body, void *user);

    /* A caption and its value, at the row's two ends. */
    void (*oak_kv)(OakUi *ui, OakStr key, OakStr value, uint32_t color, OakResponse *out);

    /* A thin bar `fraction` full. */
    void (*oak_meter)(OakUi *ui, float width, float fraction, uint32_t color);

    /* A spinner painted in `rect`. */
    void (*oak_spinner)(OakUi *ui, const OakRect *rect, uint32_t color);

    /* Small text in `color` (0: the theme's faint one). */
    uint32_t (*oak_caption)(OakUi *ui, OakStr text, uint32_t color, OakResponse *out);

    /* A note in the middle of `rect`. */
    void (*oak_center_note)(OakUi *ui, const OakRect *rect, OakStr text);

    /* A spinner and what is being done. */
    void (*oak_working)(OakUi *ui, OakStr what);

    /* A spinner, or `error` when there is one. */
    void (*oak_loading)(OakUi *ui, OakStr error);

    void (*oak_error_note)(OakUi *ui, OakStr error);

    /* A filled box with `margin` around what `body` draws. */
    void (*oak_inset)(OakUi *ui, uint32_t fill, float margin, OakBody body, void *user, OakResponse *out);

    /* An on/off switch. */
    uint32_t (*oak_switch)(OakUi *ui, bool on, float width, float height, OakResponse *out);

    /* The dot of something running (or paused), at (`x`, `y`). */
    void (*oak_state_dot)(OakUi *ui, float x, float y, bool paused);

    /* A section's heading that opens and closes it: `count` after the
     * title, `right` at its other end. Returns `OAK_CLICKED` when pressed.
     */
    uint32_t (*oak_drop_head)(OakUi *ui, uint64_t id, float height, OakStr title, OakStr count, OakStr right, uint32_t right_color, bool enabled, bool open, OakResponse *out);

    /* A text field for a secret, with Oak's menu: masked until its Show
     * button is pressed. `id` names the field for that state.
     */
    uint32_t (*oak_secret_edit)(OakUi *ui, OakStr id, const OakTextEdit *edit, OakStr text, OakSink changed, void *user, OakResponse *out);

    /* A dialog over the dimmed page: `body` draws it. Returns
     * `OAK_CANCELLED` when it should close (Escape, a click outside).
     */
    uint32_t (*oak_dialog)(OakStr id, OakBody body, void *user);

    /* A dialog that asks before doing something: `title`, what `body`
     * draws, `error` when the last try failed, a button named `action`
     * (a spinner while `running`) and Cancel. Returns `OAK_CONFIRMED` or
     * `OAK_CANCELLED`, or 0.
     */
    uint32_t (*oak_confirm_dialog)(OakStr id, OakStr title, OakBody body, void *user, OakStr error, OakStr action, bool running);
};

/* Text from a C string, and from a literal. */
static inline OakStr oak_str(const char *s) { OakStr r = { s, s ? strlen(s) : 0 }; return r; }
#define OAK_S(literal) ((OakStr){ (literal), sizeof(literal) - 1 })

/* What a plugin exports. */
OAK_EXPORT const OakPlugin *oak_plugin(void);

#ifdef __cplusplus
}
#endif

#endif
