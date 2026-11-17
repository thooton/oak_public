/* oak_view.h: Oak's components made short to call from C.
 *
 * oak_plugin.h is the interface itself; this header is a thin layer over
 * it (all static inline, nothing to link) for what most plugins draw: a
 * page, sections, buttons, list rows, fields, switches, choices, notices,
 * tables, toasts, and prefs kept from one run of Oak to the next.
 *
 * One file of the plugin defines OAK_VIEW_MAIN before including it (that
 * file holds the table's pointer), and its init calls ov_init:
 *
 *     #define OAK_VIEW_MAIN
 *     #include "oak_view.h"
 *
 *     static void page(void *user, OakUi *ui) {
 *         ov_heading(ui, "Counter");
 *         if (ov_primary(ui, "Count")) clicks++;
 *         ov_label(ui, ov_fmt("clicked %d times", clicks));
 *     }
 *     static void show(void *user, OakUi *ui) { ov_page(ui, "Hello", "", page, user); }
 *
 *     static bool init(const OakApi *api) { ov_init(api, "hello"); ... }
 *
 * Texts are C strings (UTF-8, NUL-terminated). The full structs
 * (OakPage, OakItem, OakField, ...) are there for what these leave out:
 * OV_ITEM(...) and the like make one with its size set, for the ov_*_with
 * functions or oak_plugin.h's own.
 *
 * Like the interface, everything here is for the UI thread, but for
 * ov_toast, ov_spawn and the prefs.
 */
#ifndef OAK_VIEW_H
#define OAK_VIEW_H

#include "oak_plugin.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The table Oak gave (ov_init), and the plugin's id (for its prefs). */
extern const OakApi *ov_api;
extern const char *ov_plugin;

#ifdef OAK_VIEW_MAIN
const OakApi *ov_api;
const char *ov_plugin = "";
#endif

/* In the plugin's init, first: keeps the table and the plugin's id. */
static inline void ov_init(const OakApi *api, const char *plugin_id) {
    ov_api = api;
    ov_plugin = plugin_id ? plugin_id : "";
}

/* Structs with their size set, for the *_with functions:
 *     ov_item_with(ui, OV_ITEM(.title = OAK_S("Inbox"), .flags = OAK_ITEM_DOT));
 */
#define OV_PAGE(...) (&(OakPage){ .size = sizeof(OakPage), __VA_ARGS__ })
#define OV_HEADING(...) (&(OakHeading){ .size = sizeof(OakHeading), __VA_ARGS__ })
#define OV_SECTION(...) (&(OakSection){ .size = sizeof(OakSection), __VA_ARGS__ })
#define OV_CARD(...) (&(OakCard){ .size = sizeof(OakCard), __VA_ARGS__ })
#define OV_ACTION(...) (&(OakAction){ .size = sizeof(OakAction), __VA_ARGS__ })
#define OV_ITEM(...) (&(OakItem){ .size = sizeof(OakItem), __VA_ARGS__ })
#define OV_FIELD(...) (&(OakField){ .size = sizeof(OakField), __VA_ARGS__ })
#define OV_CHOICE(...) (&(OakChoice){ .size = sizeof(OakChoice), __VA_ARGS__ })
#define OV_SLIDER(...) (&(OakSlider){ .size = sizeof(OakSlider), __VA_ARGS__ })
#define OV_TABLE(...) (&(OakTable){ .size = sizeof(OakTable), __VA_ARGS__ })
#define OV_LOOK(...) (&(OakLook){ .size = sizeof(OakLook), __VA_ARGS__ })

/* ---------------------------------------------------------------- text */

/* printf into one of a few buffers that take turns: for a label drawn
 * now (the 8th call after reuses the first's). UI thread only.
 */
static inline const char *ov_fmt(const char *format, ...) {
    static char buffers[8][512];
    static unsigned next;
    char *b = buffers[next++ % 8];
    va_list args;
    va_start(args, format);
    vsnprintf(b, sizeof buffers[0], format, args);
    va_end(args);
    return b;
}

/* A text field's buffer: what it holds, and its room (with the NUL). */
typedef struct OvBuf {
    char *text;
    size_t cap;
} OvBuf;

