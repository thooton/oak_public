# Vendored, patched crates

Oak loads plugins as dynamic libraries. A plugin links its own copy of egui (and of everything
under it), and draws into the host's `egui::Context`. Two crates keep process-wide state in
statics that breaks when two copies of them work on the same objects, so Oak builds with patched
copies, through `[patch.crates-io]` in the workspace's `Cargo.toml`. A plugin's workspace has to
use the same two (`docs/PLUGINS.md`).

Both are the crates.io sources of the version named, changed only where a comment says
"Oak patch". To move to a newer version: copy the new source over, apply the same changes, and
run `cargo test -p oak-sdk` and the plugin tests.

## parking_lot_core 0.9.12

`src/parking_lot.rs`, `src/lib.rs`. Upstream keeps the hash table of parked threads in a static.
With one table per copy, a thread that one copy parked on a lock is never woken by another
copy's unlock of that lock: a worker thread asking for a repaint while a plugin draws would
sleep forever. The table and its thread count are in a `ParkingState` now, and
`oak_adopt_parking_state` points a copy at another's (`oak_parking_state`). The host's is the
one every plugin adopts, before it does anything else (`oak_sdk::abi`).

On Windows this relies on `WaitOnAddress` (Windows 8 and later), which parking_lot_core uses
when it is there; its keyed-event fallback for older Windows is per copy.

## epaint 0.37.0

`src/text/face_store.rs`, `src/lib.rs`. A font face's key came from a static counter, so the
first face a plugin's copy parsed (a system font found for a character the built-in fonts
lack) got the key of the host's first face and replaced it. The counter's address is hashed
into the key. `OAK_PATCHED` marks the patched crate; `oak-sdk` names it, so building a plugin
against the unpatched crate fails.

## What was checked

Statics and thread-locals in egui, epaint, emath and ecolor 0.37.0 (the crates whose values
cross between the host and a plugin): besides the two above, only debug name maps
(`Id`, `IdSalt`: per copy, used for debug output) and the immediate-viewport renderer
(set by eframe in the host; plugins don't open viewports). `scripts/audit-statics.sh` lists
them again for a new version.
