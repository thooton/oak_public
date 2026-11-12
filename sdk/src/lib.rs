//! What an Oak plugin is built with.
//!
//! A plugin is a dynamic library Oak loads when it starts (from the
//! `plugins` folder in Oak's folder, beside the executable, or the folders
//! `OAK_PLUGINS` names). The interface between the two is plain C
//! (`abi`, and `include/oak_plugin.h`, which is written from it): a table
//! of functions that only grows at its end, so a plugin built once runs in
//! every later Oak, and can be written in any language that speaks C.
//!
//! The rest of this crate is that interface made comfortable in Rust: a
//! plugin implements `Plugin`, hands Oak its parts through the `Registrar`
//! (tabs on Home, kinds of tab, cloud backends, themes, fonts, roles, a
//! vault for logins, sections of Settings, hooks), draws with `egui` (the
//! names of the egui crate, over Oak's functions: Oak does the drawing),
//! and reaches Oak's services through `host()` and the `App` it is given
//! while it draws. `docs/PLUGINS.md` has the whole of it; `plugins/hello`
//! is the smallest one, `plugins/hello-c` the same in C.
//!
//! ```ignore
//! use oak_sdk::{Info, Plugin, Registrar};
//!
//! struct Hello;
//!
//! impl Plugin for Hello {
//!     fn info(&self) -> Info {
//!         Info::new("hello", "Hello", env!("CARGO_PKG_VERSION"), "Says hello on Home.")
//!     }
//!     fn register(&self, r: &mut Registrar) {
//!         r.home_tab(HelloTab);
//!     }
//! }
//!
//! oak_sdk::export_plugin!(Hello);
//! ```

pub use serde_json;

pub mod abi;
pub mod chat;
pub mod cloud;
pub mod egui;
pub mod host;
pub mod plugin;
pub mod rt;
pub mod text_menu;
pub mod theme;
pub mod time;
pub mod ui;
pub mod usage;
pub mod widgets;

pub use host::{App, Host, Request, Response, has_host, host};
pub use plugin::*;
pub use theme::Theme;

/// Now, in ms since 1970.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// "s" unless `n` is one.
pub fn plural<N: TryInto<u64>>(n: N) -> &'static str {
    if n.try_into().ok() == Some(1) { "" } else { "s" }
}

/// `v`'s hash (std's default hasher): a key for caches and change checks.
pub fn hash_of<T: std::hash::Hash + ?Sized>(v: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::hash::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}
