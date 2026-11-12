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
| A tab on Home | `HomeTab` | `OakHomeTab` | A tab beside Conversations and Cloud; the plugin draws the cell. |
| Tabs of its own | `TabKind` | `OakTabKind` | Tabs in the strip under keys the plugin owns: their title and icon, their page, whether they have the message box and what a message sent there does, their corner box. |
| A choice on the New screen | `NewKind` | `OakNewKind` | Beside Local folder and Cloud: its form, and what the first message starts. |
| A cloud backend | `CloudBackend` | `OakCloudBackend` | Another service cloud sessions run through: its accounts and its requests. Oak's cloud engine, list and transcript do the rest. |
| What sessions are given | `SessionKit` | `OakSessionKit` | For Oak's own cloud sessions or a backend's: the server they reach, the setup script and variables of their environment, the first message's block, files to and from the user, moving to another account. |
| A theme | `ThemeDef` | `OakThemeDef` | Nine colors, offered on Settings. |
| A font | `FontDef` | `OakFontDef` | Four faces, offered on Settings; it can download them when first picked. |
| A system prompt | `Role` | `OakRole` | Offered on the New screen (or only given by the plugin's own `NewKind`). |
| A vault | `Vault` | `OakVault` | Somewhere else logins and keys are kept and renewed, for several machines. One at most. |
| Rows on Settings | `SettingsSection` | `OakSettings` | |
| A screen before everything | `Gate` | `OakGate` | Shown instead of the app while it says it is shut (a sign-in). |
| Hooks | `Hooks` | `OakHooks` | At startup, every frame, at the top of Home's Providers, at exit. |

Two examples to copy: `plugins/hello` (Rust: a Home tab, a theme, a Settings row, a system
prompt) and `plugins/hello-c/hello.c` (C, one file: a Home tab with a button and a text field).

## Using plugins

Oak looks for libraries in, in this order:

1. the folders `OAK_PLUGINS` names (separated as `PATH` is);
2. `plugins` in Oak's folder (`~/.config/oak/plugins`, `%APPDATA%\oak\plugins`, or under `OAK_DIR`);
3. `plugins` beside the executable.

- `oak --plugins` lists the folders, each library found and whether it runs, and what was
  registered. It exits with 1 when a library doesn't run.
- Settings → Plugins shows the same: each plugin's name, version and description, or why it
  isn't running.
- `oak --no-plugins`, or `OAK_PLUGINS=off`, starts Oak without any.
- Two libraries with the same plugin id: the first found runs.

Plugins load once, at startup, and are never unloaded: installing or replacing one takes a
restart. Updating Oak doesn't touch them, and they don't need rebuilding for it.

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
        ui.label("Hello");
        if ui.button("New conversation").clicked() {
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

- **`oak_sdk::egui`** has egui's names and shapes (`Ui`, `RichText`, `Button`, `TextEdit`,
  `Frame`, `ScrollArea`, `Grid`, `ComboBox`, `Window`, `Area`, `Painter`, `Response`, ...), so
  code written for egui mostly compiles as it is. It is not egui: each call goes through the
  table to Oak, which draws with its own egui. A page drawn through it paints what the same
  page paints with egui itself (the test `the_sdk_draws_what_egui_does` compares the two shape
  by shape). It covers what Oak's own screens use, not all of egui: no custom fonts, no
  `LayoutJob`, no meshes or custom shaders. What is missing is added to the table's end.
- **`&mut dyn App`** is the window, on the UI thread: the theme in use, opening and closing tabs,
  going Home or to the New screen, the message box's text and attachments, the hint over it, this
  machine's conversations, starting one, drawing a transcript or a cloud list as Oak draws its own.
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

## Writing one in C, or anything else

```sh
cc -shared -fPIC -I sdk/include -o liboak_hello_c.so plugins/hello-c/hello.c
OAK_PLUGINS=. oak
```

`oak_plugin.h` is all there is to include (C11 or C++), and every function and field is
documented in it. How it reads (`hello.c` shows each):

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
- **Threads**: functions that take an `OakUi`, and those named `ui_`, `paint_`, `input_`,
  `app_`, `oak_`, `window`, `texture_`, work on the UI thread, inside a callback Oak made. The
  rest (HTTP, files, `repaint`, `share`, usage, cloud) work from any thread.

## Rules of the road

- **State is the plugin's own**: statics, or fields behind locks (parts are called from several
  threads). Keep files in Oak's folder under names of your own (`dir`).
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
  its parts answer nothing from then on, Settings names it and `crash.log` has the panic. A
  crash in C (or any `abort`) takes Oak down with it, as in any shared process.
- **Never unwind or `longjmp` across the boundary**, in either direction.

## Testing a plugin

The app's tests run plugins headless, in the whole app, without a display:

```sh
cargo build -p oak-hello
OAK_PLUGINS=target/debug cargo test --lib the_example_plugins -- --ignored   # both examples, step by step
OAK_PLUGINS=<folder> cargo test --lib every_plugin_draws -- --ignored        # any plugins
```

The first also compiles `plugins/hello-c/hello.c` (with `CC`, or `cc`). `every_plugin_draws`
loads whatever `OAK_PLUGINS` holds and draws every Home tab in every theme, Settings, every
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
  drawing), `host.rs` (`Host`, `App`, HTTP), `ui.rs`, `widgets.rs`, `text_menu.rs` (Oak's look),
  `theme.rs`, `time.rs`, `chat.rs`, `cloud.rs`, `usage.rs`.
- `src/plugins.rs`: finding and loading libraries. `src/plugins/api.rs`: Oak's side of the
  table, every function of it. `src/plugins/parts.rs`: a plugin's parts as Oak calls them.
  `src/plugin_app.rs`: what the window does for plugins.
- Where parts are used: `home.rs` (tabs, Providers), `tabs.rs`, `composer.rs`, `session_box.rs`,
  `new_screen.rs`, `cloud.rs` (backends and kits), `api/vault.rs`, `theme.rs`, `markdown.rs`
  (fonts), `settings.rs`, `agent/prompt.rs` (roles).
