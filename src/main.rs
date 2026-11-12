//! The `oak` binary: everything is in the library (`lib.rs`), so plugins
//! can be built against the same crate graph (`docs/PLUGINS.md`).

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> eframe::Result {
    oak::main()
}
