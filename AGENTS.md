# Oak

A coding agent for Windows and Linux, written in Rust with [egui](https://github.com/emilk/egui).
Each conversation works in a folder you choose, with a model from one of Oak's providers: Claude
(a subscription login or an API key), the GPT models a ChatGPT plan includes for Codex,
Google Antigravity (`src/antigravity/`: Gemini, Claude and GPT-OSS models over Google OAuth and Cloud Code Assist),
OpenRouter, Muse Code (Meta's Muse models through a Meta Model API key, `src/musecode.rs`), NodeClub,
Freebuff, Refuseless (abliterated GLM 5.3 Flash and DeepSeek V4.1 Flash), AWS Quick (`src/quick.rs`: Amazon
Quick's own agent, over its OIDC sign-in and chat WebSocket), a llama.cpp server of
your own, or models a plugin serves (Duck.ai's, through the private duck plugin). Cloud sessions run Claude Code in Anthropic's cloud on your Claude accounts.

Oak is one executable. It loads plugins when it starts: dynamic libraries that add tabs, cloud
backends, themes, fonts, system prompts and more, through a stable C interface
([docs/PLUGINS.md](docs/PLUGINS.md)).

The repository:

- `src/`: the app. Every file opens with a comment saying what it holds; `lib.rs` has the app's
  state, its screens and the frame loop (`main.rs` only calls it).
- `sdk/`: the plugin interface (`sdk/src/abi.rs`, and the C header written from it,
  `sdk/include/oak_plugin.h`) and `oak-sdk`, its Rust wrapper; `sdk/src/view.rs` and
  `sdk/include/oak_view.h` are its components (pages, sections, buttons, rows, fields, toasts)
  and plugin state, which Oak draws from `src/view.rs`. Every component reads its look there
  (`view::style`: the scopes' and its own `Look` over the theme); a new component, or a new
  part of one, takes its colors and sizes from that `Style` and says which in its doc comment
  and in docs/PLUGINS.md's table. `plugins/hello` and
  `plugins/hello-c`: example plugins in Rust and in C.
- `protocols/`: Oak's own protocols (Browser, Window, Blender), compiled in (see Protocols).
- `prompts/`: the system prompt, compiled in. `assets/`: fonts, logos and pictures (with the
  scripts that make some of them), Blender's add-on (`blender/`) and the browser extension
  (`brave-extension/`). `linux/`: the self-contained Linux bundle. `scripts/`: checks.

This file is the description of how Oak behaves that QA checks the app against: keep it true
with every change, and put here the decisions the code alone doesn't explain. How a part works
belongs in its file's opening comment.

## Keep changes in sync with the remote

Unless the user explicitly asks to keep changes local, verify your changes, commit just the work for the task, and push it to the branch's upstream before reporting completion. The default branch here is `origin/master`. If the remote has advanced, rebase your commits on it, resolve any conflicts, and rerun affected checks before pushing. Never force-push or include unrelated edits. Confirm the push succeeded; if blocked, report what remains local and why.

## Build, run and test

You need a recent stable Rust (edition 2024). Oak runs on Windows 10/11 with
[Git for Windows](https://gitforwindows.org/) (commands run in Git Bash) and on Linux with X11 or
Wayland (commands run in `bash`). Optional, for the agent's tools: Chrome, Chromium, Brave or Edge
(FETCH `render`, SCREENSHOT, the BROWSER kit), `curl` on the PATH (SEARCH for images without
a key; Windows and Git for Windows bring one), and on Linux Xvfb, xdotool and ImageMagick
(testing GUI apps off-screen).

```sh
cargo run --release
cargo build && cargo test && cargo clippy --all-targets
```

The build downloads the latest [ripgrep](https://github.com/BurntSushi/ripgrep/releases) release
for the target with curl (checked against its `.sha256`) and bundles it into the binary (`build.rs`):
each version is kept in the build folder, so rebuilds download nothing, and a newer release is
picked up when the build script runs again (with each commit). Offline, it bundles the newest one
kept; `OAK_RIPGREP=<path to rg>` bundles that executable instead. With neither, a release build
(`OAK_RELEASE=1`) fails and any other is built without ripgrep, with a warning.

`oak --version` prints the version, `oak --plugins` the plugins found and whether each runs,
`oak --no-plugins` starts without any, `oak --mcp blender` serves Oak's Blender MCP server over
stdio (for Claude Code, Codex or any MCP client) and exits when stdin closes.

Ignored tests, run with `cargo test [--release] <name> -- --ignored --nocapture`:

- Benchmarks, at no cost: `bench_frames`, `bench_between_turns` (frames idle, scrolling and with
  a reply streaming in, startup, picture memory; `BENCH_TURNS` sets the long conversation's length),
  `bench_streaming_reply` (a 40 kB reply streaming in at the end of a long conversation),
  `bench_transcript_steps`, `bench_request_messages`, and `bench_tool_turnaround` (what a
  conversation adds around EXECUTE, READ and WRITE calls, against the fake model of
  `bench/tools/` in the private repository, beside Claude Code's and the command center's; set
  `OAK_API_BASE` and a scratch `OAK_DIR`). Build them with `--release`.
- Against the real services, signed in (a small cost): `live_rolling`, `live_images`,
  `live_agents`, `live_openrouter` (`OAK_DIR` a scratch folder with a copy of openrouter.json;
  `OAK_TEST_MODELS`, `OAK_TEST_RUNS`, `OAK_TEST_DUMP` for the request bodies), `live_freebuff` (`OAK_TEST_PROMPT`, `OAK_TEST_FOLDER`) and `live_cloud_list`
  (read only), `live_rewrite`, `live_search` and `live_images_search` (`OAK_TEST_QUERY`). `live_hidden_desktop` (Windows, after `cargo build`)
  opens Oak on a hidden desktop. `quick_live_turn` drives one real AWS Quick turn end to end (a signed-in
  `OAK_DIR` with `quick.json`; `OAK_QUICK_PROMPT` sets the prompt), printing what Quick says. `closes_tk_apps_and_cmd_wrappers` (Windows, after `cargo build`,
  Python with Tk required; `OAK_TEST_PYTHON` overrides its path) checks literal argument passing,
  quoted `cmd.exe /c` assignments, normal close and the five-second forced-close fallback; set
  `OAK_WINDOW_TEST_DIR` for its fixtures and logs.
- Whole-app tests that open a window: `whole_app_close_with_a_task`, `whole_app_tab_shortcuts`,
  `whole_app_preview_expands_and_fades` (starts a real browser). `upgrade_real_conversations` loads a copy of real
  conversations (`OAK_UPGRADE_FROM`).
- Plugins, headless, each alone (plugins load once per process):
  `the_example_plugins_draw_in_the_window` (after `cargo build -p oak-hello`, with
  `OAK_PLUGINS=target/debug`; it compiles the C example itself), `reloading_picks_up_what_changed`
  (builds variants of the C example and reloads them) and `every_plugin_draws`
  (whatever `OAK_PLUGINS` holds); docs/PLUGINS.md has them. The interface's own tests are in
  `sdk/src/abi.rs` (the header and the record of each version are current, and no version
  breaks an earlier one) and `src/plugins/api.rs` (the SDK against Oak's table, in process).
  `handoff_in_a_whole_app` runs the private repository's classifier-avoidance plugin against
  `bench/tools/fake_refusal.py` and `fake_deepseek.py` there (both ports are its arguments):
  a refusal is handed to DeepSeek for a note, and a new conversation on the refused model
  picks the work up from it. It needs `REFUSELESS_API_KEY`, and `OAK_API_BASE`,
  `REFUSELESS_BASE_URL` and `OAK_PLUGINS` (the plugins' build folder).

To use the app on a virtual display (Linux), with a scratch config folder:

```sh
sudo apt-get install -y pkg-config build-essential xvfb xdotool imagemagick libgl1 libegl1 \
  libgl1-mesa-dri libxkbcommon-x11-0 libxcursor1 libxi6 libxrandr2
export OAK_DIR=$(mktemp -d)
xvfb-run -a -s "-screen 0 1280x800x24" ./target/debug/oak &
```

Drive it with `xdotool` and look with `import -window root shot.png`, inside xvfb-run's
`DISPLAY` and `XAUTHORITY` (or start `Xvfb :99` yourself and export `DISPLAY=:99`). `OAK_API_BASE`
(`src/bench.rs`) points the Claude API at a fake or a recording proxy.

## Releases and updates

- `.github/workflows/client.yml` builds and tests the app on Windows and on Linux (Ubuntu 22.04,
  so glibc 2.35 is enough) for every push that touches it, the example plugin included. A passing
  push to master (the default branch) is released as `v<version>`, with `oak-windows-x86_64.exe`,
  `oak-linux-x86_64` and `oak-linux-x86_64.tar.gz` (each with a `.sha256`); the 20 newest releases are kept.
  Its "Time startup" step starts the release build on both systems with a fresh profile and with
  300 conversations (~200 MB), on Windows with Defender's real-time scanning on and off, and puts each
  `startup.log` in the run's summary (informational: it never fails the build).
- The version is MAJOR.MINOR from `Cargo.toml` and the number of commits as the patch
  (`build.rs`), so every commit on master is a version. Other builds are marked `-dev`;
  `OAK_BUILD_VERSION` sets any version (to try the updater). `oak --version` prints it.
- Oak updates itself (`src/update.rs`) from this repository's releases, github.com/thooton/oak_public,
  whatever built it (`build.rs`; `OAK_UPDATE_REPO` at build time names another repository, or
  `none`). It looks for a release at start and every ten minutes, and the refresh button by the
  version on Home looks at once; its tooltip names the repository. It asks GitHub anonymously, and
  only if GitHub turns that down (a private repository, a rate limit) again with the GitHub login
  the machine already has (`OAK_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token`, then
  git's saved login). "Update available" downloads the build, checks its SHA-256 and swaps the
  executable; "Restart to update" restarts into it (pausing conversations at work to resume
  after). `OAK_NO_UPDATE=1` turns it off. `OAK_UPDATE_API` points it at a stand-in for GitHub's API
  (to try it against a fake or a relay; no login is sent there).
- An update replaces Oak alone. Plugins aren't part of it and don't need rebuilding for it: the
  plugin interface only grows.
- `linux/package.sh` makes `target/dist/oak-linux-x86_64.tar.gz`: unpack it anywhere and run
  `./oak`, or `./install.sh` to add it to the launcher and `~/.local/bin` (`--uninstall` undoes
  it). It links only glibc and libgcc_s; fonts, prompts and TLS are built in, and OpenGL, X11 or
  Wayland and libxkbcommon come from the system.

## Where Oak keeps its data

`%APPDATA%\oak` on Windows, `~/.config/oak` (`$XDG_CONFIG_HOME/oak`) on Linux, or `OAK_DIR` (to try
a build against a copy of your conversations). Each conversation is a file in `conversations/`
named after its folder and title, with its id last. Pictures in conversations are files of their
own in `images/`, named after the SHA-256 of their bytes (`src/blobs.rs`): an image block names its
file (`blob`) and size (`_oak_size`) instead of holding base64 `data`, which goes back in only as a
request is sent (`api::stream`). Pictures still saved as data are moved out at the next start
(only data that reads back byte for byte, so requests send what they did); files are never
deleted. At start only each conversation's metadata is read, and where each message is in its
file, while the window is set up (`preload` in `src/lib.rs`): from `conversations/index.bin` for
the files unchanged since the last start (by size and time; a cache, rewritten after a start that
read any file), else from the file. A conversation's messages are read when first looked at, and then only the last ones
(`store::Messages`); earlier ones as the view scrolls near them (`chat_view::read_back`) or a
request sends them (from the summary's cut on). Messages on disk as they are are let go again
when nothing needs them (`agent::forget_messages`: all of a conversation not shown for ten
minutes, else those well above its view; never while it works). A save writes only the
messages that changed and copies the rest from the old file. Beside it: `settings.json`, `plugins.json` (the plugins switched off), `plugin-prefs.json` (what plugins keep through `prefs_set`, by plugin id), `protocols/` (the user's protocols' files), `protocols.json` (the protocols switched off), `protocol-cache.json` (what each MCP server offered at its last connection), `protocol-logs/` (each stdio server's stderr, its newest run; all four were named `mcp…` before and are moved), `code-mode/` (each Code Mode conversation's saved REPL context), `blender/` (installed add-on support files; running instances instead use their agent's scratchpad), `models.json`, the
login files (`accounts.json`, `codex.json`, `antigravity.json`, `openrouter.json`, `nodeclub.json`,
`refuseless.json`, `musecode.json`, `freebuff.json`, `freebuff-device.json`, `freebuff-device-key.json`, `local.json`,
`quick.json`;
owner-only on Linux), `window.json` (the window's
size, place and maximized state), `seats.json` (which account each
conversation is on), `tasks/` and `task-output/` (background tasks; `tasks/tray.json` is what
the taskbar widget shows, `tray/` holds its copy of oak.exe), `browser/` (the browsers'
profiles), `browser-profiles/` (the named ones), `plugins/` (plugins to load, and what they keep is beside it under names of their
own), `plugin-copies/` (the copies of those libraries Oak loads, named after each file, its place,
size and time; reused while the file is unchanged, the rest deleted at each start or reload), `bin/` (programs for the agent's commands: the bundled ripgrep, `src/toolbin.rs`),
`wallpapers/` (Home's wallpapers to pick from), `usage.jsonl`, `crash.log` and `startup.log` (how long the last start took, step by step, until
its first frame was on screen, and the processor time spent by each step: `src/startup.rs`; the
place to look first when Oak starts slowly). Files are written to a temporary file and renamed over
the old one, so a crash never leaves half a file; an unreadable `settings.json` is kept as
`settings.json.unreadable`.

Each session also has a scratchpad, `.oak/tmp/tempN` in its working folder (`.oak/.gitignore` keeps
it out of git), for test scripts, downloads, screenshots and logs. Settings → Scratchpads can clear
those left idle (`src/scratch.rs`).

## Writing UI copy

Treat the user as a competent adult. Never condescend or overexplain.
Use concise, neutral labels and status text.
Do not narrate obvious controls, add unsolicited tutorials, or use praise, reassurance,
or baby talk. Remove redundant copy rather than rephrasing it. Put non-obvious
operational details in short tooltips; keep costs, security implications, shared scope,
and destructive consequences explicit. Apply the same standard to generated replies.
Never use bullets, middle dots, or em dashes as separators between labels, metadata, or sentences.
Use commas, parentheses, or spacing instead; actual Markdown lists are unaffected.

## The app

**Layout.** A title bar Oak draws itself, a strip of tabs and the page. The first tab is Home
(Alt+H): a wallpaper (`src/wallpaper.rs`; Settings → Wallpaper opens a grid of them, three to a row, the one under
the pointer playing, to pick Oak's forest or a PNG, JPEG, GIF or WebP from `wallpapers/` in Oak's
folder; animated ones play from a decoding thread, and small ones, pixel art, are drawn sharp;
each keeps its shape, scaled to cover the window and cropped about the middle: `home::paint_cover`), what
plugins draw on it (`Hooks::home_scene`), and in the bottom right corner the Settings gear and the
menu button. The menu is shut at start; the button, Alt+H on Home or a click on the Home tab opens
it, and Esc, the button (an X then) or a click on the wallpaper shuts it. It is one box: its left
side has the tabs Conversations and Cloud, then those plugins add, then
Protocols and Plugins (and at the row's right end the links plugins add), its right side the providers' accounts, a usage preview and the version. The Conversations tab's search field (Ctrl+F, which opens the menu too) narrows the list to
conversations whose title or folder has every word typed, in any case; Enter opens the first, Esc
clears it, and the query isn't kept across restarts. The picker beside it sorts the list by the
latest message or by scratchpad size, biggest first (each row then shows its size; kept in settings).
Conversations,
cloud sessions and plugins' tabs open as tabs, which are remembered. The browsers' tab keys work, Home
counting as the first tab: Ctrl+W (or Ctrl+F4) closes the tab shown, even while typing (Ctrl+Backspace
deletes a word), Ctrl+Tab and Ctrl+PageDown go to the next tab and Ctrl+Shift+Tab and Ctrl+PageUp to
the previous one (round the ends), Ctrl+1 to Ctrl+8 pick one, Ctrl+9 the last, and Ctrl+Shift+T (or
the tab menu) reopens the tabs closed since Oak started, newest first, in their places. Closing a tab
never closes its conversation, its draft or its work. A conversation shows its
transcript with a minimap, the session box (title, folder, provider, model, effort, fallbacks, goal, notes,
agents, progress meters) and the message box with the smart context and account boxes (and a
plugin's box left of its buttons: Unslop's).
The message box stays docked to the bottom as drafts, attachments and notices change its height,
including in a side chat.
A click on the session box's folder opens it in the system's file manager; its right-click menu
has Open folder and Copy path (a folder that no longer exists can only be copied).
Settings is the gear in a conversation's bottom left corner and Home's bottom right one; its Theme row offers Oak's two
built-in themes (Oak, and Liquid Glass) and those plugins add, and its Font row shows once a plugin adds a choice to IBM
Plex Sans. Plugins are managed on Home, not in Settings, and so are their
settings: Settings holds none of a plugin's controls.
Liquid Glass (`src/glass.rs`, id `glass`) is Antigravity's colors (its Hub's dark Onyx theme) with the iOS 27 dark
Liquid Glass of liquid_glass_widgets, its premium shader ported to WGSL (`src/glass/`): the paraxial lens at the edges,
the frost (a wide, luminance-weighted blur with a ghost of the content through it), the body's tint, saturation and
shade, the meniscus, the rim light at the ends of the light's axis and the dark hairline across it, and the glint where
it is pressed. As in the package, glass is for what floats: the theme's panels are glass in areas above the page (the
session box and the chat's corner boxes, menus, popups, tooltips, dialogs, toasts), in the message box's row and over
Home's wallpaper, and the page's own cards stay the flat Onyx ones; a selected item is the clear indicator lens, a
switch held down lifts a lens over its knob, and nothing casts a shadow. It changes no layout and no shape (square
corners stay square): Oak draws as in any theme, and as the frame ends each glass rectangle gets a GPU callback in its
place, over the flat panel (what shows if the GPU part can't draw); the theme's colors painted on glass become white
fills at a few percent. Glass shows what was painted under it this frame, glass under glass included: before egui's
pass Oak draws each group's backdrop (glass that sees the same one) into a texture of its own with a second egui
renderer, and blurs it.
The room above the gear is the plugins' corner (`Hooks::composer_corner`, `src/plugins.rs`): a
plugin can draw one row there. Only hooks that lay out or paint a row take space, and each row
is clipped to the corner so it cannot move or cover the rest of the composer. The rows sit in the
composer row's height, right above the gear; when there are more than fit, the message box grows
taller with them, so its bottom always stays level with the buttons, the gear and the boxes beside it.
Home's Plugins tab (`src/plugins_view.rs`), like a game's mod manager, lists every library found
with its name, version and description (or why it isn't running) and a switch; a click on a row
flips it, and Enable all, Disable all and Open folder (Oak's `plugins` folder) sit above the list.
A running plugin that has settings (`SettingsSection`s) has a gear on its row, left of the switch,
and Settings in the row's right-click menu: either opens the plugin's own settings page
(`App::plugin_settings_page`, `src/settings.rs`), drawn in the window as Settings is, under the
plugin's name with its sections down the box (each titled when it has several); its X or Esc goes
back to the Plugins tab. What a plugin saves stays where it always kept it.
Home's Protocols tab (`src/mcp_view.rs`) lists the protocols conversations can load (see Protocols
below): Oak's own (Browser, Window, Blender) first, then those of the files in `protocols/`, each
with its title, a switch (a click on the row flips it; off ends its connection, and switches Browser
or Window out of LOAD), its state (Connected, Starting, Failed), its tool count and a line under its
name (its command or URL, why it can't be used, or its last error). Open folder
and Reload sit above the list; Reload reads the files again and ends the connections whose
configuration changed. Home > Protocols' Blender row
says how many private agent Blenders are running, or whether the user's own is available for an
explicit attachment, and, until the add-on is installed (or while the installed copy differs from
Oak's), has Install add-on, needed only for attaching the user's own Blender (agents start their own
otherwise). The right-click menu
has Connect (start it now and list its tools again), Disconnect, Open file, Copy path, Open log and,
for Blender, Install add-on again, Save add-on file (for Blender's Install from Disk), Open Blender,
Copy Claude Code command and Copy Codex config (both run `oak --mcp blender`).
Switches are saved by plugin id in `plugins.json` and apply at the next start or Reload: a changed
row says "On after reload" or "Off after reload", and Restart to apply (shown then) restarts Oak as
an update's restart does. Reload (`plugins::reload`) looks at the folders again without a restart:
libraries added or rebuilt start (a rebuilt or stopped one from a fresh copy), those removed or
switched off stop, and plugins that didn't change keep running untouched; the heading says for a
few seconds what it did ("Restarted Hello C; started Hello D"). It is lit while there is something
to apply (a switch changed, or a library added, changed or removed: the folders are looked at every
two seconds while the tab is shown), and a running plugin whose file changed says "Changed on
disk". Oak loads each library from a copy in `plugin-copies/` so the file itself can be rebuilt or
replaced while Oak runs (Windows refuses to write a loaded DLL). A plugin that stops at a reload
gets its `exiting` hook and its parts leave the window, but its library is never unloaded (its
threads may still run in it): a clean slate takes a restart. Switching a plugin on switches on the
plugins it needs; one whose needs are switched off says so. A switched-off plugin's library is
opened to read who it is but never started.
The window opens with the size, place and maximized state it was closed with (`src/geometry.rs`);
a place no monitor shows any more is left to the system, and a window partly off its monitor is
moved and shrunk onto it. Wayland keeps only the size and maximized state.
Ctrl+Plus (or Ctrl+=), Ctrl+Minus and Ctrl+0 zoom the whole window, as Settings → Zoom does; the
zoom is kept in `settings.json` across launches.

**Providers and accounts** (`src/providers.rs`). Every provider has accounts (a subscription login
or an API key) and every conversation a provider. Oak builds every request in its own format
(`src/oak.rs`), which each provider translates to its wire format and back; conversations are
stored in it too, and reasoning keeps which provider wrote it, so no provider gets a signature it
can't read.

- Logins and keys are kept on this machine, in Oak's folder. Oak renews a Claude or Codex login
  itself when its access token nears expiry. A plugin can bring a vault (`src/api/vault.rs`,
  `oak_sdk::Vault`): somewhere else that keeps the logins and keys and renews them for several
  machines; Oak then asks it for access tokens and keeps only those, and uses the logins on this
  machine only when none of the vault's can be.
- Account rotation (`src/rotation.rs`): a conversation keeps its account for as long as it can,
  since its prompt cache lives there. A new one takes the account with the most room. It moves when
  its account is used up (the request is sent again through another, and the transcript says
  "account rotated"), or, once its cache is cold anyway, when its account is nearly used up and
  another has much more. A key the provider rejects is skipped the same way. With every account
  used up, Oak waits for the first reset, or until an account is signed in or comes free early: an
  account refused for its limit is usable again once a usage reading taken after the refusal shows
  its 5-hour and weekly limits below 100% (a reset used here or anywhere else; a verified Ultra
  account has no 5-hour limit, and its weekly one is enough), and waiting
  sessions try again then. Shared 5-hour/weekly limits apply to every Claude model; model-scoped
  quotas (including Fable's `limits[].scope.model`, even with a null id) apply only to that family.
  They hold on every plan: Max includes Fable but caps it too, and a used-up allowance is refused
  with claim `seven_day_overage_included` (headers `..-7d_oi-..`) while the 5-hour and weekly
  windows still have room. Waiting on only such a quota, the transcript's wait line names it ("All
  accounts at their Fable limit", `api::limit_scope`) and says other models have room. Refusals without a shared-window claim stay with the requested family, and cloud refusals with
  no known scope never disable desktop models. A scoped refusal clears only after a newer reading
  shows both shared room and that family's own room, or its reset passes. Legacy account-wide
  refusals retain the early-reset recognition above. Waiting sessions recheck eligibility for their
  current model, including fixed seats, so a stale wait cannot hide available capacity.
  Settings can turn rotation off. The model is never told.
- Fallbacks (`Conversation::fallbacks`, `agent::pick_fallback`): a conversation can list other
  models, each with its provider, in order of priority. The session box's fallback row (its
  hover lists them) opens the list: move up, move down, remove, and Add… with the models the
  provider row offers, less the conversation's own and those listed. Changes are saved at once;
  forks and side chats copy the list, subagents start without one. When a request fails for good
  (after the usual retries, or a rate limit's five minutes) or every account of its provider is
  at its limit, the conversation moves to the first fallback that is not its model now nor one
  that failed this turn, that a chat or Code Mode can use, and that can take a request now
  (`providers::Kind::ready`: signed in; for Claude, Codex and Antigravity an account with room;
  for a plugin's model its plugin running; other providers are tried as they are). The request
  goes again there. The move is a switch as the session box makes one (effort kept, cache cold,
  the model not told), ends a wait for the old provider's limit, and
  adds a transcript line, "fell back to <model> on <provider>, after: <why>". The conversation
  stays on the fallback. A reply clears what failed this turn; with no fallback left the turn
  fails, or waits for the reset, as it did. A model the user picked meanwhile goes first, and a
  safety classifier's refusal or an interrupt never falls back.
- Switching models works between every provider (the session box's pickers, a subagent's, a
  fallback's, a Mix's SWITCH). Nothing is written to the transcript and the model isn't told; what
  it needs is what any change needs (`src/prefix.rs`). What one provider's model wrote and another
  can't take is left out of its request (`agent::for_provider`): reasoning another provider signed
  (as before), a Quick snippet's own call block (`quick::QUIET_KEY`) and its result, the Freebuff
  call an Oak call was made from (`freebuff_tools::KEY`). Tool call ids are fitted to Claude's
  pattern and made unique per conversation (`oak::fit_call_ids`) wherever an API checks them
  (Claude, Muse, Codex, Antigravity, and OpenRouter only for its Claude models), so a plugin's,
  NodeClub's or Quick's ids never collide with another reply's. Antigravity gives another model's
  calls the stand-in signature Google documents (`skip_thought_signature_validator`) rather than
  turning them into text. Each reply's blocks are marked with the provider that wrote them
  (`agent::BY_KEY`, never sent), and Quick, which keeps its own copy of a thread, is sent what
  other models wrote since its own last reply (`agent::unseen_by`, carried as `<conversation_so_far>`),
  so switching to it loses nothing.
- A Claude account's plan is read from its profile: Pro, Max, Ultra, Team Standard or Team
  Premium. Max and Ultra are the two sizes of Max (rate-limit tiers `..._max_5x` and `..._max_20x`);
  accounts, the usage log and vaults keep them as "Max 5x" and "Max 20x", the names they had
  before, and only what Oak shows says Max and Ultra (`api::plan_label`). Summed on Home, each
  account's limits count by its plan's usage over Pro (`providers_view::weight`): Max 5x and Ultra
  10x, as Anthropic states them since October 30, 2026 (Max 5x Pro every 5 hours and every week,
  Ultra 10x Pro every week and no 5-hour limit once verified; the old "20x" was per 5 hours).
- A used-up week holds the 5-hour window too (`api::effective_limits`): while an account's (or
  a Codex login's) shared weekly limit is at 100%, its 5-hour limit counts as 100% until the week
  resets, whatever that window reads, on its card and in the composer (its hover says so, with
  what the window itself reads), in the reset dialog's "used" line and in Home's summed lines,
  whose 5-hour reset is then the week's. Rotation already holds such an account until the week's
  reset, so a 5-hour reset frees nothing. A model's own quota (Fable's week) holds only that
  family and leaves the 5-hour window as read, and a plan without a 5-hour window (a verified
  Ultra) is given none. The readings themselves (the usage log, `/v1/status`) stay as sent.
- Usage-limit resets (`src/api/resets.rs`, `src/resets_view.rs`): the resets Anthropic offers a
  Claude account ("Full reset", "Fable reset", "5-hour reset", "Weekly reset", as Claude Code's
  /limit-reset lists them) come in its usage report (`/api/oauth/usage?cedar_ember=1`, read at
  least every 15 minutes) and are listed under its card on Home with their expiry. Clicking one
  opens a dialog that reads them afresh, says what it refills, what is left of those limits and
  when it expires, and uses it (`/api/organizations/{org}/reset_rate_limits`, one request id per
  dialog so trying again can't use it twice) only once the person ticks that they understand and
  presses Yes. Nothing else uses one: not an agent, not rotation, not a retry.
- The Claude models to pick from are the newest Opus, Sonnet, Haiku and Fable the Models API lists
  (`src/api/models.rs`). Fable's entry says how the accounts' plans pay for it (`fable_offer`): on
  Pro and Team Standard, Fable 5.2's launch offer includes it "until Nov 16, 2026, 11:59 PM PT",
  then it takes usage credits; Max, Ultra and Team Premium include it; its tooltip lists the accounts.
- Effort levels are each model's own (Claude's from the Models API). `max_tokens` is the model's
  maximum. The prompt cache lifetime (Settings, `src/cache_ttl.rs`) is chosen per conversation:
  an hour when a long enough pause is likely, else 5 minutes.
- The prefix invariant (`src/prefix.rs`, no setting): what a conversation has sent the model (the
  system prompt as rendered, the session memory and context block, the tools, every message) is
  never changed; requests only add to it. Smart context acting (a summary replacing the older part)
  is the only moment it is rebuilt. The cache expiring, another account or model, an effort, date,
  goal, note, notes-limit or instruction-file change, smart context turned off or on, a save and
  restart, a newer Oak's prompt text: none of them changes it. What changes reaches the model after
  the latest user turn ("Today's date is now", "The limit on your notes is now", a tool
  definition, Claude's effort): Claude and Codex as their own mid-conversation messages, which turn themselves off if
  refused (then, like other providers, as a text note where they were). A model switch is not
  told: the new model reads the conversation as its own, under the system prompt as it was sent,
  until a summary renews it (a Mix's SWITCH result, which the model asked for, is the exception). A loaded kit stays where
  its LOAD put it: its instructions LOAD's result, its tool right after it (where tools can't be
  added, called through LOAD). Only once a summary covers the LOAD do the kit's prompt block and tool
  join the system prompt and tool list. A revert to before a LOAD unloads the kit. A revert (or an
  edit) to before a summary's cut puts back what requests went with before that summary (the
  summary before it, the prefix, the session memory: `Conversation::earlier`, saved with it, the
  last 8 summaries back) and sends exactly what was sent then, so a prompt cache still alive is
  read again; only an older Oak's summary is rebuilt. A side chat sends what its source sent.
  Regression tests (`agent::tests::the_prefix_only_grows_*`) check each provider's wire body.
  Freebuff is the one exception, by design: the service requires its CLI's own system prompt and
  tools with the current date, so its prompt's date is always today's (`freebuff::system_prompt`,
  tested by `the_prompt_has_todays_date_and_the_row_name`) and a new day renews its cache; its
  messages still only grow. No other provider or event gets this.
- Quick keeps a conversation itself (its thread on Quick's server), so a request sends only its
  newest user turn. A conversation that other providers' models worked on since Quick's own last
  reply sends what it missed too, as `<conversation_so_far>` in that message (`agent::unseen_by`,
  `quick::so_far`): the user turns, calls, results and replies since, without what the request
  sends anyway. With nothing new from the user it is told to carry on from there (`CARRY_ON_SO_FAR`).
- A local llama.cpp server (`src/local.rs`) gets only the basic tools and the `{{#local}}` parts of
  the prompt, and is never offered to subagents (it serves one request at a time).
- Plugins' models (`src/provided.rs`, `oak_sdk::ModelProvider`, interface version 10) are one
  provider, `Kind::Plugin`, named after the plugins' providers ("Duck.ai") in the pickers and
  shown with the plugin's logo. Their ids are `plugin:<provider>/<model>`; each request goes to the
  plugin whole (system prompt, Oak's messages, tools) and its events come back as Oak's, so their
  tool calls show and run as any model's. They are free (no accounts; the composer's box names the
  provider) and get what a local model gets (`local::basic`): the basic tools, the `{{#local}}`
  prompt, short summaries, no MCP servers. Normal conversations only: chats and Code Mode refuse
  them (`agent::chat_refuses_provider`, `code_mode_refuses_provider`) and subagents can't use them
  (left out of AGENT new's list, refused by `agent_new`). A model whose plugin isn't running fails
  its request with a message saying so. A failure starting "prompt is too long" summarizes the
  conversation and asks again, as for any provider.

**The agent** (`src/agent.rs`, `src/agent/`). One worker thread per conversation. Its tools
(`src/tools/defs.rs`): READ, WRITE, EDIT, EXECUTE, SEARCH, FETCH, SCREENSHOT, DISPLAY, ASK,
ACCOUNTS, LOAD, and TASK, NOTE, HEARTBEAT, PROGRESS and GOAL, which each take an
`action` and show as tools of their own (`TASK action=wait` is TASK_WAIT). LOAD loads a kit (AGENT,
BROWSER, WINDOW, or another protocol by its name): a tool and its instructions, left out of the
prompt until needed.

**WINDOW** (Windows; `src/window_tool.rs`, a family as BROWSER is: `WINDOW action=click` is
WINDOW_CLICK). GUI apps on the hidden desktop `oak-apps` (`src/winctl.rs`), driven without touching
the user's screen, mouse, keyboard or focus: `launch` (through `oak --window launch`, a child of
Oak's in a job object, so the app ends with Oak, `close`, or the conversation's deletion; its output
goes to a log in the scratchpad; only a window of the app's own process tree counts, so an app that
hands off to one already running, as Windows 11's Store Notepad does, fails rather than taking the
user's window), `list`, `shot`, `click`, `move`, `drag`, `scroll`, `type`, `key`, `resize`, `close`.
Input actions take `shot` (a screenshot after it) and `wait`. Ids read `hidden:0x…`; the window a
conversation last launched or acted on is its current one, the default for `window`. Everything runs
on one thread attached to the hidden desktop (`winctl::on_desktop`). Mouse messages go to the
deepest child control under the point (and to the pressed one until release), keys to the app's
focused control; a control gets typed text as WM_CHAR alone, since its message loop translates a
posted key-down into a second character. `attach` (with `why`, and `title` to pick one first) asks
the user for one of their own windows (visible, titled, unowned, not tool or cloaked windows, not
Oak's); the one allowed is `user:0x…` and the current window until `detach` or the conversation's
deletion. It gets the same posted input from the calling thread (`winctl::input_gently`) except that
it is never moved (hidden windows are moved under the hidden desktop's cursor for apps that read it),
and it can't be closed. Other `user:` ids, and ids of windows on neither, are refused. Ordinary
launch arguments use Windows C-runtime quoting (spaces, quotes and trailing backslashes preserved).
For `cmd.exe /c` or `/k`, give the whole command as one string after the switch, with cmd's own
quotes: Oak preserves it verbatim and adds `/s` and the outer quote pair cmd strips, rather than
backslash-escaping its quotes as ordinary executable arguments. Closing a launched window also
recognizes a first window created by a child of the launcher (for example Python started by cmd).
The close result clears its current-window state before formatting the window id, which reads that
state too: holding the mutex across both deadlocked Oak. A bounded subprocess regression test guards
that path. Launched console programs use `CREATE_NO_WINDOW` (their GUI windows still use the hidden
desktop, and their output still goes to the log): without it, Windows Terminal could open a console
on the user's real desktop despite `lpDesktop`. The Tk regression also checks that no console is
attached. Elsewhere WINDOW stays instructions (Xvfb, xdotool).

**Protocols** (`src/mcp.rs`; MCP servers, as models know them, and Oak's own kits). The user sees
"Protocols"; the model's prompts say MCP. Oak's own are this repository's `protocols/` files (built
into Oak): `browser.json` and `window.json` are Oak's BROWSER and WINDOW kits (`"builtin"`, run by
`src/tools.rs` as before, listed and switched like any protocol; their `description` is LOAD's), and
`blender.json` the Blender server in Oak's process. Each protocol switched on is a kit offered by
LOAD (not in chats or to a local model), named after it in upper case (`BLENDER`; `-` as `_`),
described by its file's `description` or its instructions' first sentence. Loading an MCP server
starts it (or reconnects), lists its tools and returns them with its instructions; it then joins the
conversation's tools as one tool named after the kit, as Oak's families are: its `action` picks the
server's tool, named in lower case without the server's name or a leading `get_` (`BLENDER
action=scene_info` calls `get_scene_info`, `execute_code` `execute_blender_code`; a name that would
collide keeps the tool's own), every action's fields side by side (one two actions describe
differently becomes untyped) and listed per action in its description; a call passes on only its
action's fields. Calls run and show as Oak's families' do (`BLENDER_SCENE_INFO`, shown "BLENDER
scene_info"); conversations from before keep working (`mcp:blender` kits, `mcp__blender__…` calls).
It joins right after that LOAD, as any kit's do (`src/prefix.rs`), and once a summary covers the
LOAD its instructions (tool names rewritten as `BLENDER action=…`) join the system prompt, under "#
MCP Server Instructions". Where the model can't take new tools mid-conversation
(`prefix::Mode::Generic`), LOAD's text describes it and it is called through LOAD with the tool's
input in `input` (an older `{"tool", "arguments"}` still works). A call returns the server's text and
pictures (audio and binary resources are named, not passed on); long text goes to the scratchpad as
EXECUTE's does, the user's message interrupts it, and a server that exited is started again once. The
user's protocols come from `protocols/` in Oak's folder: a JSON file per server named after it
(`{"command", "args", "env", "cwd"}` for a program on stdio, `{"type": "http", "url", "headers"}`
for streamable HTTP, `${VAR}` in headers from the environment), or a file with `mcpServers` as
Claude Desktop's and `.mcp.json` are; optional `title`, `description`, `timeout` (seconds per call,
default 600) and `window` (it shows a program's window). A name that would name its tool as one of
Oak's, one of Oak's own protocols' names and a name used twice are refused (the row says why), as is
`builtin` outside Oak's own files. The old HTTP+SSE transport and OAuth sign-in aren't
supported. One connection per server serves every conversation, made on first use; stdio servers end
with Oak. What a server offered is cached in `protocol-cache.json`, so a conversation's next request lists
the tools without starting it; a server saying its tools changed is listed again before its next call.
Oak answers a server's ping and roots (none) and declines sampling and elicitation.

**Blender** (`src/mcp/blender.rs`, `assets/blender/oak_mcp.py`). The built-in protocol `blender` (the BLENDER kit)
offers the community Blender MCP server's tools (github.com/ahujasid/mcp-for-blender, as Claude Code
and Codex users run it): get_scene_info, get_object_info (with its world bounding box),
get_viewport_screenshot, execute_blender_code (what the code prints; a traceback on error) and the Poly
Haven tools (categories, search, download_polyhaven_asset for HDRIs as the world's lighting, textures
as a material and models imported, set_texture). It reaches a running Blender through an add-on's
socket on 127.0.0.1:9876 (`BLENDER_HOST`, `BLENDER_PORT`): Oak's add-on or the community one, which
speak the same protocol; every tool is Python run through `execute_code`, and Poly Haven's files are
downloaded by Oak into the system's temp folder (`oak-polyhaven/`). Oak's add-on runs commands on
Blender's main thread, starts with Blender (not in background mode) and has an Oak tab in the 3D
viewport's sidebar to stop or start it. Install add-on finds Blender (`OAK_BLENDER`, the PATH, the
usual install folders and Steam) and runs it twice in the background, to copy the add-on into its
user add-ons and then switch it on and save its preferences; with no Blender program found it copies
the file into each of Blender's settings folders (`BLENDER_USER_RESOURCES` overrides where those are).
`oak --mcp blender` serves the same tools over stdio to other MCP clients.

**Which Blender** (`src/mcp/blender_run.rs`). Loading BLENDER starts one private hidden Blender per
agent/conversation (including subagents). No shared instance exists and no first-call autodetection
or fallback selects the user's Blender. `with_caller` scopes the conversation, folder, scratchpad
and cancellation flag with an unwind-safe guard around LOAD and calls. The instance has its own
loopback port, `blender-user` settings, `blender-runtime` add-on/bootstrap files, and log in that
agent's scratchpad. `BLENDER_HOST`/`BLENDER_PORT` apply only to standalone `oak --mcp blender`, not
agent calls. Older persisted kits start their private instance on first use. A failed LOAD keeps the
kit available, so an agent can download a portable Blender and call `use_own(program=...)`.

`use_own` accepts an optional `program` (absolute or relative to the conversation folder) and `.blend`
`file`; a missing executable fails without fallback or destroying a running scene. An existing
instance is reused, or replaced for a different executable/file only if its scene is saved.
`restart` and `close` refuse unsaved changes unless `discard=true`. Closed instances stay closed
until `use_own`/`restart` (or LOAD) reopens; a dead/busy instance fails without redirecting.
`use_users` is an explicit exclusive attachment to the user's add-on on 9876, never a default: it
keeps the agent's private scene intact, `use_own` returns to that scene, and a second agent cannot
attach concurrently. The user's process is never closed by Oak's lifecycle actions. Interactive
Blender must never be launched directly through TASK/EXECUTE or bound to 9876 (the Minecraft session
bypassed the hidden launcher that way); background `-b` renders are fine.

Per-agent startup gates prevent duplicate launches without serializing unrelated agents. Ports are
reserved across startup; retired ports are not reused during Oak's lifetime so stale input cannot
reach another instance. Cancellation/deletion checks prevent a pending launch from resurrecting a
deleted agent. Checked RAII kill-on-close jobs own each launcher and its process tree; a pipe
handshake prevents descendants escaping before job assignment. Cleanup closes the job, frame/input
state expires, and conversation deletion (the GUI's or AGENT_DELETE's) or Oak shutdown ends all
private instances. The window a conversation last launched, attached or acted on is its current one:
actions without `window` go to it, and with none they fail instead of picking some other hidden
window. A Blender another agent runs is left out of WINDOW's `list` and refused by id, including its
other windows (`blender_run::can_use`), and so is any app another conversation launched
(`others_app`, by window or process); closing your own Blender through WINDOW goes through
`close_blender`'s unsaved-work check. Unsaved work counts Blender's own dirty flag and any agent
Python (`execute_blender_code`, texture and Poly Haven imports) since the last save or load, tracked
by the add-on (1.2.0, `session_state`); read-only scene info and screenshots don't count. `restart`
opens `file` or an empty scene.

The bootstrap calls `start_on(port)`, closes the splash, suppresses old installed copies' auto-start
on 9876, and enables event simulation (`--enable-event-simulate`). Blender's OpenGL cannot be captured
through PrintWindow: the add-on's `window_frame` saves `bpy.ops.screen.screenshot` pictures and
`simulate` feeds `event_simulate` input (y flipped; keys use the last pointer location). WINDOW's
input/shot and the dedicated interactive Blender preview use these. The preview selects only the
shown agent's Blender, independently of WINDOW's current app; subagent views work too.

- The system prompt is `prompts/<role>.md` (who the model is; every file but `tools.md` and
  `chat.md` is a role offered on the New screen, `super.md` only as its Super agent; plugins can add roles) with `prompts/tools.md` in place of `{{tools}}`. `{{folder}}`,
  `{{scratch}}`, `{{date}}`, `{{os}}`, `{{model}}` and `{{agent}}` are filled in, `{{#windows}}`,
  `{{#unix}}`, `{{#main}}`, `{{#agent}}`, `{{#full}}`, `{{#local}}`, `{{#goal}}` and kit blocks are kept only where they apply, and
  comments are dropped (`build.rs`, `src/agent/prompt.rs`). It is compiled in: a change takes a
  rebuild. It calls the model Oak, whichever lab made it.
- SEARCH (`src/tools/search.rs`) asks one provider, the next when it fails or finds nothing:
  Google (through Serper) only when `SERPER_API_KEY` is set, then Firecrawl, Parallel, Exa and
  Tavily, which need no key (`FIRECRAWL_API_KEY`, `PARALLEL_API_KEY`, `EXA_API_KEY` and
  `TAVILY_API_KEY` are used when set; `OAK_SEARCH_PROVIDER` puts one first). One that says its
  rate limit is reached goes last for 15 minutes. A result is a title, a link and a summary of up
  to 500 characters; `max_results` is 1-20 and `page` (up to 5) reads on; `site:` in every query
  keeps the results to those sites.
- SEARCH with `images` (`src/tools/image_search.rs`) searches for images instead, with the same
  objective, queries, `max_results` (1-24, default 12) and `page`: it returns one picture, a sheet of up to 24
  thumbnails laid out like Google Images in the Oak theme's colors (columns, each thumbnail at its
  own height with rounded corners), each with its number (large, oak green), its title and the
  start of its link under it, and each one's title, size and site as text. SEARCH with `image` (and nothing else) shows one in full by
  its number (or any image by its URL) with its link and its page's, and saves the file in the
  scratchpad's `images` folder. The numbers count up through the conversation. The images come
  from Google (Serper, with `SERPER_API_KEY`) or else from Brave's image search page, fetched
  with `curl`; no browser is used.
- A reply's tool calls run in parallel, each as soon as it has streamed in, waiting only for calls
  on a file it touches, and for ASK and CD, which run on their own (`src/agent/tool_calls.rs`).
- CD switches the conversation's working folder for the rest of the session (relative paths,
  EXECUTE and new tasks; running tasks keep theirs). The scratchpad stays where it was (its name
  becomes its full path), the result carries the new folder's project instructions, and the
  system prompt keeps the folder its cache was written with (`prefix::Pin::folder`).
- EXECUTE takes a timeout (0 to 550 seconds, 0 for the background) and a one-line summary shown in
  place of the command. Longer commands become background tasks (`src/tasks.rs`), kept across
  restarts; on closing, Oak can leave them running (on Windows each one's bash is handed a handle
  to its own job, so no Oak process stays behind to lock oak.exe; the next Oak takes it back). On
  Windows, tasks left running are counted on the taskbar until Oak starts again (`src/tray.rs`):
  a notification-area icon showing the number as tall digits, moved out of Windows 11's overflow
  the first time; clicking it lists the tasks (name, conversation, run time or how it ended, last
  output line; a task opens its output file), its menu opens Oak or hides it. When they have all
  ended it says so in a notification and goes. It runs from a copy of oak.exe (`tray/Oak
  tasks.exe` in Oak's folder), not oak.exe itself, and not when Oak is restarting. A
  TASK wait under way when Oak closes (Pause all and close, or Exit now with the tasks kept) isn't
  ended or answered with "Oak closed": the next Oak runs it again first thing, for what was left of
  its timeout (only the time Oak ran counts, `Conversation::task_waits`), and its result says how
  long Oak was closed; a task that ended meanwhile reports its real running time. Other calls cut
  off by a close get a result saying so. Every
  command's PATH starts with Oak's `bin/` folder, which holds the ripgrep bundled at build time
  (`src/toolbin.rs`; written there on a run's first command when missing or different), and the
  prompt tells the model to search with `rg` instead of grep. Output over 50,000 characters is saved to the
  scratchpad. Before a command runs, `src/guard.rs` refuses likely-destructive ones (recursive
  removals of the home folder, the working folder or `.git`, disk commands, git commands that
  discard work, killing Oak) and `src/sleeps.rs` refuses sleeps over 5 seconds; the error says
  what to do instead. On Linux each command leads its own process group, killed with it.
- Smart context (`src/rolling.rs`): past a trigger (120k tokens by default) the older part is
  summarized in the background and the newest ~40k stay verbatim. The summary request (in the
  background, or blocking when a request is refused as too long) sends the chat request's system
  prompt and tools, then only what the summary replaces, as the model saw it (the summary in use
  and the messages before the cut: no part kept verbatim, cleared tool output, other providers'
  reasoning or reverted messages; `agent::summary_messages`), then the instruction. The
  transcript only grows: a banner marks where a summary was made and a line where its part ends.
  Old tool output is replaced by a placeholder in requests, and a line marks where that stops:
  only when a summary is made, the one moment what was sent may change. Tool output in the newest
  ~20k tokens stays (the smart context box's "Tool output kept", in thousands of tokens, 0 to 1000;
  a change applies to summaries made after it). A suggested trigger is never below keep × 4/3, the
  least that allows the keep, so it is always for the keep in the box. Turning smart context
  off for a conversation stops new summaries; the one in use stays. It doesn't apply to Quick
  (`rolling::applies`): Quick keeps the conversation on its own thread, so its conversations get
  no summaries and the composer row shows only the account box.
  The trigger is at most 90% of the model's window, and never past it: a window too short for the
  least trigger (20k; a plugin's model, Duck.ai's 19k) gets 90% of it (`rolling::max_trigger`). A
  summary takes out at least 10k tokens, or for a basic model (local or a plugin's) a quarter of
  its trigger when that is less (`rolling::least_for`). A refusal as too long that gives no size
  counts as the last measured request plus an estimate of what came after it (`rolling::size_of`),
  not as the last request alone, or the cut would land a turn too early.
- Goals: the model keeps working until it calls GOAL finish; "pause goal" stops Oak asking it to
  continue without telling it. ASK waits for the user's answers, except while a goal is set (not
  paused or finished): then it returns at once, telling the model the user was asked
  asynchronously and to keep working, the questions wait in the card (and with the conversation,
  `pending_ask`, across a restart), and the answers or the skip reach the model as a message that
  repeats the questions (`Session::answer_questions`). A new ASK replaces questions still
  unanswered and lists them in its result, so the model asks again with the full list it wants.
  (REQUEST_ASK, which gated ASK during a goal, is gone: a conversation that still lists it is told
  to ASK, and one an older Oak left waiting is told so on start.)
  An endless goal (the Endless box when editing it) refuses GOAL justify and finish, so it lasts
  until the user pauses or clears it; the model is told in the goal's message and its memory block,
  and changing only the mode sends the goal again.
- Notes (sent with every request, so they outlive summaries; Settings → Notes limit caps them all
  together, 2,000 tokens by default: NOTE itself refuses a new note, or an update making a note
  longer, that would pass it, and notes already past a lowered limit are kept but must be shortened
  or deleted first; the system prompt names the limit, a change mid-session comes as a message, and
  Notes reminder nudges the model at 85% of it), heartbeats, progress meters (a
  shell command whose output's last line is `x/y`, `N%` or a number; `src/progress.rs`) and
  subagents (`src/agent/subagents.rs`; sessions message each other, and a subagent's final reply
  reaches the main session by itself). AGENT pause and resume take an agent's `id` or a `model`
  (id, family word such as opus or sol, or provider), acting on every agent on it and naming the
  ones left alone.
- Files changed on disk by someone else are reported to the model before its next request
  (`src/changes.rs`). Instruction files (CLAUDE.md, AGENTS.md) and project memory are off for now
  (`src/instructions.rs`).
- Messages sent while Oak works are queued for its next request. Pause lets the request and
  commands under way finish; ultrapause stops everything at once. A pause survives a restart, even one
  still waiting for its request when Oak closed. Reverting
  (a message's icon, or right-click inside a reply) takes the conversation back, not the files.
- A safety classifier's refusal is retried as failed requests are, then every minute and a half;
  there is no fallback to another model (a conversation's fallbacks included). OpenRouter's built-in models (`openrouter::MODELS`) are
  DeepSeek V4.1 Flash 1017, Gemma 4 31B, and the free Gemma 4 31B, Particle-1 and Nemotron 3.5
  Super VL (capped at $0, so they never fall through to a paid provider). Settings' "OpenRouter
  models" row adds more: Add… opens OpenRouter's list (`/models`, fetched without a key; only models
  that call tools), searchable, where a click adds or removes one; the row's buttons remove one.
  Added ones are kept as `or_models` in settings.json, as the list described them, and go to any
  provider that calls tools (no price ceiling unless free; no thinking or effort when the model has
  none). Free models share their provider's free quota with every OpenRouter user, so OpenRouter
  turns away a share of requests ("rate-limited upstream", HTTP 429) all day; such a rate limit,
  a paid provider's too, is retried every few seconds for up to five minutes (the status line names
  the model and provider) rather than with the usual four quick retries, and fails after that.
  `live_openrouter` (ignored) runs real sessions on each model. Titles come from OpenRouter's title model when there is
  a key, else from Haiku or the conversation's model. Rename (a conversation's tab or Home row
  right-click menu, or double-click its title in the session box; `src/rename.rs`) replaces the
  title, a title still being made included; the id, history and file stay (the file takes the
  new name). Subagents keep theirs, since sessions address them by it. Fork (a conversation's tab
  or Home row right-click menu; `src/fork.rs`) starts a new conversation with the transcript,
  summary, notes, marks, loaded kits and session memory of the one it came from, but none of its
  running work: no tasks, subagents, goal, heartbeats, meters, inbox, seat or usage totals. The
  fork gets its own title ("Fork: …"), id, scratchpad and tab, and waits for the user; the source
  carries on as it is. Its prefix's pin is let go, so its first request renders the system prompt
  for it rather than sending the source's (which named the source's scratchpad). Right-clicking a
  message or one of the model's items also offers Fork from here, which copies the conversation up
  to just before it, as Revert to before this goes back to, and leaves the source as it is. Files
  in the working folder are shared, as a side chat's; nothing links a fork to its source, so
  deleting either leaves the other.
- Rewrites (`src/rewrites.rs`): a plugin's rewriter (`Rewriter`; Unslop, a private plugin)
  has replies, and optionally thinking and notes, rewritten for the user; the model's context
  keeps its own text. Oak keeps the rewrites in their blocks (under the rewriter's key, which
  starts with `_oak_` and so is never sent), shows them and starts the unfinished ones again;
  the plugin says what is rewritten, picks thinking's sections and does the rewriting. A new
  epoch from the rewriter drops the notes' rewrites, so they are made again.
- Losing the GPU device ultrapauses the conversations at work and restarts Oak (`src/gpu.rs`).
  On Windows Oak sets up Direct3D 12 alone (the other backends cost a quarter of a second at
  every start); should that fail it starts again with every backend (`OAK_GPU_ALL_BACKENDS`).
  `WGPU_BACKEND` picks another.

**Chats** (`Conversation::chat`). The New screen's Chat, beside Local folder, starts a
conversation that is no project: no folder, role or goal, just a model and effort. Its system
prompt is `prompts/chat.md` whole (no `tools.md`, no project instructions or memory, no
scratchpad), and its tools are `tools::CHAT_TOOLS`: SEARCH, FETCH, ASK and NOTE. Every other tool
(files, EXECUTE, tasks, DISPLAY, SCREENSHOT, CD, heartbeats, progress, goals, LOAD and its kits, so
also subagents and the browser) is refused at run time (`tool_calls::chat_refuses`), whatever the
model was given or calls, so nothing reaches the user's files or runs on the machine. Goal
commands are dropped and Freebuff (whose agent works on a project) is neither offered nor sent a
chat (`agent::chat_refuses_provider`). It is saved with `"chat": true` (coding conversations, the
default, don't write it); its tab has a speech-bubble icon, its Home row says Chat, and its side
chat is a chat too. `oak_sdk`'s `go_new` takes `chat` to pick it.

**Code Mode** (`Conversation::code_mode`, experimental; `src/code_mode.rs`, `src/agent/repl.rs`).
The New screen's Code Mode starts a conversation in a folder whose one tool is REPL: the model
writes JavaScript, and every other tool it would have is an async function there (`await
READ({path})`, families by `action`), with helpers for the shell and files (`$` tagged templates,
`file`, `edit`, `write`, `log`, `searchTools`, `describeTool`), as Claude Code's Code Mode has
them; the declarations and the tool list join the system prompt from `src/code_mode/prompt.md`.
It runs in the user's own Node.js (18 or later: `OAK_NODE`, else PATH and the usual install
folders); without it the choice is greyed out with why, and a REPL call says Node.js is missing.
Each conversation gets one host process (`src/code_mode/host.js`, written to the temp folder and
run by node; JSON lines on stdin and stdout), started at its first REPL call: Node's REPL module
evaluating in a sealed `vm` context (no `require`, `process` or `import`), so declarations, top-level
await and the REPL's error wording are Node's own. A script's calls come back to Oak, which runs each
as the model's call would be run (`Worker::run_call`, so a subagent's limits, the command guard
and the transcript's file tracking apply; `$` goes through EXECUTE's checks and its output
streams under the REPL row), and the result lists them (`_oak_calls`). A call made directly rather
than from a script is refused (`tool_calls::code_mode_refuses`). A script stops after 10 minutes
not counting ASK waits; an interrupt or ultrapause stops it, and one that won't stop (a loop that
never awaits) has its host restarted. After each script the host saves the context's variables
(values, cycles, Map/Set/Date/RegExp/BigInt/typed arrays, functions and classes by their source,
instances of the script's classes, let/const kept as such) to `code-mode/<id>.json` in the data
folder. A host no script has used for 15 minutes is stopped; a new host (after that, after Oak
restarts, or when the host died) restores the snapshot before its first
script, and that script's result names what couldn't be kept (promises, symbols, closures'
hidden state). Freebuff isn't offered (it sends its own tools); subagents are ordinary; a side
chat starts from a copy of the snapshot; deleting the conversation deletes it. Saved as
`"code_mode": true`; its Home row says "folder, Code Mode".

**Super agents** (`agent::prompt::SUPER_ROLE`). The New screen's Super agent, beside Chat, starts a
conversation that coordinates: a name (its title, kept), a folder, model and effort, then Create or
a first message. Its role is `prompts/super.md` (not offered as a System prompt; `main` says it
plans the work and hands it to subagents with the AGENT kit, `agent` says its subagents each do one
task), saved as `"role": "super"`, so its subagents work in its folder on this machine with the
same prompt. Its tab has Oak's tree, its Home row says "folder, super agent", and `oak_sdk`'s
`go_new` takes `super` to pick it. It looks as the command center's super agents do (the Oak tab's
and Delta's), drawn by the one implementation of that look, `src/super_view.rs`, which plugins
use through `oak_sdk::ui::super_chat`, `super_box` and `super_agent_box` (interface version 3),
and `super_agent_box_act` (version 4: the agent box's model and effort pickers, Pause or Resume,
a detail line and what blocks it, and a switch waiting shown under the pickers):
`src/super_local.rs` makes a super agent's tab a text conversation (the user's messages in deep
blue bubbles on the right, what it says beside its avatar on the left, in stretches with the day
and time after a pause, three dots and its status while it works; its tool calls, thinking and
subagents' messages stay out of it, and its questions show under it), its corner box its name and
folder, what it is doing with Stop, its model and effort, and its subagents, each opening its tab,
and a subagent's corner box an agent's (its title, "Agent n, run by" the super agent, its state
with Stop and Pause or Resume, its model and effort pickers), each with notes and background tasks
under it when there are any. A subagent's model switches in place, from its pickers or the super
agent's AGENT action=set: its conversation and work carry on, a request under way finishes on the
model it started on (the box says it is switching until then, AGENT_LIST and set's reply too), and
the next request, with that one's tool results, goes to the new model, which isn't told of the
switch; a switch to another provider asks first, as the session box does.
Each session has a pulse (`src/agent/pulse.rs`), as the command center's workers do: its phase
from its status line (its model writing, its tools running, waiting on a retry, a usage limit, an
answer or its agents, summarizing; idle, paused) and since when, its last progress (its model's
reply or a tool's result going into the transcript, by tool name, never a command; kept with the
conversation as `progress_at`/`progress`, never sent) and what holds it up, only as known (the wait
its status line names, or the error its last turn ended on). A subagent's box shows it under its
state, its row on the super agent's list on hover, and AGENT_LIST in its state and last progress
columns, so "working" doesn't pass for progress. A collapsed "Local controls"
section keeps provider/model/effort switching, goals, progress meters, heartbeats, folder actions
and parent navigation available without crowding the shared layout. It still
runs here: its subagents are conversations of their own. Oak's own choices win over a plugin's `NewKind` of the same id,
as built-in roles do over plugin roles (`add_roles`), so a plugin can't replace either.

**Side chats** (`src/side_chat.rs`). The message bubble in the title bar, left of the window's buttons,
opens this conversation's side chat in a resizable pane on the right within Oak, with only its
transcript and message box. The usual attachment, pause and ultrapause buttons sit by the message box.
The first opening snapshots the saved conversation (including smart context, notes, the prefix as
sent and the scratchpad it names) and adds:
'The session was forked to be placed in a "side chat". This is only a chat for asking questions or requesting small changes, so do not continue work as you were before it was forked. Only make changes to the project if the user explicitly requests so.'
It waits for a question; the original keeps working independently. A side chat shares the working
folder, so any explicitly requested file changes are real, but it inherits no running tasks,
subagents, goal or heartbeats. Closing its pane retains it and its draft; Ctrl+W does the same
while the side chat has focus. The bubble reopens it, even after restarting Oak (drafts last only
this run, as in normal tabs). Its top bar has a Delete chat icon immediately beside the close X,
with confirmation; right-clicking the bubble also offers deletion. The side chat is an ordinary
persisted conversation linked to its source, not a subagent or a Home row; it never reports back. Deleting the source also deletes its side chat.

**Other parts of the window**: the transcript is a virtual list (`src/vlist.rs`) that lays out
only what is on screen (of a long reply, the parts near it; `src/markdown.rs`); after a local
conversation's transcript and session box are drawn, plugins draw over it (`Hooks::chat_scene`,
`App::chat_scene`: `chat_view::Scene` has the view clear of the scroll bar and minimap, the
session box's boxes, which `look::panel_boxes` collects, and the units on screen with their rows
of text, which `List::scene` reads from what they painted); Ctrl+F finds text
in it (`src/find.rs`); Ctrl+A after clicking or selecting text in a message selects the whole message, copied as it reads (`src/select.rs`); replies are markdown, with links
to files opening them. Resting the pointer on a chat's item that has a recorded time (`_oak_at`:
a message, a reply, an image or file, a tool's or thinking's row, a line Oak added; in a super
agent's chat each bubble, error row, time under a stretch and day line) shows its full date and
time (`transcript::time_tip`); an item from before Oak kept the time shows none, never a made-up
one. Its own controls' tooltips win over it. Right-clicking a chat's text (a transcript, a plugin's chat) offers
Copy for the selection, which the right-click leaves in place, Copy message, Copy link address over
a link, and Revert to before this where it applies (`src/copy_menu.rs`). Middle-clicking a
transcript or a plugin's chat scrolls it as browsers do (`src/autoscroll.rs`): the further the pointer goes above or
below the ring left where it was pressed, the faster; letting go after moving with the button held
ends it, and after a plain click it runs until the next click, Esc or the wheel. The clicks it
takes reach nothing else, so the text selection stays, and a middle click on a tab still closes
it. The browser (`src/browser.rs`, its view `src/browser_view.rs`) is a real Chromium page per
conversation that only the model opens and closes (the BROWSER kit's open and close, when the user
asks); there is no button for it. It fades in and out as it does, and while it is open a
see-through preview sits beside the transcript, between the column and the minimap (over the
transcript's corner when there is no room). A click on it, or Ctrl+Shift+B, expands it over the
window as an image opens, where the user uses the page; the X, a click beside it, Ctrl+Shift+B or
Esc go back. Its viewport is its own (1920x1080 CSS pixels until the model resizes it), scaled to
fit either way, and the model's pointer shows on it as Oak's green cursor. While the shown agent has
BLENDER loaded, its dedicated preview (`src/blender_view.rs`, using the interactive renderer in
`src/window_view.rs`) shows that agent's whole private Blender window under the browser preview.
Subagent conversations use the actual shown agent, not their parent. The preview stays with Blender
when WINDOW selects another app. A click expands it interactively; mouse, wheel and keys go through
Blender's simulated events, about nine frames a second. Only an explicit user Blender attachment
uses the viewport-only read-only renderer on 9876. A current non-Blender WINDOW app has its own
preview under those: each window has its own worker, frame cache and texture id, so a busy agent
cannot overwrite or stall another's image. Ordinary apps run about fifteen frames a second while
expanded. Real mouse and focus are untouched. Esc goes to the app unless the pointer is off the
picture; off-picture Esc, the X or a click beside it returns. When the
model asks for something of the user's (WINDOW or BROWSER action=attach), a Permission card at the
transcript's end (`src/approval.rs`) gives the model's reason and the user's windows or tabs, its
suggestion picked: Allow gives it the one picked, Decline refuses; every request is asked anew, the
status line says "Waiting for your permission", and a message from the user withdraws it. The
browser runs on the
conversation's own profile unless the model opens it on a named one (`open` with `profile`;
`browser-profiles/<name>`, names in lower case), which every conversation can open, one browser at
a time: a login made once serves them all, and another conversation's open on it is refused. Opening
with another `profile` switches (same viewport); open without one keeps the profile it is on. A
browser closed a moment ago is waited for before another starts on its profile. The model also
manages its state (`src/browser/state.rs`): cookies (listed; saved as JSON or a Netscape
cookies.txt that curl reads; loaded from those, Playwright's storage state or extension exports;
deleted), site data per origin, and overrides with `emulate` (user agent, locale, timezone, location,
color scheme, offline, extra headers, permissions), which a restart applies again. It reads the
network log, recorded from open (the newest 500 requests, with headers, posted data and bodies), and
the console, recorded only from its first `console` call because turning on DevTools' Runtime domain
is something anti-bot scripts detect. `attach` (with `why`, and `title` to pick one first) asks the
user for one of their own browser's tabs instead (Brave, Chrome or Edge with Oak's extension,
`assets/brave-extension`, loaded unpacked; its id is fixed by the manifest's `key`): Oak lists them
from the extension and, once one is allowed, works in it as in its own browser (which closes) until
`close`, which only lets go. The extension connects to `src/browser/bridge.rs` (a WebSocket server
on 127.0.0.1:47615 started with Oak, accepting only the extension's origin, its handshake's SHA-1
its own) and passes DevTools commands and events through `chrome.debugger`, so the browser shows
its debugging bar while Oak has the tab; each tab is a `Cdp` session over that link (`Cdp::over`,
`deliver`). Nothing of the user's is overridden: no launch, viewport (the tab's size is read before
each call), user agent, one-page script, downloads, file choosers or closing of other tabs, and
resize, emulate, the cookie actions and clear_data are refused. A tab closed, its debugging canceled
or the extension gone ends it, and the view offers to attach to the tab again. Home > Protocols'
Browser row says whether the extension is connected; its menu's Set up browser extension writes it
to Oak's folder (`browser-extension/`), opens that and copies its path for Load unpacked. The Usage page
(`src/usage_view.rs`) charts API-priced cost per 1% of the plan limits, from this machine's usage log
(`usage.jsonl`; a plugin can add the records other machines made, `oak_sdk::Host::usage_set_shared`). Every text field has a right-click menu (Cut,
Copy, Paste, Select all; `src/text_menu.rs`) that keeps the selection and the field's focus; a
masked field offers neither Cut nor Copy. A new field goes through it rather than `ui.add`. Up in
an empty message box brings back the last message sent from that tab (after a restart, the
conversation's last one), cursor at its end, to edit or send again; it sends nothing, and with text
in the box Up only moves the cursor.

**Testing GUI apps off-screen.** `oak --window` (Windows, `src/winctl.rs`) runs a program on a
hidden desktop and drives it without touching your screen: `"$OAK" --window launch -- app.exe`,
then `shot`, `record` (frames for a video), `click`, `type`, `key`, `close` (`"$OAK" --window help`
lists them); the WINDOW tool does the same inside Oak. The window there is never under the real cursor, so Windows tells the app the pointer
left after every move: `launch` sets `OAK_HIDDEN_DESKTOP` for the app, and Oak then ignores
egui's `PointerGone` (`raw_input_hook` in `src/lib.rs`) so hover and drags work; and every press,
release and wheel message follows a fresh move (`point_at`), as winit forgets the position at
each leave and would drop them. On Linux the
prompt points the agent at Xvfb, xdotool and ImageMagick instead.

## AWS Quick

The AWS Quick provider (`src/quick/`) is a provider of the Freebuff kind: a separate agent Oak
talks to as a pipe. Quick runs its own system prompt, agent loop and model (Claude Opus through
Bedrock; `turn_usage.resolved_model_id` says which), so Oak sends neither its prompt nor its
tools, and the picker's models are Quick's three modes (`smart`, `fast`, `balanced`, sent as
`agent_mode`), not model ids.

**Signing in** is Quick's own OIDC flow against AWS IAM Identity Center: Oak registers a public
client for itself (`/client/register`), copies a link to the authorization endpoint with PKCE
and `prompt=login`, takes the code on a loopback port, and exchanges it for an access token
(`aoa…`, an hour) and a refresh token (`aor…`). The refresh token is what Oak keeps in
`quick.json` (beside the client registration and the workspace's host); a request buys a fresh
access token with it when the one it has is about to expire.

**Accounts.** Quick has as many logins as were signed in (`quick.json` holds `logins`; a single
login from before is read as the first). Each has an id: its email, lowercased, else one made from
its client; signing the same account in again renews it in place. Each keeps this machine's outpost
for that account. A conversation's Quick thread lives on its login, so a conversation keeps its
login (`quick::login_for`, a seat in `seats.json` like any provider's) while that one can run, and
moves only when it can't (its session window or month used up, Quick saying it may not run, or its
renewal failing); with none that can, it stays and Quick says why. A conversation that moved is
sent all of itself with the request (`all_unseen`), as a switch from another provider is, since
the new login's thread has none of it. The turn's login is the thread's (`login::set_using`, which
its tool threads take with them). Home lists every login, with its month; Quick's provider box
averages them, and the account box shows the conversation's own.

**On a vault.** Connected to one, a sign-in goes to the vault (kind `quick`) with what renews it:
the client Oak registered and its secret, the Identity Center region or Gateway endpoint and
resource, the refresh token, and the workspace's host. The vault alone renews it from then on and
hands out access tokens, so every machine connected to it has the login; a vault that doesn't keep
Quick logins leaves it on this machine, saying so. A login on this machine moves there from its
card's menu. `OAK_QUICK_HOST` names the socket's host instead of the one the
sign-in's callback reports (a proxy or a stand-in, as `OAK_API_BASE` does for Claude). The sign-in's
callback doesn't always report the workspace, so Oak falls back to the cell Quick's desktop app
signs this identity store in to (`DEFAULT_CELL`), and the account box says so when neither is known.

**Tools are Oak's** — this is the part that differs from every other provider. Quick's agent asks
for a tool by name and waits on the socket for its result; Quick's own sandbox would run it
against copies under `/outposts/<device>/…`, not the user's folder, so Oak answers every call
itself, in the conversation's working folder, with the same tools it runs for other models
(`quick::turn`, `run_tool`). Quick's agent reaches for `run_python` (and its `file_*`, `fdfind`,
`ripgrep`), so those are mapped too. Its device Python (`run_python_outpost_local`, which Quick's
agent knows as `run_python_on_device`) runs Oak's own tools, not Python (`quick/calls.rs`): its
code is one or more Oak calls written as Python calls, a line each, with literal arguments only
(`READ(path="src/main.rs")`; a tool's required fields may also go first by position), and a
snippet with anything else is refused before any of it runs. Each call becomes a block of the
reply (`Turn::oak_call`) that the conversation's agent runs as it runs any model's calls, all at
once (those on a file one of them writes in order), so they show, stream and are tracked as Oak's
own; the turn waits for their results (`turn::run_oak_alive`, handed each by the batch through
`turn::ran`) and answers Quick with each one's, in order. The snippet's own call is no row of the
transcript (`quick::QUIET_KEY`, `a_quiet_call_is_not_shown`): the user sees only the Oak calls it
ran, unless it failed (a snippet Oak refused), when it shows with why. ASK can't be called that way (it waits for the reply's end, which the turn never
reaches while it waits). A snippet of a thread no conversation's agent reads runs here directly,
with only READ, WRITE, EDIT, EXECUTE, SEARCH and FETCH. Quick is told all this in the user message
(`oak_preface`): a thread's first message opens with Oak's briefing, once per thread: first Oak's
own system prompt as the request has it, notes included, in `<instructions_loaded>`, and its tool
definitions in `<tools_loaded>`, so Quick's agent takes them as what it was loaded with rather
than as the user's words, then the call syntax; later ones (mid-turn ones too) are the user's text alone. Quick keeps the
thread's history, so the briefing never goes again: not when the prompt or tools change (notes,
kits, the folder), not after Oak restarts (a thread Quick ran snippets in has had it), and the
`<system-reminder>` blocks a Generic-mode request adds of such changes are left out too
(`user_message_tells_quick_how_to_call_oaks_tools`); a summary's instruction goes alone.
That is why a Quick conversation works on the real project rather than on a copy of it.
Quick hands this machine's tool calls to every socket Oak has open to it, whatever thread asked
(a call names its thread only in its arguments, `_quickwork.offload_session_id`), so each turn
runs and shows only its own thread's calls: another conversation's is left to that
conversation's turn, and a call of a thread no turn reads is run once, by the first turn to see
it, and shown nowhere (`turn::owner`). Snippets run with `CREATE_NO_WINDOW`
(`turn::no_console`), as every console program Oak starts does, so neither they nor what they
start open a console on the user's desktop (`a_snippet_opens_no_console_window`). The transcript
shows a `run_python*` call's code, as Python (Quick's cloud `run_python`, and a failed snippet).

**The reply grows at its end**, as every other provider's does (`the_reply_grows_at_the_end`):
Quick sends a whole agent loop (thoughts, text, calls, more thoughts, the answer) as one message,
and text after a thought or a call is a new block below it, never more of the text above. Quick
opens only a message's first thought with `thinking_start`; each later one comes as deltas alone
and is a block of its own. Each `thinking_complete` carries all of the message's thinking so far,
so what follows the last of it is the newest thought whole, which stands for deltas that went
missing.
Quick's whole text (`text_complete`, `message_complete`) can likewise carry the message's earlier
text blocks run together before the block's own (`the_answer_does_not_repeat_the_steps_above_it`):
those are left out, so the answer doesn't repeat what was said between the calls.

**Quick's own calls are shown too** (`quicks_own_calls_are_shown`): Quick reports each call its agent
makes (`tool_start`, `tool_arguments`, `tool_result`), the ones it runs on its server (`web_search`,
`kg_search`, cloud `run_python`, `load_skill`) as well as the ones it hands Oak. A call Quick runs is a
tool block like any other, ending with what Quick says it returned, so Oak's agent never runs it; one
that has no result when the message ends ends saying so. A call Quick reports and hands Oak is one
block, whichever frame comes first, and ends with Oak's result, not Quick's record of it.
Quick's tools take the colors of the OAK tools that do the same work (`like_oaks` in `transcript.rs`:
its file reads READ's blue, Python EXECUTE's violet, the web SEARCH's and FETCH's cyans, its
knowledge and memory tools the notes' sage), as Freebuff's do, and their rows say what they work on,
with a device's paths as the device has them.

**Quick's markup is read, not shown** (`quick/markup.rs`, `a_decision_card_is_an_ask`). Quick's agent
writes for Quick's app: a choice it offers is a decision card (`<decision question="...">` with its
`<option>`s, an option's `description` attribute shown under it; several in `<decisions>`), and the
memories it used close the reply in `<sources>`. A tag counts only where it begins a line, outside a
code fence. Neither shows as text: while the text streams, what may begin one is held back until it
closes (`markup::split`), and at the reply's end what never closed is shown as it is. The cards'
questions become an ASK at the reply's end (call id `quick-ask-` and the message's id; the turn then
stops on `stop::TOOL_CALLS`, so Oak's loop runs it), and its answers are Quick's next message, as
Quick's app sends a card's: `<decision_response>` with a `Question: answer` line each
(`new_user_text`). The sources are dropped.

**A conversation is a Quick thread**: its conversation's id is the thread's, which is how Quick
keeps the history, so a request sends only its newest user turn (`user_message`) and Quick
replays the rest; a conversation another model worked on since Quick's last reply sends that
part too (`unseen_by`, above). A turn ends the way Quick's does: `stop::COMPLETE` says the reply's tool calls
are the record of work already done and answered, not work for Oak's loop to run again (the
loop's `batch.finish` would otherwise repeat every call Oak already answered). A goal
still carries such a turn on (`a_done_turn_carries_a_goal_on`): Quick's agent gets the goal as
its `<goal>` message, calls GOAL through a snippet like any Oak tool, and when its turn ends with
the goal unfinished, Oak's continue prompt is the next message it is sent.
Only what the user said since Quick's last reply is sent (`newest_user_text`): a request
in the middle of a turn (after a dropped stream, a pause, a retry or a usage limit) has only tool
results in its newest user turn, and it tells Quick to carry on (`CARRY_ON`) rather than read back
to an older user turn, which sent the user's first message again
(`a_request_mid_turn_does_not_send_the_first_message_again`). A summary sends its instruction.

**A message typed while Quick works goes into its turn**, as Quick's app sends one: a `user_message`
with `mid_turn: true` and a `message_id` of its own (`steer_message`), on the turn's socket, between
Quick's frames (`quick::turn::steer`, handed over by `steer_quick` while the request streams; it
stays queued, shown as sent to Quick). Quick's agent takes it in at its next step and says so
(`steering_ingested`, naming the id): the reply Oak was reading ends there (`stop::STEERED`, its
calls run and answered), the message joins the transcript after their results marked
`quick::STEERED_KEY`, and the next request reads on in Quick's same
message on the parked socket (`PARKED`, a minute at most), which starts a reply of its own
(`a_message_taken_in_mid_turn_splits_the_reply_and_nothing_repeats`). A message Quick didn't take in
before its turn ended goes with the next request, as before. `OAK_QUICK_STEER=off` turns it off.
The mark is one of Oak's own keys, which every request drops, so Quick's requests leave the marked
blocks out altogether (`for_provider`); else the next request took the message for new and sent it
mid-turn again, under a new id (`a_message_quick_took_in_is_left_out_of_its_requests`).

**The wire is Quick's**: client frames are binary WebSocket frames carrying
`{"eventType":"rawInputEvent","payload":{"payload":"<base64 JSON>"}}`, and answers are AWS
event-stream records whose body is the JSON the app reads (base64 again inside its `payload`).
The socket is Oak's own (`quick/socket.rs`, rustls, the frame code in `cdp.rs`); it sends no
permessage-deflate, and installs rustls' ring provider itself because reqwest's TLS brings a
second one.

**Usage** is read as Quick's desktop app reads it (`quick/usage.rs`): a POST of `{}` to the
workspace's `/integration/quick-work/user-info`, whose `usageSummary` has the month's units
(`monthlyUsage`: `availableUnits` of `provisionedUnits`, `usedPercentage`, `resetsAt` in seconds),
the session window (`sessionUsage`: `usedPercentage`, `resumeInMinutes`), the subscription
(`subscriptionType`, a free trial's `expiryDate`) and `entitlementStatus`. It is fetched with the
other providers' usage (every two minutes, and after a sign-in) and kept in memory, not in
`quick.json`. The account box (a Quick conversation's bottom right, and the login's card on Home)
names the plan ("AWS Quick, Lite trial"; the trial's end on hover) and shows the month as a plan's
limits show: "Month", the percent used and when it resets, over a bar that turns red at 90% or
when Quick says the account can't run, and on hover the units used, of how many, and left. The
session window shows under it while it is in use, when the box has room for it (not on Home's
card, which also counts conversations). A failed fetch keeps the last reading, marked "as of"
its time; with none, the box says usage is unavailable. Quick's provider box on Home shows the
month too, where it said "free".

`quick_live_turn` (ignored; needs `OAK_DIR` with `quick.json`, and a prompt with
`OAK_QUICK_PROMPT`) drives one real turn end to end, printing what Quick says.
`quick_models_are_listed` (ignored; `OAK_DIR` a profile) prints the picker's model list and fails
when Quick's models are missing from it.


## Cloud sessions

Cloud sessions (`src/cloud.rs`) are Claude Code sessions in Anthropic's cloud, on your Claude
accounts (the subscription logins on this machine, and a vault's when a plugin brings one): the
Cloud tab lists them, and + then Cloud starts one. Oak calls Anthropic's session API itself, and
only while sessions are on screen. Each account gets an environment named `OAK <hash>`. A session
takes pictures as attachments.

Plugins extend them two ways (docs/PLUGINS.md). A **session kit** (`oak_sdk::SessionKit`) gives
the sessions a server of the plugin's: a setup script and variables for their environment, a
block at the start of the first message (hidden in the transcript), a files box (any file to the
session, and files back from it) and **Move to another account** when one runs out. A **cloud
backend** (`oak_sdk::CloudBackend`) is another service with accounts of its own that sessions run
through, with its own tab on Home and its own choice on the New screen; a session whose backend
isn't loaded stays in its tab and says so.

## Plugins

[docs/PLUGINS.md](docs/PLUGINS.md): what a plugin can add, how Oak finds them, the interface and
how it stays stable, and how to write, build and test one. In short: a plugin is a dynamic
library that exports one C function, `oak_plugin`, and is handed a table of C functions
(`OakApi`) to register its parts and to draw with; Oak draws everything, the plugin says what.
The table only grows at its end and structs only at theirs, so a plugin built once runs in
every later Oak, and it can be written in any language (`sdk/include/oak_plugin.h`); `oak-sdk`
is the Rust wrapper, shaped like egui. Changing the interface: `sdk/src/abi.rs` only, at the
ends, then `OAK_WRITE_HEADER=1 OAK_WRITE_ABI=1 cargo test -p oak-sdk` (docs/PLUGINS.md).
