//! What an Oak plugin is built against.
//!
//! A plugin is a dynamic library Oak loads when it starts (from the
//! `plugins` folder in Oak's folder, beside the executable, or the folders
//! `OAK_PLUGINS` names). It implements `Plugin`, hands Oak its parts
//! through the `Registrar` (tabs on Home, kinds of tab, cloud backends,
//! themes, fonts, roles, a vault for logins, sections of Settings, hooks),
//! and reaches Oak through `host()` (services any thread may use) and the
//! `App` it is given while it draws. `docs/PLUGINS.md` has the whole of it;
//! `plugins/hello` is the smallest one.
//!
//! ```ignore
//! use oak_sdk::{Info, Plugin, Registrar};
//!
//! #[derive(Default)]
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
//! oak_sdk::export_plugin!(Hello::default());
//! ```
//!
//! The plugin draws with its own copy of egui into Oak's window, so it must
//! be built exactly as Oak was (`abi`): the same compiler, `Cargo.lock`,
//! profile and patched crates. Oak checks, and names what differs.

pub use egui;
pub use serde_json;

pub mod abi;
pub mod chat;
pub mod cloud;
pub mod host;
pub mod plugin;
pub mod text_menu;
pub mod theme;
pub mod time;
pub mod ui;
pub mod usage;
pub mod widgets;

pub use abi::{has_host, host};
pub use host::{App, Host, Request, Response};
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
