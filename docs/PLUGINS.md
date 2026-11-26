# Plugins

Oak loads plugins when it starts: dynamic libraries (`.so` on Linux, `.dll` on Windows) that add
to the window and change how Oak behaves. Oak itself stays one executable; with no plugins it is
the whole app.

The interface between Oak and a plugin is plain C: one exported function, a table of function
pointers, and C structs (`sdk/include/oak_plugin.h`). It is stable: a plugin built once keeps
running in every later Oak, and it can be written in any language that can export a C function
and call C function pointers. `oak-sdk` wraps the same interface for Rust.

A plugin can add:

| Part | Rust trait (`oak_sdk`) | C descriptor | What it is |
| --- | --- | --- | --- |
| A tab on Home | `HomeTab` | `OakHomeTab` | A tab beside Conversations and Cloud; the plugin draws the cell. Or a link at the right end of the tabs' row (`link`, `OAK_HOME_TAB_LINK`) that opens one of the plugin's own tabs, or a page of its own (`page`, `OAK_HOME_TAB_PAGE`) drawn in all of the window under the tabs' row, as Settings is. |
| Tabs of its own | `TabKind` | `OakTabKind` | Tabs in the strip under keys the plugin owns: their title and icon, their page, whether they have the message box and what a message sent there does, their corner box. |
| A choice on the New screen | `NewKind` | `OakNewKind` | Beside Local folder and Cloud: its form, and what the first message starts. |
| A cloud backend | `CloudBackend` | `OakCloudBackend` | Another service cloud sessions run through: its accounts and its requests. Oak's cloud engine, list and transcript do the rest. |
| What sessions are given | `SessionKit` | `OakSessionKit` | For Oak's own cloud sessions or a backend's: the server they reach, the setup script and variables of their environment, the first message's block, files to and from the user, moving to another account. |
| A theme | `ThemeDef` | `OakThemeDef` | Nine colors, offered on Settings. |
| A font | `FontDef` | `OakFontDef` | Four faces, offered on Settings; it can download them when first picked. |
| A system prompt | `Role` | `OakRole` | Offered on the New screen (or only given by the plugin's own `NewKind`). |
| A vault | `Vault` | `OakVault` | Somewhere else logins and keys are kept and renewed, for several machines. One at most. |
| Its settings page | `SettingsSection` | `OakSettings` | Rows on a page of the plugin's own, which the gear on its row on Home's Plugins tab opens (and Settings in the row's right-click menu), in the window as Settings is; its X or Esc goes back. Never on Settings itself. |
| A screen before everything | `Gate` | `OakGate` | Shown instead of the app while it says it is shut (a sign-in). |
| Hooks | `Hooks` | `OakHooks` | At startup, every frame, at the top of Home's Providers, in a message box's bottom left corner, on Home's wallpaper (`home_scene`, under the menu; interface version 7), over a local conversation's transcript (`chat_scene`, clipped to it; version 7), as a button inside a message box's top right corner (`composer_button`, a 22-point square, the box's text kept clear of it; version 11), at exit. |
| Rewrites | `Rewriter` | `OakRewriter` | What a conversation's transcript shows in place of the model's own words (interface version 9): its replies' text, its thinking (in the sections the plugin picks) and its notes, rewritten as they come; the model is sent its own text still. Oak keeps each rewrite in its block under the rewriter's `_oak_` key, shows it streaming in ("Drafted a message" opens on the model's own text, unless No drafts) and starts again what was left unfinished; the plugin says what is rewritten and does the rewriting (often with `model_stream`). The user's own conversations only, not subagents'. The first registered. |
| Models for conversations | `ModelProvider` | `OakProvider` | Models Oak's normal conversations can run on, served by the plugin (interface version 10): listed in the pickers under the provider's name with its logo, named `plugin:<id>/<model>`. Each request comes whole (`provider::Request`: the system prompt, the messages in Oak's format, the tools), and the reply streams back as events (`provider::event`: text, thinking, whole tool calls, retries, usage, stop); Oak runs the calls as for any model. Their conversations get the basic tools (a local model's); chats, Code Mode and subagents don't use them. A failure starting `provider::TOO_LONG` has Oak summarize and ask again. One per id: the first. |
| A box beside the message box | `ComposerBox` | `OakComposerBox` | Framed like smart context's and as tall, left of the message box's buttons (version 9): switches for what the plugin does to every conversation. The row makes room for it only while there is one, and drops it before smart context when the window is narrow. The first registered. |

Two examples to copy: `plugins/hello` (Rust: a Home tab built from Oak's components, with state
kept between runs and work on a thread; a theme, a settings page, a system prompt) and
`plugins/hello-c/hello.c` (C, one file: the same kind of Home tab, through `oak_view.h`).

## Using plugins

Oak looks for libraries in, in this order:

1. the folders `OAK_PLUGINS` names (separated as `PATH` is);
2. `plugins` in Oak's folder (`~/.config/oak/plugins`, `%APPDATA%\oak\plugins`, or under `OAK_DIR`);
3. `plugins` beside the executable.

- `oak --plugins` lists the folders, each library found and whether it runs, and what was
  registered. It exits with 1 when a library doesn't run.
- Home → Plugins shows the same: each plugin's name, version and description, or why it isn't
  running, with a switch to turn it off or on. Switches are kept in `plugins.json` in Oak's folder
  and apply at the next start or Reload. A switched-off plugin's library is opened to read
  who it is, but its `init` never runs; switching one on also switches on what it `needs`.
- `oak --no-plugins`, or `OAK_PLUGINS=off`, starts Oak without any.
- Two libraries with the same plugin id: the first found runs.

Plugins load at startup, and Home → Plugins → Reload loads them again without a restart: libraries
added or rebuilt start, those removed or switched off stop, and the rest keep running untouched.
Oak loads each library from a copy (in `plugin-copies` in Oak's folder), so the file in the plugins
folder can be rebuilt or replaced while Oak runs: build, then Reload. A plugin that stops at a
reload gets its `exiting` hook and its parts leave the window, but its library is never unloaded:
what it left running (threads, servers) goes on unless `exiting` ends it, so a plugin meant to be
reloaded should stop its threads there. One started again is a fresh instance (its statics start
over), and the plugins that need it aren't started again with it. Updating Oak doesn't touch
plugins, and they don't need rebuilding for it.

A plugin is code with all of Oak's rights: it sees the logins Oak has and can do anything Oak
can. Install only those you would run as a program.

## The interface, and why it stays stable

- A plugin exports `const OakPlugin *oak_plugin(void)`: who it is (id, name, version, what it
  needs) and its `init`. Oak calls `init` with the `OakApi`, a table of functions, which stays
  valid for good. The plugin registers its parts through it and keeps it for everything else.
- **The table only grows at its end.** `OAK_API_VERSION` counts up when functions are added. A
  plugin says which version it was built against (`OakPlugin.api`); every Oak with that version
  or a later one runs it. An Oak older than the plugin says so and leaves it alone.
- **Only C data crosses**: integers, floats, `OakStr` (UTF-8 with a length, borrowed for the
  call), `OakRect`, colors as `0xRRGGBBAA`, ids as `uint64_t`, opaque handles, and callbacks
  with a `user` pointer. Text and bytes Oak hands back go to an `OakSink` the caller gives.
  Anything structured (a list of accounts, a login, a request) is JSON text.
- **Structs of options start with their own size** and are all zeroes by default, so fields can
  be added at their end: each side reads only as much of a struct as the other says it has, and
  what is missing is zero. Set `.size = sizeof(...)`; a struct whose size is 0 reads as all
  defaults. `OakStr`, `OakBytes`, `OakRect` and `OakResponse` have no size and never change.
- Nothing of a compiler, of Rust or of egui is part of it. Oak draws everything; a plugin says
  what to draw.

`sdk/src/abi.rs` is the single source: the Rust types, the table, Oak's side of it and
`sdk/include/oak_plugin.h` all come from its declarations. `sdk/abi/<version>.txt` records each
version, and a test (`no_version_breaks_an_earlier_one`) fails on any change that would break a
plugin built for one: a function moved, removed or changed, a field anywhere but at a struct's
end, a constant or a callback changed. To add to the interface: add functions at the table's
end and fields at structs' ends, count `OAK_API_VERSION` up, and run
`OAK_WRITE_HEADER=1 OAK_WRITE_ABI=1 cargo test -p oak-sdk`.

## Writing one in Rust

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
oak-sdk = { path = "../oak_public/sdk" }    # or a git dependency on this repository
```

```rust
use oak_sdk::{App, HomeTab, Info, Plugin, Registrar};
use oak_sdk::egui::Ui;
use oak_sdk::view::{Action, Page};

struct Hello;

impl Plugin for Hello {
    fn info(&self) -> Info {
        Info::new("hello", "Hello", env!("CARGO_PKG_VERSION"), "What it does, in a line.")
    }
    fn register(&self, r: &mut Registrar) {
        r.home_tab(Tab);
    }
}

oak_sdk::export_plugin!(Hello);

struct Tab;

impl HomeTab for Tab {
    fn id(&self) -> String { "hello".into() }
    fn label(&self) -> String { "Hello".into() }
    fn show(&self, ui: &mut Ui, app: &mut dyn App) {
        let mut go = false;
        Page::new("Hello").subtitle("What this tab is for.").show(ui, |ui| {
            go = Action::primary("New conversation").show(ui).clicked();
        });
        if go {
            app.go_new("local");
        }
    }
}
```

```sh
cargo build -p oak-hello
OAK_PLUGINS=target/debug cargo run          # or copy target/debug/liboak_hello.so into a plugins folder
```

The plugin is an ordinary crate in any workspace, built with any compiler version and profile.
It links only `oak-sdk` (and through it `serde_json`, `chrono` and egui's two small math and
color crates); the library exports one symbol.

What a plugin works with:

- **`oak_sdk::view`** is Oak's components (interface version 8), what most pages are made of,
  drawn by Oak in its look and the theme in use: `Page` (a centered column under a title, with a
  back link and buttons at its right), `Heading`, `Section` (in a card, folding), `Card`,
  `Action` (a button: `new`, `primary`, `quiet`, `danger`, `link`, `pill`, `icon`; busy,
  disabled, selected), `Badge`, `Item` (a list row: a dot, a picture or a symbol, title,
  subtitle, faint text and buttons at its right), `Field` (label, hint, help or error, a
  button; `confirmed()` on Enter), `Toggle` (a row with a switch), `setting` (label and help at
  the left, a control at the right), `Choice` (joined boxes, tabs, chips, a drop-down, a list),
  `Slider`, `Progress`, `Notice` (how things stand, in a `Tone`), `Empty`, `Stat`, `Table` (only
  the rows in view are drawn), `columns`, and `toast` (a message in the window's corner, from
  any thread). Each is a builder drawn with `show`, and each takes a `Look` (below). `Heading`
  and `Section` take a `right` body (at the heading's right end), `Item` a `left` and a `right`
  one (an avatar, a checkbox; buttons, a switch). Beside them, what a plugin keeps: `State<T>`
  (a static, made with `Default` when first used), `Stored<T>` (serde, kept in
  `plugin-prefs.json` in Oak's folder under the plugin's id, written soon after a change), and
  `Job<T>` (work on a thread of its own: `start`, `busy`, `take` its result; a panic in it is an
  error, and Oak repaints when it ends).
- **`oak_sdk::egui`** has egui's names and shapes (`Ui`, `RichText`, `Button`, `TextEdit`,
  `Frame`, `ScrollArea`, `Grid`, `ComboBox`, `Window`, `Area`, `Painter`, `Response`, ...), so
  code written for egui mostly compiles as it is. It is not egui: each call goes through the
  table to Oak, which draws with its own egui. A page drawn through it paints what the same
  page paints with egui itself (the test `the_sdk_draws_what_egui_does` compares the two shape
  by shape). It covers what Oak's own screens use, not all of egui: no custom fonts, no
  `LayoutJob`, no meshes or custom shaders. What is missing is added to the table's end.
- **`&mut dyn App`** is the window, on the UI thread: the theme in use, opening and closing tabs,
  going Home, to the New screen, to Settings or to a plugin's page, the message box's text and attachments, files pasted or dropped on the window and the system's file picker, the hint over it, this
  machine's conversations, `insert_input` (text put at the message box's cursor, as if typed there, spaced from its neighbours if asked; version 11), starting one, drawing a transcript or a cloud list as Oak draws its own,
  and what Oak is doing now (`activity`, version 7: the page, Home's menu and corner buttons, the
  window maximized or focused, each conversation's phase such as thinking, writing or a tool),
  and during `chat_scene` what the transcript shows (`chat_scene`, version 7: the view, the boxes
  over it, the body text's size, its units on screen and where each starts, and its rows of text,
  so something drawn there can stand on a line and scroll with it).
  A plugin can also work a conversation it is shown: `session(key)` says what it is, what it is
  doing and how its last turn went (its status, in which Oak says when a safety classifier
  declined a reply), `set_model` switches it without the picker's question, `send` writes to it as
  the message box does, `interrupt` stops its turn and `resume` has it answer its last message
  again. A conversation started from `NewConversation` may name a folder, model, provider, effort
  and chat of its own, so a plugin can carry work on where another conversation was, on the model
  it ran.
- **`App::private_browser`** (interface version 13) draws Oak's browser inside a Home plugin
  page. The user controls it directly. The plugin supplies an HTTPS entry URL, but gets no
  page content, input, or cookies. The browser uses a temporary profile and closes when the
  user leaves the plugin page.
- **`oak_sdk::host()`** is Oak's services, from any thread: HTTP with Oak's client
  (`oak_sdk::Request`), Oak's folder, private files, repaints, the usage log, the cloud engine's
  names and accounts, a JSON blackboard plugins tell each other things through (`shared`,
  `share`).
- **`oak_sdk::ui`, `widgets`, `text_menu`** are Oak's look: markdown, the message bar, corner
  boxes, list rows, spinners, switches, dialogs, drop-down heads, secret fields, the right-click
  menu of text fields. **`theme`, `time`, `chat`, `cloud`, `usage`** are plain data and helpers.

Ids: `egui::Id` values are the same numbers on both sides, so a widget's id from a `Response`
can be given to `Memory::request_focus`. A plugin's own per-id state (`ui.data_mut`) is kept in
the plugin.

### Adding an account from a plugin

With interface version 12, call `app.start_sign_in(provider)` in response to a user action.
Use `claude`, `codex`, `antigravity`, `freebuff` or `quick`. On success the call returns the
same browser authorization URL Oak would copy from **Home → + Add account**. The plugin
chooses how to present it, for example `ui.hyperlink_to("Continue in browser", &url)`.
Starting a sign-in does not open a browser or change the clipboard.

For Claude, Codex and Antigravity, draw `app.sign_in_form(ui)` each frame in the plugin, or
let the user complete the same form on Home. Claude uses a pasted authorization code. Codex
and Antigravity listen for the browser callback and let the user paste the callback URL if
needed. Freebuff and Quick finish in the background and don't use the form. Oak handles
credentials and local or vault storage just as it would for a sign-in from Home.

In C the same calls are `api->app_start_sign_in` (the output sink gets the URL on success or
an error on failure) and `api->app_sign_in_form`.

## Writing one in C, or anything else

```sh
cc -shared -fPIC -I sdk/include -o liboak_hello_c.so plugins/hello-c/hello.c
OAK_PLUGINS=. oak
```

`oak_plugin.h` is all there is to include (C11 or C++), and every function and field is
documented in it. `oak_view.h`, beside it, is optional: Oak's components made short to call
(`ov_page`, `ov_section`, `ov_primary`, `ov_item`, `ov_input`, `ov_toggle`, `ov_choice`,
`ov_notice`, `ov_table`, `ov_toast`, ...), `ov_fmt` for a label made with printf, a text field
over a `char` buffer, and prefs (`ov_pref_text`, `ov_set_pref_bool`, ...). It is all `static
inline`; one file defines `OAK_VIEW_MAIN` before including it, and `init` calls
`ov_init(api, "<plugin id>")`. `OV_ITEM(...)` and the like make a component's full struct with
its size set. Looks: `ov_next_look(OV_LOOK(.fill = OV_RGB(0x2b5cd6), .radius = 6))` before a
component, or `ov_look_scope(ui, OV_LOOK(...), body, user)`; `OV_ZERO` is a length that is a
true zero. How the interface reads (`hello.c` shows each):

- **Text** is an `OakStr`: `oak_str(s)` for a C string, `OAK_S("literal")` for a literal. Oak
  copies what it keeps; a string only has to live for the call. (Callbacks and their `user`
  pointers, once registered, are kept for good.)
- **Options** are structs: zero them, set `.size`, set what you want.
  `OakText t = { .size = sizeof t, .text = OAK_S("Hi"), .color = theme.accent };`
- **Drawing is immediate**: every frame Oak calls the part's `show` with an `OakUi *`, and the
  plugin calls `ui_label`, `ui_button`, `ui_text_edit`, `paint_rect`, ... on it. An `OakUi` is
  valid only inside the callback it came with.
- **Containers take a callback**: `ui_row(ui, OAK_ROW, body, user, out)` calls `body(user, inner_ui)`
  to draw what is in the row. The same for scroll areas, frames, grids, menus and windows.
- **Widgets answer** with flags (`OAK_CLICKED`, `OAK_HOVERED`, `OAK_CHANGED`, ...) as their
  return value, and fill an `OakResponse` (its rectangle, its id, the pointer) when given one;
  `out` may be `NULL`. `ui_response` adds a tooltip, a context menu or a cursor to one.
- **Text fields**: the text is the plugin's. Give the current text; when it was edited, the
  `changed` sink gets the new one.
- **Colors** are `0xRRGGBBAA`; 0 means the default (or none). `app_theme` gives the colors of
  the theme in use.
- **Parts** are registered in `init` with a descriptor: its texts, its `user` pointer, and its
  callbacks. Besides the drawing callbacks, a part has one `call(user, method, args, data, out,
  out_user)` for everything else, by name, with JSON arguments ("look", "send", "accounts",
  "token", ...; each descriptor's comment lists its methods). Return `OAK_CALL_UNKNOWN` for a
  method you don't have, and Oak does what it does without it.
- **Components** (version 8) are the table's functions from `oak_page` to `oak_tone_color`.
  `oak_page`, `oak_section`, `oak_card` and `oak_setting` take a body; `oak_table` and
  `oak_columns` take an `OakCell` callback, called for each cell. Their options are structs as
  above (`OakPage`, `OakItem`, `OakField`, ...). What they say by color is a tone (`OAK_TONE_*`).
  Their answers add `OAK_CONFIRMED` (a field's Enter or button), `OAK_CANCELLED` (a page's back
  link, a notice's X) and `OAK_OPEN` (a section drew its body) to the usual flags.
  `oak_look_next` and `oak_look_scope` take an `OakLook` (below).
- **Threads**: functions that take an `OakUi`, and those named `ui_`, `paint_`, `input_`,
  `app_`, `oak_`, `window`, `texture_`, work on the UI thread, inside a callback Oak made. The
  rest (HTTP, files, `repaint`, `share`, usage, cloud, `toast`, `spawn`, `prefs_get`,
  `prefs_set`) work from any thread.
- **Models** (version 9): `model_stream` asks a model for a reply through one of Oak's providers,
  with the user's logins and keys, and waits for it, handing over each event (`{"text"}`,
  `{"thinking"}`, `{"cost"}`, `{"retry"}`) as it streams in. It names its usage log (`usage`,
  "plugin" when not given), so its cost shows on the Usage page apart. Not on the UI thread.

## Changing how components look

Oak's look is the default, never the limit. An `OakLook` (`view::Look` in Rust, `OV_LOOK` in C)
changes it, part by part; what it leaves unset stays as Oak draws it:

- **Colors** (0xRRGGBBAA; 0 keeps Oak's): `fill` (its background), `fill_hover` (under the
  pointer), `stroke` (its outline or line), `ink` (its text), `ink_weak` (its fainter text),
  `accent` (what is chosen or matters most: primary buttons, switches, bars, selections,
  links), and the tones' `good`, `warn`, `bad`.
- **Lengths** (points; 0 keeps Oak's, below 0 is a true zero): `radius`, `stroke_width`,
  `padding_x`, `padding_y`, `height`, `width`, `text_points` (its main text's size; its other
  texts follow), `gap` (between its parts, or after it), and `text_scale` (a factor).
- **Flags**: `OAK_LOOK_MONOSPACE` (the code font), `OAK_LOOK_NO_FRAME` (no background or
  outline).

Given to one component (`.look(..)` on any builder; `oak_look_next` before the call; it doesn't
pass to what is drawn in it), all of it applies. Given to a scope (`look_scope(ui, &look, |ui|
...)`; `oak_look_scope`), its colors, `radius`, `stroke_width`, `text_scale` and the code font
apply to every component drawn in it, and egui's own widgets there take its colors; scopes in
scopes add to it (scales multiply), and a component's own look goes over its scopes'.
`tone_color` (`oak_tone_color`) answers in the scope's colors.

What a look's sizes mean to each:

| Component | `width` | `height` | `padding` | `text_points` | `gap` | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Page | the column | | `_y`: above | the title | under the header | |
| Heading | | its row | | its text | | `stroke`: its rule |
| Section | its card | its heading's row | its card's | its heading | after it | frame parts go to its card |
| Card | its width | | inside | | | |
| Action | exact | its height | `_x`: its sides | its text | | own colors beat its kind's |
| Badge | | its height | `_x` | its text | | `stroke`: an outline |
| Item | | its height | `_x` | its title | | `stroke`: its line; `no_frame`: none |
| Field | the field | its least | inside the field | its text | | `ink_weak`: label and help |
| Toggle | | its row | `_y` | its label | | `fill_hover`: the row lit |
| setting | its left side | | | its label | after it | |
| Choice | a drop-down's | its height | `_x` | its text | between chips | pills take a button's look |
| Progress | the bar | the bar's | | its label | | `fill`: the track |
| Notice | | | its box's | its text | | `stroke`: its bar; `stroke_width`: the bar's |
| Empty | its text's widest | | `_y`: around it | its title | | |
| Stat | its box | | its box's | the number | | `ink`: the number |
| Slider | its track | its row | | its value | | `fill`: the track; `ink`: the knob |
| Table | | a row's | `_x`: in a cell | the headings | | `fill`: the headings' row; `no_frame`: no lines |
| columns | | | | | between them | |

## Rules of the road

- **State is the plugin's own**: statics, or fields behind locks (parts are called from several
  threads). Keep files in Oak's folder under names of your own (`dir`), or small settings in
  its prefs (`prefs_set`, `view::Stored`), which are kept per plugin id.
- **Never block the UI thread.** `show` runs every frame: do requests on a thread, keep what
  came back, and call `repaint`.
- **Depend on another plugin with `needs`.** It starts after the ones it names, and doesn't run
  without them. Plugins can't call each other: they share JSON values through `share(name,
  value)`, and each reads them with `shared(name)`.
- **Ids.** A plugin's id is lower case with no spaces. A Home tab's id is what `app_go_home`
  takes; a cloud backend's id starts its accounts' ids (`"<id>~..."`), and its Home tab, if it
  has one, has the same id. A tab kind owns the keys its `owns` says; pick a prefix nothing
  else uses.
- **Failures.** In Rust, a panic stops the plugin, not Oak: the SDK catches it at the boundary,
  its parts answer nothing from then on, Home's Plugins tab names it and `crash.log` has the panic. A
  crash in C (or any `abort`) takes Oak down with it, as in any shared process.
- **Never unwind or `longjmp` across the boundary**, in either direction.

## Testing a plugin

The app's tests run plugins headless, in the whole app, without a display:

```sh
cargo build -p oak-hello
OAK_PLUGINS=target/debug cargo test --lib the_example_plugins -- --ignored   # both examples, step by step
OAK_PLUGINS=<folder> cargo test --lib every_plugin_draws -- --ignored        # any plugins
cargo test --lib reloading_picks_up -- --ignored                             # Reload, with C variants
```

The first also compiles `plugins/hello-c/hello.c` (with `OAK_TEST_CC`, else `CC`, else `gcc` on Windows and `cc` elsewhere). `every_plugin_draws`
loads whatever `OAK_PLUGINS` holds and draws every Home tab and plugin page in every theme, Settings, each plugin's settings page, Home's Plugins tab, every
choice on the New screen, and the tabs `OAK_PLUGIN_TABS` names (keys separated by commas),
failing if a plugin doesn't load or stops. It runs in a scratch Oak folder, never the user's:
`OAK_PLUGIN_SEED` names a folder whose files are copied there first (a saved connection to a
test server, say). `OAK_PLUGIN_WAIT_MS` gives each view that long to fetch what it shows;
`OAK_PLUGIN_SHOW=1` prints the text each view drew (with `--nocapture`). Run each alone:
plugins load once per process.

A Rust plugin can also be tested in Oak's own test process, without building a library:
`oak_sdk::rt::attach(&TABLE)` points the SDK at Oak's table (`src/plugins/api.rs` has such tests).

## Where it is in the source

- `sdk/src/abi.rs`: the interface (types, callbacks, constants, the table) and the header's
  generator. `sdk/include/oak_plugin.h`: the header. `sdk/abi/`: each version's record.
- The rest of `sdk/`: the Rust wrapper. `rt.rs` (the entry, the panic guard), `plugin.rs` (the
  parts' traits, the registrar and the callbacks behind them), `egui.rs` (the egui-shaped
  drawing), `view.rs` (the components, `State`, `Stored`, `Job`), `host.rs` (`Host`, `App`,
  HTTP), `ui.rs`, `widgets.rs`, `text_menu.rs` (Oak's look), `theme.rs`, `time.rs`, `chat.rs`,
  `cloud.rs`, `usage.rs`. `sdk/include/oak_view.h`: the components for C.
- `src/view.rs`: the components themselves (Oak's own pages can use them too), and the toasts,
  drawn over everything each frame from `src/lib.rs`. `src/plugin_prefs.rs`: the prefs and
  `plugin-prefs.json`.
- `src/plugins.rs`: finding, loading and reloading libraries. `src/plugins/api.rs`: Oak's side of the
  table, every function of it. `src/plugins/parts.rs`: a plugin's parts as Oak calls them.
  `src/plugin_app.rs`: what the window does for plugins.
- `src/plugins_view.rs`: Home's Plugins tab, the manager (and the gear to each plugin's settings page).
- Where parts are used: `home.rs` (tabs, Providers), `tabs.rs`, `composer.rs`, `session_box.rs`,
  `new_screen.rs`, `cloud.rs` (backends and kits), `api/vault.rs`, `theme.rs`, `markdown.rs`
  (fonts), `settings.rs` (Settings, and each plugin's settings page), `agent/prompt.rs` (roles).