/* An OakSink that copies into an OvBuf, cut short at a whole character. */
static inline void ov_sink_buf(void *user, const uint8_t *ptr, size_t len) {
    OvBuf *b = (OvBuf *)user;
    if (!b->cap) return;
    if (len >= b->cap) {
        len = b->cap - 1;
        while (len > 0 && (ptr[len] & 0xC0) == 0x80) len--;
    }
    if (len) memcpy(b->text, ptr, len);
    b->text[len] = 0;
}

/* ---------------------------------------------------------------- tones */

#define OV_PLAIN OAK_TONE_PLAIN
#define OV_ACCENT OAK_TONE_ACCENT
#define OV_GOOD OAK_TONE_GOOD
#define OV_WARN OAK_TONE_WARN
#define OV_BAD OAK_TONE_BAD

/* ---------------------------------------------------------------- looks */

/* How components look, over Oak's (OakLook: every field optional; 0
 * keeps Oak's). A color from 0xRRGGBB, opaque; a length that is a true
 * zero (square corners, no padding):
 *
 *     ov_next_look(OV_LOOK(.fill = OV_RGB(0x2b5cd6), .ink = OV_RGB(0xffffff), .radius = 6, .height = 36));
 *     ov_primary(ui, "Deploy");
 *
 *     ov_look_scope(ui, OV_LOOK(.accent = OV_RGB(0xe06c9f), .radius = OV_ZERO), page, user);
 */
#define OV_RGB(hex) ((uint32_t)(hex) << 8 | 0xFFu)
#define OV_RGBA(hex) ((uint32_t)(hex))
#define OV_ZERO (-1.0f)

/* The next component drawn (only it) looks as `look` says. */
static inline void ov_next_look(const OakLook *look) {
    ov_api->oak_look_next(look);
}

/* Everything `body` draws looks as `look` says (its colors, corners,
 * outlines, text scale, code font); scopes in scopes add to it.
 */
static inline void ov_look_scope(OakUi *ui, const OakLook *look, OakBody body, void *user) {
    ov_api->oak_look_scope(ui, look, body, user);
}

/* ------------------------------------------------------------ the basics */

/* A line of text: in the theme's text color, wrapping. */
static inline void ov_label(OakUi *ui, const char *text) {
    OakText t = { .size = sizeof t, .flags = OAK_TEXT_WRAP, .text = oak_str(text) };
    ov_api->ui_label(ui, &t, NULL);
}

/* In a color (0: the default) and size (0: the default). */
static inline void ov_text(OakUi *ui, const char *text, uint32_t color, float points) {
    OakText t = { .size = sizeof t, .flags = OAK_TEXT_WRAP, .text = oak_str(text), .color = color, .points = points };
    ov_api->ui_label(ui, &t, NULL);
}

/* Small and faint. */
static inline void ov_caption(OakUi *ui, const char *text) {
    ov_api->oak_caption(ui, oak_str(text), 0, NULL);
}

static inline void ov_space(OakUi *ui, float points) {
    ov_api->ui_add_space(ui, points);
}

/* What `body` draws, side by side; one under the other. */
static inline void ov_row(OakUi *ui, OakBody body, void *user) {
    ov_api->ui_row(ui, OAK_ROW, body, user, NULL);
}

static inline void ov_column(OakUi *ui, OakBody body, void *user) {
    ov_api->ui_row(ui, OAK_COLUMN, body, user, NULL);
}

/* ---------------------------------------------------------- the structure */

/* A page: `body` in a column in the middle of a scroll area, under the
 * title and subtitle (either may be empty). Returns OAK_CANCELLED when
 * a back link (ov_page_with's .back) was pressed.
 */
static inline uint32_t ov_page(OakUi *ui, const char *title, const char *subtitle, OakBody body, void *user) {
    OakPage p = { .size = sizeof p, .title = oak_str(title), .subtitle = oak_str(subtitle) };
    return ov_api->oak_page(ui, &p, body, user);
}

static inline uint32_t ov_page_with(OakUi *ui, const OakPage *page, OakBody body, void *user) {
    return ov_api->oak_page(ui, page, body, user);
}

/* A section's heading; a page's title's size. */
static inline void ov_heading(OakUi *ui, const char *text) {
    OakHeading h = { .size = sizeof h, .level = OAK_HEADING_SECTION, .text = oak_str(text) };
    ov_api->oak_heading(ui, &h, NULL);
}

static inline void ov_title(OakUi *ui, const char *text) {
    OakHeading h = { .size = sizeof h, .level = OAK_HEADING_TITLE, .text = oak_str(text) };
    ov_api->oak_heading(ui, &h, NULL);
}

