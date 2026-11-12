/* The example plugin, in C: a tab on Home with a counter and a text field.
 *
 * All it needs is sdk/include/oak_plugin.h. Build it as a dynamic library
 * and put it where Oak looks for plugins (docs/PLUGINS.md):
 *
 *     cc -shared -fPIC -I ../../sdk/include -o liboak_hello_c.so hello.c
 *     OAK_PLUGINS=. oak
 *
 * A plugin exports one function, oak_plugin, which says who it is. Oak
 * calls its init with a table of functions; everything else goes through
 * that table.
 */
#include "oak_plugin.h"

#include <stdio.h>

static const OakApi *oak;

/* A plugin's state is its own. */
static int clicks;
static char name[64] = "world";

/* A label, in a color of the theme (0: the default). */
static void label(OakUi *ui, const char *text, uint32_t color, float points) {
    OakText t = { .size = sizeof t, .text = oak_str(text), .color = color, .points = points };
    oak->ui_label(ui, &t, NULL);
}

/* The text field hands back its text when it was edited. */
static void name_changed(void *user, const uint8_t *text, size_t len) {
    (void)user;
    if (len >= sizeof name) len = sizeof name - 1;
    memcpy(name, text, len);
    name[len] = 0;
}

/* A row: containers take a function that draws what is in them. */
static void count_row(void *user, OakUi *ui) {
    (void)user;
    OakText text = { .size = sizeof text, .text = OAK_S("Count in C") };
    OakButton button = { .size = sizeof button, .text = &text };
    if (oak->ui_button(ui, &button, NULL) & OAK_CLICKED) clicks++;

    char line[64];
    snprintf(line, sizeof line, "clicked %d times in C", clicks);
    oak->oak_caption(ui, oak_str(line), 0, NULL);
}

/* Draws the tab's cell. */
static void show(void *user, OakUi *ui) {
    (void)user;
    OakTheme theme = { .size = sizeof theme };
    oak->app_theme(&theme);

    oak->ui_add_space(ui, 24);
    char hello[96];
    snprintf(hello, sizeof hello, "Hello, %s, from C", name);
    label(ui, hello, theme.accent, 18);
    oak->ui_add_space(ui, 12);
    oak->ui_row(ui, OAK_ROW, count_row, NULL, NULL);
    oak->ui_add_space(ui, 12);

    OakTextEdit edit = { .size = sizeof edit, .hint = OAK_S("Your name"), .width = 240 };
    oak->ui_text_edit(ui, &edit, oak_str(name), name_changed, NULL, NULL);
}

/* Oak starts the plugin: it keeps the table and registers its parts. */
static bool init(const OakApi *api) {
    oak = api;
    OakHomeTab tab = {
        .size = sizeof tab,
        .id = OAK_S("hello-c"),
        .label = OAK_S("Hello C"),
        .tip = OAK_S("The example plugin written in C"),
        .order = 110,
        .show = show,
    };
    oak->add_home_tab(&tab);
    return true;
}

OAK_EXPORT const OakPlugin *oak_plugin(void) {
    static const OakPlugin plugin = {
        .size = sizeof(OakPlugin),
        .abi = OAK_ABI,
        .api = OAK_API_VERSION,
        .id = { "hello-c", 7 },
        .name = { "Hello C", 7 },
        .version = { "0.1.0", 5 },
        .about = { "The example plugin in C: a Home tab.", 36 },
        .init = init,
    };
    return &plugin;
}
