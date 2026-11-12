# Plugins

Oak loads plugins when it starts: dynamic libraries (`.so` on Linux, `.dll` on Windows) that add
to the window and change how Oak behaves, the way mods are loaded into a game. Oak itself stays
one executable; with no plugins it is the whole app.

A plugin can add:

| Part | Trait (`oak_sdk`) | What it is |
| --- | --- | --- |
| A tab on Home | `HomeTab` | A tab beside Conversations and Cloud; the plugin draws the cell. |
| Tabs of its own | `TabKind` | Tabs in the strip under keys the plugin owns: their title and icon, their page, whether they have the message box and what a message sent there does, their corner box. |
| A choice on the New screen | `NewKind` | Beside Local folder and Cloud: its form, and what the first message starts. |
| A cloud backend | `CloudBackend` | Another service cloud sessions run through: its accounts and its requests. Oak's cloud engine, list and transcript do the rest. |
| What sessions are given | `SessionKit` | For Oak's own cloud sessions or a backend's: the server they reach, the setup script and variables of their environment, the first message's block, files to and from the user, moving to another account. |
| A theme | `ThemeDef` | Colors, offered on Settings. |
| A font | `FontDef` | Four faces, offered on Settings; it can download them when first picked. |
| A system prompt | `Role` | Offered on the New screen (or only given by the plugin's own `NewKind`). |
| A vault | `Vault` | Somewhere else logins and keys are kept and renewed, for several machines. One at most. |
| Rows on Settings | `SettingsSection` | |
| A screen before everything | `Gate` | Shown instead of the app while it says it is shut (a sign-in). |
| Hooks | `Hooks` | At startup, every frame, at the top of Home's Providers, at exit. |

`plugins/hello` is a small plugin that uses four of these; copy it to start one.

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

A plugin has to have been built for the exact Oak it is loaded into (the next section). One that
wasn't is left alone, and Oak says which part differs. Plugins load once, at startup, and are
never unloaded: installing or replacing one takes a restart.

A plugin is code with all of Oak's rights: it sees the logins Oak has and can do anything Oak
can. Install only those you would run as a program.

## Writing one

```toml
# plugins/hello/Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
oak-sdk = { path = "../../sdk" }
oak = { path = "../.." }      # for the dependency graph only, see below
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

What a plugin works with:

- **`oak_sdk::egui`** is egui itself. A part's `show` gets Oak's `egui::Ui` and draws with any
  egui widget; its ids, memory, textures and fonts are the window's.
- **`&mut dyn App`** is the window, on the UI thread: the theme in use, opening and closing tabs,
  going Home or to the New screen, the message box's text and attachments, the hint over it, this
  machine's conversations, starting one, drawing a transcript or a cloud list as Oak draws its own.
- **`oak_sdk::host()`** is Oak's services, from any thread: HTTP with Oak's client
  (`oak_sdk::Request`), Oak's folder, private files, repaints, the usage log, the cloud engine's
  names and accounts, a JSON blackboard plugins tell each other things through (`shared`,
  `share`), and the drawing only Oak can do (markdown, image blocks, the message bar).
- **`oak_sdk::ui`, `widgets`, `theme`, `time`, `text_menu`** are Oak's own helpers: the layout
  constants, corner boxes, list rows, spinners, dialogs, the right-click menu of text fields.

Rules of the road:

- **State is the plugin's own**: statics, or fields behind locks (parts are shared between
  threads). Keep files in Oak's folder under names of your own (`host().dir()`).
- **Never block the UI thread.** `show` runs every frame: do requests on a thread, keep what
  came back, and call `host().repaint()`.
- **Depend on another plugin with `Info::needs`.** It registers after the ones it names, and
  doesn't run without them. Plugins can't call each other: they share values through
  `host().share(name, value)`, and each reads them with `host().shared(name)`.
- **Ids.** A plugin's id is lower case with no spaces. A `HomeTab`'s id is what `App::go_home`
  takes; a `CloudBackend`'s id starts its accounts' ids (`"<id>~..."`), and its Home tab, if it
  has one, has the same id. A `TabKind` owns the keys its `owns` says; pick a prefix nothing
  else uses.
- **A panic stops the plugin, not Oak.** Every call into a plugin is guarded: after one panics,
  Oak calls none of its parts again, names it on Settings and writes the panic to `crash.log`.
  What a plugin did before stays (a thread it started keeps running).
- **Don't unload, don't `dlopen` yourself.** And use the system allocator (the default): memory
  one side allocates, the other frees.

## Why a plugin must match its Oak exactly

A plugin and Oak pass Rust values to each other: `egui::Ui`, trait objects, strings, vectors.
Rust promises nothing about how those are laid out between two separate builds, so there is no
stable interface to build against, only an exact one: the plugin is compiled from the same
source of `oak-sdk`, `egui` and everything under them, with the same compiler, features, profile
and target as the Oak it goes into. Then both sides hold identical definitions.

Each side links its own copy of those crates (the plugin is a `cdylib`, Oak an ordinary
executable; there is no shared `liboak`). The plugin's copy of egui works on the objects Oak's
copy made, which is sound as long as the copies are the same code.

Oak checks before it calls anything. The library's one exported function, `oak_plugin_entry`
(written by `export_plugin!`), returns a `#[repr(C)]` block with a stamp (`sdk/src/abi.rs`):

- the interface version, and a digest of the SDK's source and of the patched crates;
- a digest of the `TypeId`s of the types that cross. A `TypeId` is derived from the crate's
  version, features, dependencies and compiler options, so it differs whenever any of those do;
- the sizes and alignments of the same types;
- what egui's id hasher makes of a fixed value (it depends on the CPU features compiled for);
- whether debug assertions are on (egui's `Style` has a field only then);
- the compiler's version and the target.

Any difference and the plugin isn't started; Settings and `oak --plugins` say which.

What this asks of a build:

1. **The plugin is a member of Oak's workspace** (as `plugins/hello` is), or of a workspace made
   of Oak's source with the plugin added. The second is how plugins kept in another repository
   are built: a build folder that links Oak's `src`, `sdk`, `vendor`, `build.rs`, `assets` and
   `prompts`, has Oak's `Cargo.toml` with the plugins added to `members`, and starts from Oak's
   `Cargo.lock`. The patched crates must sit at the same path from the workspace's root
   (`vendor/`), since a path is part of a crate's identity.
2. **It depends on `oak`** as well as on `oak-sdk`. Nothing of it is called: it makes cargo
   resolve the plugin's dependency graph together with Oak's, so egui is built once, with Oak's
   features, for both. `export_plugin!` names `oak::PLUGIN_PIN` so the dependency can't be
   forgotten. A plugin that asks for an egui feature Oak doesn't have changes the build of both.
3. **The same profile**: a debug plugin for a debug Oak, release for release.
4. **The same compiler and flags** (`RUSTFLAGS`, `target-cpu`).

So plugins are released with the Oak they were built for, and rebuilt whenever Oak is.

## The two patched crates

Two crates under egui keep process-wide state in statics, which two copies would each have
their own of. Oak builds with patched copies (`vendor/`, through `[patch.crates-io]`);
`vendor/README.md` has the details.

- `parking_lot_core`: the table of parked threads. A plugin adopts Oak's when it starts, so a
  lock taken in one copy and released in the other wakes its waiters.
- `epaint`: font face keys came from a counter, so a plugin's first face took the key of Oak's
  first. The key now includes the counter's address.

`scripts/audit-statics.sh` lists the statics and thread-locals of the crates whose values
cross, to check again when egui is updated.

## Testing a plugin

The app's tests can run it headless, in the whole app, without a display:

```sh
cargo build -p oak-hello
OAK_PLUGINS=target/debug cargo test --lib the_example_plugin -- --ignored   # the example, step by step
OAK_PLUGINS=<folder> cargo test --lib every_plugin_draws -- --ignored      # any plugins
```

`every_plugin_draws` loads whatever `OAK_PLUGINS` holds and draws every Home tab in every theme,
Settings, every choice on the New screen, and the tabs `OAK_PLUGIN_TABS` names (keys separated
by commas), failing if a plugin doesn't load or panics. `OAK_PLUGIN_WAIT_MS` gives each view
that long to fetch what it shows; `OAK_PLUGIN_SHOW=1` prints the text each view drew (with
`--nocapture`). Run it alone: plugins load once per process.

## Where it is in the source

- `sdk/`: `oak-sdk`, what plugins are written against. `abi.rs` (the entry, the stamp, the
  panic guard), `plugin.rs` (the parts and the registrar), `host.rs` (`Host`, `App`, HTTP),
  `ui.rs`, `widgets.rs`, `theme.rs`, `time.rs`, `text_menu.rs`, `chat.rs`, `cloud.rs`, `usage.rs`.
- `src/plugins.rs`: finding, checking and loading libraries, and Oak's `Host`.
  `src/plugin_app.rs`: the window as `oak_sdk::App`.
- Where parts are used: `home.rs` (tabs, Providers), `tabs.rs`, `composer.rs`, `session_box.rs`,
  `new_screen.rs`, `cloud.rs` (backends and kits), `api/vault.rs`, `theme.rs`, `markdown.rs`
  (fonts), `settings.rs`, `agent/prompt.rs` (roles).