/* True: its action link was pressed. */
static inline bool ov_heading_with(OakUi *ui, const OakHeading *h) {
    return ov_api->oak_heading(ui, h, NULL) & OAK_CLICKED;
}

/* A section: its heading, and `body` under it in a card. */
static inline void ov_section(OakUi *ui, const char *title, OakBody body, void *user) {
    OakSection s = { .size = sizeof s, .flags = OAK_SECTION_CARD, .title = oak_str(title) };
    ov_api->oak_section(ui, &s, body, user, NULL);
}

/* OAK_OPEN: `body` was drawn; OAK_CLICKED: its action was pressed. */
static inline uint32_t ov_section_with(OakUi *ui, const OakSection *s, OakBody body, void *user) {
    return ov_api->oak_section(ui, s, body, user, NULL);
}

/* `body` in a bordered box. */
static inline void ov_card(OakUi *ui, OakBody body, void *user) {
    OakCard c = { .size = sizeof c };
    ov_api->oak_card(ui, &c, body, user, NULL);
}

static inline uint32_t ov_card_with(OakUi *ui, const OakCard *c, OakBody body, void *user) {
    return ov_api->oak_card(ui, c, body, user, NULL);
}

/* A setting's row: label and help at the left, what `control` draws at
 * the right.
 */
static inline void ov_setting(OakUi *ui, const char *label, const char *help, OakBody control, void *user) {
    ov_api->oak_setting(ui, oak_str(label), oak_str(help), control, user, NULL);
}

/* `count` columns of equal width: `cell` draws each (its `column`). */
static inline void ov_columns(OakUi *ui, size_t count, OakCell cell, void *user) {
    ov_api->oak_columns(ui, count, 0, cell, user);
}

/* -------------------------------------------------------------- buttons */

static inline bool ov_action_of(OakUi *ui, uint32_t kind, const char *text, uint32_t flags) {
    OakAction a = { .size = sizeof a, .flags = flags, .kind = kind, .text = oak_str(text) };
    return ov_api->oak_action(ui, &a, NULL) & OAK_CLICKED;
}

/* True when pressed. */
static inline bool ov_button(OakUi *ui, const char *text) { return ov_action_of(ui, OAK_ACTION_DEFAULT, text, 0); }
static inline bool ov_primary(OakUi *ui, const char *text) { return ov_action_of(ui, OAK_ACTION_PRIMARY, text, 0); }
static inline bool ov_quiet(OakUi *ui, const char *text) { return ov_action_of(ui, OAK_ACTION_QUIET, text, 0); }
static inline bool ov_danger(OakUi *ui, const char *text) { return ov_action_of(ui, OAK_ACTION_DANGER, text, 0); }
static inline bool ov_link(OakUi *ui, const char *text) { return ov_action_of(ui, OAK_ACTION_LINK, text, 0); }

/* A chip, lit when `selected`. */
static inline bool ov_pill(OakUi *ui, const char *text, bool selected) {
    return ov_action_of(ui, OAK_ACTION_PILL, text, selected ? OAK_ACTION_SELECTED : 0);
}

/* A square with a Nerd Font symbol; `tip` says what it does. */
static inline bool ov_icon_button(OakUi *ui, const char *glyph, const char *tip) {
    OakAction a = { .size = sizeof a, .kind = OAK_ACTION_ICON, .icon = oak_str(glyph), .tip = oak_str(tip) };
    return ov_api->oak_action(ui, &a, NULL) & OAK_CLICKED;
}

static inline uint32_t ov_action_with(OakUi *ui, const OakAction *a) {
    return ov_api->oak_action(ui, a, NULL);
}

/* ---------------------------------------------------------------- lists */

/* A row of a list (subtitle may be empty or NULL). True when clicked. */
static inline bool ov_item(OakUi *ui, const char *title, const char *subtitle) {
    OakItem i = { .size = sizeof i, .title = oak_str(title), .subtitle = oak_str(subtitle) };
    return ov_api->oak_item(ui, &i, NULL) & OAK_CLICKED;
}

static inline uint32_t ov_item_with(OakUi *ui, const OakItem *item) {
    return ov_api->oak_item(ui, item, NULL);
}

/* A table of `rows` rows: `cell` draws each cell. Returns the row
 * clicked (with OAK_TABLE_CLICK in `flags`), or SIZE_MAX.
 */
