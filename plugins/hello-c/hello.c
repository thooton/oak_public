/* The example plugin, in C: a tab on Home with a counter and a text field.
 *
 * All it needs is sdk/include: oak_plugin.h, the interface, and
 * oak_view.h, Oak's components made short to call. Build it as a dynamic
 * library and put it where Oak looks for plugins (docs/PLUGINS.md):
 *
 *     cc -shared -fPIC -I ../../sdk/include -o liboak_hello_c.so hello.c
 *     OAK_PLUGINS=. oak
 *
 * A plugin exports one function, oak_plugin, which says who it is. Oak
 * calls its init with a table of functions; everything else goes through
 * that table.
 */
#define OAK_VIEW_MAIN
#include "oak_view.h"

/* A plugin's state is its own. */
static int clicks;
static char name[64] = "world";
/* Kept from one run of Oak to the next (ov_pref_*). */
static bool counting = true;

/* A row: containers take a function that draws what is in them. */
static void count_row(void *user, OakUi *ui) {
    (void)user;
    if (ov_primary(ui, "Count in C")) clicks++;
    ov_caption(ui, ov_fmt("clicked %d times in C", clicks));
}

static void counter(void *user, OakUi *ui) {
    ov_row(ui, count_row, user);
}

static void you(void *user, OakUi *ui) {
    (void)user;
    /* The field hands back its text when it was edited; Enter or Keep
     * confirm it.
     */
    OakField *field = OV_FIELD(.label = OAK_S("Your name"), .hint = OAK_S("Your name"), .button = OAK_S("Keep"), .width = 320);
    if (ov_field_with(ui, field, name, sizeof name) & OAK_CONFIRMED) {
        ov_set_pref_text("name", name);
        ov_toast(OV_GOOD, "Kept %s", name);
    }
    ov_space(ui, 8);
    if (ov_toggle(ui, &counting, "Show the counter", "Kept for the next time Oak starts.")) {
        ov_set_pref_bool("counting", counting);
    }
}

static void page(void *user, OakUi *ui) {
    if (counting) ov_section(ui, "Counter", counter, user);
    ov_section(ui, "You", you, user);
}

/* Draws the tab's cell. */
static void show(void *user, OakUi *ui) {
    ov_page(ui, ov_fmt("Hello, %s, from C", name), "The example plugin, drawn with oak_view.h.", page, user);
}

/* Oak starts the plugin: it keeps the table and registers its parts. */
static bool init(const OakApi *api) {
    /* (The descriptor's id is a C string: its literal ends in a NUL.) */
    ov_init(api, oak_plugin()->id.ptr);
    ov_pref_text("name", name, sizeof name);
    counting = ov_pref_bool("counting", true);
    OakHomeTab tab = {
        .size = sizeof tab,
        .id = OAK_S("hello-c"),
        .label = OAK_S("Hello C"),
        .tip = OAK_S("The example plugin written in C"),
        .order = 110,
        .show = show,
    };
    api->add_home_tab(&tab);
    return true;
}

OAK_EXPORT const OakPlugin *oak_plugin(void) {
    static const OakPlugin plugin = {
        .size = sizeof(OakPlugin),
        .abi = OAK_ABI,
        .api = OAK_API_VERSION,
        .id = { "hello-c", 7 },
        .name = { "Hello C", 7 },
        .version = { "0.2.0", 5 },
        .about = { "The example plugin in C: a Home tab.", 36 },
        .init = init,
    };
    return &plugin;
}
