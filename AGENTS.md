# Oak

A coding agent for Windows and Linux, written in Rust with [egui](https://github.com/emilk/egui).
Each conversation works in a folder you choose, with a model from one of Oak's providers: Claude
(a subscription login or an API key), the GPT models a ChatGPT plan includes for Codex,
OpenRouter, NodeClub, Freebuff, Refuseless (abliterated GLM 5.3 Flash and DeepSeek V4.1 Flash), or a
llama.cpp server of your own. Cloud sessions run Claude Code in Anthropic's cloud on your Claude accounts.

Oak is one executable. It loads plugins when it starts: dynamic libraries that add tabs, cloud
backends, themes, fonts, system prompts and more, through a stable C interface
([docs/PLUGINS.md](docs/PLUGINS.md)).

The repository:

- `src/`: the app. Every file opens with a comment saying what it holds; `lib.rs` has the app's
  state, its screens and the frame loop (`main.rs` only calls it).
- `sdk/`: the plugin interface (`sdk/src/abi.rs`, and the C header written from it,
  `sdk/include/oak_plugin.h`) and `oak-sdk`, its Rust wrapper. `plugins/hello` and
  `plugins/hello-c`: example plugins in Rust and in C.
- `prompts/`: the system prompt, compiled in. `assets/`: fonts, logos and pictures (with the
  scripts that make some of them). `linux/`: the self-contained Linux bundle. `scripts/`: checks.

This file is the description of how Oak behaves that QA checks the app against: keep it true
with every change, and put here the decisions the code alone doesn't explain. How a part works
belongs in its file's opening comment.

## Build, run and test

You need a recent stable Rust (edition 2024). Oak runs on Windows 10/11 with
[Git for Windows](https://gitforwindows.org/) (commands run in Git Bash) and on Linux with X11 or
Wayland (commands run in `bash`). Optional, for the agent's tools: Chrome, Chromium, Brave or Edge
(FETCH `render`, SCREENSHOT, the browser pane), and on Linux Xvfb, xdotool and ImageMagick
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
`oak --no-plugins` starts without any.

Ignored tests, run with `cargo test [--release] <name> -- --ignored --nocapture`:

- Benchmarks, at no cost: `bench_frames`, `bench_between_turns` (frames idle, scrolling and with
  a reply streaming in, startup, picture memory; `BENCH_TURNS` sets the long conversation's length),
  `bench_streaming_reply` (a 40 kB reply streaming in at the end of a long conversation),
  `bench_transcript_steps`, `bench_request_messages`, and `bench_tool_turnaround` (what a
  conversation adds around EXECUTE, READ and WRITE calls, against the fake model of
  `bench/tools/` in the private repository, beside Claude Code's and the command center's; set
  `OAK_API_BASE` and a scratch `OAK_DIR`). Build them with `--release`.
- Against the real services, signed in (a small cost): `live_rolling`, `live_images`,
  `live_agents`, `live_freebuff` (`OAK_TEST_PROMPT`, `OAK_TEST_FOLDER`) and `live_cloud_list`
  (read only), `live_rewrite` and `live_search`. `live_hidden_desktop` (Windows, after `cargo build`)
  opens Oak on a hidden desktop.
- Whole-app tests that open a window: `whole_app_close_with_a_task`, `whole_app_tab_shortcuts`,
  `whole_app_pane_keeps_its_width`. `upgrade_real_conversations` loads a copy of real
  conversations (`OAK_UPGRADE_FROM`).
- Plugins, headless, each alone (plugins load once per process):
  `the_example_plugins_draw_in_the_window` (after `cargo build -p oak-hello`, with
  `OAK_PLUGINS=target/debug`; it compiles the C example itself) and `every_plugin_draws`
  (whatever `OAK_PLUGINS` holds); docs/PLUGINS.md has both. The interface's own tests are in
  `sdk/src/abi.rs` (the header and the record of each version are current, and no version
  breaks an earlier one) and `src/plugins/api.rs` (the SDK against Oak's table, in process).

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
named after its folder and title, with its id last. Beside it: `settings.json`, `plugins.json` (the plugins switched off), `models.json`, the
login files (`accounts.json`, `codex.json`, `openrouter.json`, `nodeclub.json`,
`refuseless.json`, `freebuff.json`, `freebuff-device.json`, `freebuff-device-key.json`, `local.json`;
owner-only on Linux), `window.json` (the window's
size, place and maximized state), `seats.json` (which account each
conversation is on), `tasks/` and `task-output/` (background tasks), `browser/` (the browser pane's
profiles), `plugins/` (plugins to load, and what they keep is beside it under names of their
own), `bin/` (programs for the agent's commands: the bundled ripgrep, `src/toolbin.rs`),
`usage.jsonl` and `crash.log`. Files are written to a temporary file and renamed over
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
(Alt+H): its left side has the tabs Conversations and Cloud, then those plugins add, then
Plugins (and at the row's right end the links plugins add), its right side the providers' accounts, a usage preview and the version. The Conversations tab's search field (Ctrl+F) narrows the list to
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
transcript with a minimap, the session box (title, folder, provider, model, effort, goal, notes,
agents, progress meters) and the message box with the smart context, Unslop and account boxes.
A click on the session box's folder opens it in the system's file manager; its right-click menu
has Open folder and Copy path (a folder that no longer exists can only be copied).
Settings is the gear in the bottom left corner; its Theme and Font rows show once a plugin adds a
choice to Oak's own (the Oak theme, IBM Plex Sans). Plugins are managed on Home, not in Settings.
Home's Plugins tab (`src/plugins_view.rs`), like a game's mod manager, lists every library found
with its name, version and description (or why it isn't running) and a switch; a click on a row
flips it, and Enable all, Disable all and Open folder (Oak's `plugins` folder) sit above the list.
Switches are saved by plugin id in `plugins.json` and apply at the next start, since plugins load
only then: a changed row says "On after restart" or "Off after restart", and Restart to apply
restarts Oak as an update's restart does. Switching a plugin on switches on the plugins it needs;
one whose needs are switched off says so. A switched-off plugin's library is opened to read who it
is but never started.
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
  its 5-hour and weekly limits below 100% (a reset used here or anywhere else), and waiting
  sessions try again then. Settings can turn rotation off. The model is never told.
- A Claude account's plan is read from its profile: Pro, Max, Ultra, Team Standard or Team
  Premium. Max and Ultra are the two sizes of Max (rate-limit tiers `..._max_5x` and `..._max_20x`);
  accounts, the usage log and vaults keep them as "Max 5x" and "Max 20x", the names they had
  before, and only what Oak shows says Max and Ultra (`api::plan_label`).
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
- Stable prefix (Settings, on by default; `src/prefix.rs`): the system prompt and tool list stay
  byte for byte as cached; what changes meanwhile (the date, a kit loaded, effort) reaches the model
  after the latest user turn, and the prefix is rebuilt only when the cache is cold anyway.
- A local llama.cpp server (`src/local.rs`) gets only the basic tools and the `{{#local}}` parts of
  the prompt, and is never offered to subagents (it serves one request at a time).

**The agent** (`src/agent.rs`, `src/agent/`). One worker thread per conversation. Its tools
(`src/tools/defs.rs`): READ, WRITE, EDIT, EXECUTE, SEARCH, FETCH, SCREENSHOT, DISPLAY, ASK,
REQUEST_ASK, ACCOUNTS, LOAD, and TASK, NOTE, HEARTBEAT, PROGRESS and GOAL, which each take an
`action` and show as tools of their own (`TASK action=wait` is TASK_WAIT). LOAD loads a kit (AGENT,
BROWSER, WINDOW): a tool and its instructions, left out of the prompt until needed.

- The system prompt is `prompts/<role>.md` (who the model is; every file but `tools.md` and
  `chat.md` is a role offered on the New screen, `super.md` only as its Super agent; plugins can add roles) with `prompts/tools.md` in place of `{{tools}}`. `{{folder}}`,
  `{{scratch}}`, `{{date}}`, `{{os}}`, `{{model}}` and `{{agent}}` are filled in, `{{#windows}}`,
  `{{#unix}}`, `{{#main}}`, `{{#agent}}`, `{{#full}}`, `{{#local}}`, `{{#goal}}` and kit blocks are kept only where they apply, and
  comments are dropped (`build.rs`, `src/agent/prompt.rs`). It is compiled in: a change takes a
  rebuild. It calls the model Oak, whichever lab made it.
- A reply's tool calls run in parallel, each as soon as it has streamed in, waiting only for calls
  on a file it touches, and for ASK and CD, which run on their own (`src/agent/tool_calls.rs`).
- CD switches the conversation's working folder for the rest of the session (relative paths,
  EXECUTE and new tasks; running tasks keep theirs). The scratchpad stays where it was (its name
  becomes its full path), the result carries the new folder's project instructions, and the
  system prompt keeps the folder its cache was written with (`prefix::Pin::folder`).
- EXECUTE takes a timeout (0 to 550 seconds, 0 for the background) and a one-line summary shown in
  place of the command. Longer commands become background tasks (`src/tasks.rs`), kept across
  restarts; on closing, Oak can leave them running (on Windows each one's bash is handed a handle
  to its own job, so no Oak process stays behind to lock oak.exe; the next Oak takes it back). Every
  command's PATH starts with Oak's `bin/` folder, which holds the ripgrep bundled at build time
  (`src/toolbin.rs`; written there on a run's first command when missing or different), and the
  prompt tells the model to search with `rg` instead of grep. Output over 50,000 characters is saved to the
  scratchpad. Before a command runs, `src/guard.rs` refuses likely-destructive ones (recursive
  removals of the home folder, the working folder or `.git`, disk commands, git commands that
  discard work, killing Oak) and `src/sleeps.rs` refuses sleeps over 5 seconds; the error says
  what to do instead. On Linux each command leads its own process group, killed with it.
- Smart context (`src/rolling.rs`): past a trigger (120k tokens by default) the older part is
  summarized in the background, reusing the prompt cache, and the newest ~40k stay verbatim. The
  transcript only grows: a banner marks where a summary was made and a line where its part ends.
  Old tool output is replaced by a placeholder in requests, and a line marks where that stops.
- Goals: the model keeps working until it calls GOAL finish; "pause goal" stops Oak asking it to
  continue without telling it. While a goal is set, ASK needs the user's yes through REQUEST_ASK.
  An endless goal (the Endless box when editing it) refuses GOAL justify and finish, so it lasts
  until the user pauses or clears it; the model is told in the goal's message and its memory block,
  and changing only the mode sends the goal again.
- Notes (sent with every request, so they outlive summaries), heartbeats, progress meters (a
  shell command whose output's last line is `x/y`, `N%` or a number; `src/progress.rs`) and
  subagents (`src/agent/subagents.rs`; sessions message each other, and a subagent's final reply
  reaches the main session by itself).
- Files changed on disk by someone else are reported to the model before its next request
  (`src/changes.rs`). Instruction files (CLAUDE.md, AGENTS.md) and project memory are off for now
  (`src/instructions.rs`).
- Messages sent while Oak works are queued for its next request. Pause lets the request and
  commands under way finish; ultrapause stops everything at once. A pause survives a restart. Reverting
  (a message's icon, or right-click inside a reply) takes the conversation back, not the files.
- A safety classifier's refusal is retried as failed requests are, then every minute and a half;
  there is no fallback to another model. Titles come from OpenRouter's title model when there is
  a key, else from Haiku or the conversation's model. Rename (a conversation's tab or Home row
  right-click menu, or double-click its title in the session box; `src/rename.rs`) replaces the
  title, a title still being made included; the id, history and file stay (the file takes the
  new name). Subagents keep theirs, since sessions address them by it.
- Unslop (`src/unslop.rs`): replies, and optionally thinking and notes, are rewritten for the
  user by Gemma 4 31B through OpenRouter; the model's context keeps its own text.
- Losing the GPU device ultrapauses the conversations at work and restarts Oak (`src/gpu.rs`).
  On Windows Oak prefers Direct3D 12; `WGPU_BACKEND` picks another.

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

**Super agents** (`agent::prompt::SUPER_ROLE`). The New screen's Super agent, beside Chat, starts a
conversation that coordinates: a name (its title, kept), a folder, model and effort, then Create or
a first message. Its role is `prompts/super.md` (not offered as a System prompt; `main` says it
plans the work and hands it to subagents with the AGENT kit, `agent` says its subagents each do one
task), saved as `"role": "super"`, so its subagents work in its folder on this machine with the
same prompt. Its tab has Oak's tree, its Home row says "folder, super agent", and `oak_sdk`'s
`go_new` takes `super` to pick it. Oak's own choices win over a plugin's `NewKind` of the same id,
as built-in roles do over plugin roles (`add_roles`), so a plugin can't replace either.

**Side chats** (`src/side_chat.rs`). The message bubble immediately left of the browser icon opens
this conversation's side chat from the right within Oak, with only its transcript and message box.
It shares the browser's resizable pane; when both are open, the pane's tabs switch between them
without stopping either. The usual attachment, pause and ultrapause buttons sit by the message box.
The first opening snapshots the saved conversation (including smart context and notes) and adds:
'The session was forked to be placed in a "side chat". This is only a chat for asking questions or requesting small changes, so do not continue work as you were before it was forked. Only make changes to the project if the user explicitly requests so.'
It waits for a question; the original keeps working independently. A side chat shares the working
folder, so any explicitly requested file changes are real, but it inherits no running tasks,
subagents, goal or heartbeats. Closing its pane retains it and its draft; Ctrl+W does the same
while the side chat has focus. The bubble reopens it, even after restarting Oak (drafts last only
this run, as in normal tabs). Its top bar has a Delete chat icon immediately beside the close X,
with confirmation; right-clicking the bubble also offers deletion. The side chat is an ordinary
persisted conversation linked to its source, not a subagent or a Home row; it never reports back. Deleting the source also deletes its side chat.

**Other parts of the window**: the transcript is a virtual list (`src/vlist.rs`) that lays out
only what is on screen (of a long reply, the parts near it; `src/markdown.rs`); Ctrl+F finds text
in it (`src/find.rs`); Ctrl+A after clicking or selecting text in a message selects the whole message, copied as it reads (`src/select.rs`); replies are markdown, with links
to files opening them. Right-clicking a chat's text (a transcript, a plugin's chat) offers
Copy for the selection, which the right-click leaves in place, Copy message, Copy link address over
a link, and Revert to before this where it applies (`src/copy_menu.rs`). Middle-clicking a
transcript or a plugin's chat scrolls it as browsers do (`src/autoscroll.rs`): the further the pointer goes above or
below the ring left where it was pressed, the faster; letting go after moving with the button held
ends it, and after a plain click it runs until the next click, Esc or the wheel. The clicks it
takes reach nothing else, so the text selection stays, and a middle click on a tab still closes
it. The browser pane
(Ctrl+Shift+B, `src/browser.rs`) is a real Chromium page per
conversation that the user and the model (the BROWSER kit, when the user asks) both use. The Usage page
(`src/usage_view.rs`) charts API-priced cost per 1% of the plan limits, from this machine's usage log
(`usage.jsonl`; a plugin can add the records other machines made, `oak_sdk::Host::usage_set_shared`). Every text field has a right-click menu (Cut,
Copy, Paste, Select all; `src/text_menu.rs`) that keeps the selection and the field's focus; a
masked field offers neither Cut nor Copy. A new field goes through it rather than `ui.add`. Up in
an empty message box brings back the last message sent from that tab (after a restart, the
conversation's last one), cursor at its end, to edit or send again; it sends nothing, and with text
in the box Up only moves the cursor.

**Testing GUI apps off-screen.** `oak --window` (Windows, `src/winctl.rs`) runs a program on a
hidden desktop and drives it without touching your screen: `"$OAK" --window launch -- app.exe`,
then `shot`, `click`, `type`, `key`, `close` (`"$OAK" --window help` lists them). On Linux the
prompt points the agent at Xvfb, xdotool and ImageMagick instead.

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