static inline size_t ov_table(OakUi *ui, const char *const *columns, size_t count, size_t rows, uint32_t flags, OakCell cell, void *user) {
    OakStr heads[32];
    if (count > 32) count = 32;
    for (size_t i = 0; i < count; i++) heads[i] = oak_str(columns[i]);
    OakTable t = { .size = sizeof t, .flags = flags, .columns = heads, .count = count, .rows = rows };
    size_t clicked = SIZE_MAX;
    ov_api->oak_table(ui, &t, cell, user, &clicked);
    return clicked;
}

/* -------------------------------------------------------------- choosing */

/* A text field with a label over it: `buf` is what it holds (edits go
 * into it). OAK_CHANGED: it was edited; OAK_CONFIRMED: Enter.
 */
static inline uint32_t ov_input(OakUi *ui, const char *label, const char *hint, char *buf, size_t cap) {
    OakField f = { .size = sizeof f, .label = oak_str(label), .hint = oak_str(hint) };
    OvBuf b = { buf, cap };
    return ov_api->oak_field(ui, &f, oak_str(buf), ov_sink_buf, &b, NULL);
}

/* The same with all of OakField (a button, help, an error, a secret). */
static inline uint32_t ov_field_with(OakUi *ui, const OakField *field, char *buf, size_t cap) {
    OvBuf b = { buf, cap };
    return ov_api->oak_field(ui, field, oak_str(buf), ov_sink_buf, &b, NULL);
}

/* A row with a switch: a click flips `on`. True when it did. */
static inline bool ov_toggle(OakUi *ui, bool *on, const char *label, const char *help) {
    return ov_api->oak_toggle(ui, on, oak_str(label), oak_str(help), 0, NULL) & OAK_CHANGED;
}

/* One of `count` options (`style`: OAK_CHOICE_*). True when it changed. */
static inline bool ov_choice(OakUi *ui, size_t *selected, const char *const *options, size_t count, uint32_t style) {
    OakStr opts[64];
    if (count > 64) count = 64;
    for (size_t i = 0; i < count; i++) opts[i] = oak_str(options[i]);
    OakChoice c = { .size = sizeof c, .style = style, .options = opts, .count = count };
    return ov_api->oak_choice(ui, &c, selected, NULL) & OAK_CHANGED;
}

/* True when it moved. */
static inline bool ov_slider(OakUi *ui, double *value, double min, double max, double step) {
    OakSlider s = { .size = sizeof s, .flags = OAK_SLIDER_VALUE, .min = min, .max = max, .step = step };
    return ov_api->oak_slider(ui, value, &s, NULL) & OAK_CHANGED;
}

/* ------------------------------------------------------------ how it is */

/* A small tag in a tone's color. */
static inline void ov_badge(OakUi *ui, const char *text, uint32_t tone) {
    ov_api->oak_badge(ui, oak_str(text), tone, 0, NULL);
}

/* A line saying how things stand (`flags`: OAK_NOTICE_*). With
 * OAK_NOTICE_DISMISS, true when its X was pressed.
 */
static inline bool ov_notice(OakUi *ui, uint32_t tone, const char *text, uint32_t flags) {
    return ov_api->oak_notice(ui, tone, oak_str(text), flags, NULL) & OAK_CANCELLED;
}

/* A bar `fraction` (0 to 1) full; below 0: moving. */
static inline void ov_progress(OakUi *ui, float fraction, const char *label) {
    ov_api->oak_progress(ui, fraction, oak_str(label), OAK_TONE_PLAIN, 0, NULL);
}

/* A number in a box. */
static inline void ov_stat(OakUi *ui, const char *label, const char *value, const char *detail) {
    ov_api->oak_stat(ui, oak_str(label), oak_str(value), oak_str(detail), OAK_TONE_PLAIN, NULL);
}

/* What an empty page says. True: its button (`action`) was pressed. */
static inline bool ov_empty(OakUi *ui, const char *icon, const char *title, const char *text, const char *action) {
    return ov_api->oak_empty(ui, oak_str(icon), oak_str(title), oak_str(text), oak_str(action), NULL) & OAK_CLICKED;
}

/* ------------------------------------------------------------ any thread */

/* A short message in the window's corner, printf-style. */
static inline void ov_toast(uint32_t tone, const char *format, ...) {
    char b[512];
    va_list args;
    va_start(args, format);
    vsnprintf(b, sizeof b, format, args);
    va_end(args);
    ov_api->toast(tone, oak_str(b));
}

/* Runs `run(user)` on a thread of its own. Ask for a repaint
 * (ov_api->repaint) when what it did changes what is drawn.
 */
static inline bool ov_spawn(const char *name, OakDo run, void *user) {
    return ov_api->spawn(oak_str(name), run, user);
}

/* Prefs: JSON under a key, kept in Oak's folder for this plugin. */

/* The JSON under `key` into `buf`. False: none (or it didn't fit). */
static inline bool ov_pref_json(const char *key, char *buf, size_t cap) {
    OvBuf b = { buf, cap };
    if (cap) buf[0] = 0;
    return ov_api->prefs_get(oak_str(ov_plugin), oak_str(key), ov_sink_buf, &b) && buf[0];
}

/* Keeps `json` under `key` (empty: forgets it). */
static inline void ov_set_pref_json(const char *key, const char *json) {
    ov_api->prefs_set(oak_str(ov_plugin), oak_str(key), oak_str(json));
}

static inline double ov_pref_number(const char *key, double fallback) {
    char b[64];
    char *end;
    if (!ov_pref_json(key, b, sizeof b)) return fallback;
    double v = strtod(b, &end);
    return end == b ? fallback : v;
}

static inline void ov_set_pref_number(const char *key, double value) {
    char b[64];
    snprintf(b, sizeof b, "%.17g", value);
    ov_set_pref_json(key, b);
}

static inline bool ov_pref_bool(const char *key, bool fallback) {
    char b[16];
    if (!ov_pref_json(key, b, sizeof b)) return fallback;
    return strcmp(b, "true") == 0 ? true : strcmp(b, "false") == 0 ? false : fallback;
}

static inline void ov_set_pref_bool(const char *key, bool value) {
    ov_set_pref_json(key, value ? "true" : "false");
}

/* A text kept under `key` into `buf` (a JSON string, unescaped). False:
 * none, and `buf` is left as it was.
 */
static inline bool ov_pref_text(const char *key, char *buf, size_t cap) {
    char json[2048];
    if (!cap || !ov_pref_json(key, json, sizeof json) || json[0] != '"') return false;
    size_t o = 0;
    for (const char *p = json + 1; *p && *p != '"' && o + 4 < cap; p++) {
        if (*p != '\\') {
            buf[o++] = *p;
            continue;
        }
        p++;
        switch (*p) {
        case 'n': buf[o++] = '\n'; break;
        case 't': buf[o++] = '\t'; break;
        case 'r': buf[o++] = '\r'; break;
        case 'b': buf[o++] = '\b'; break;
        case 'f': buf[o++] = '\f'; break;
        case 'u': {
            unsigned c = 0;
            for (int i = 0; i < 4 && p[1]; i++) {
                char h = *++p;
                c = c * 16 + (unsigned)(h >= 'a' ? h - 'a' + 10 : h >= 'A' ? h - 'A' + 10 : h - '0');
            }
            /* (Surrogate pairs aren't joined: they don't come from ov_set_pref_text.) */
            if (c < 0x80) {
                buf[o++] = (char)c;
            } else if (c < 0x800) {
                buf[o++] = (char)(0xC0 | (c >> 6));
                buf[o++] = (char)(0x80 | (c & 0x3F));
            } else {
                buf[o++] = (char)(0xE0 | (c >> 12));
                buf[o++] = (char)(0x80 | ((c >> 6) & 0x3F));
                buf[o++] = (char)(0x80 | (c & 0x3F));
            }
            break;
        }
        case 0: p--; break;
        default: buf[o++] = *p; break;
        }
    }
    buf[o] = 0;
    return true;
}

/* Keeps `text` under `key`, as a JSON string. */
static inline void ov_set_pref_text(const char *key, const char *text) {
    char json[2048];
    size_t o = 0;
    json[o++] = '"';
    for (const unsigned char *p = (const unsigned char *)text; *p && o + 8 < sizeof json; p++) {
        if (*p == '"' || *p == '\\') {
            json[o++] = '\\';
            json[o++] = (char)*p;
        } else if (*p < 0x20) {
            o += (size_t)snprintf(json + o, sizeof json - o, "\\u%04x", *p);
        } else {
            json[o++] = (char)*p;
        }
    }
    json[o++] = '"';
    json[o] = 0;
    ov_set_pref_json(key, json);
}

#ifdef __cplusplus
}
#endif

#endif
