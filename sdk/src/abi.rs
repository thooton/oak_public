//! How a plugin's library and Oak find out whether they fit, and how the
//! plugin starts.
//!
//! A plugin is a dynamic library (`.so`, `.dll`) with one exported function,
//! `oak_plugin_entry`, which `export_plugin!` writes. It returns an `Entry`:
//! a `#[repr(C)]` block (so it reads the same whatever built it) with the
//! plugin's `Stamp` and its `init` function. Oak compares the stamp with its
//! own and calls `init` only when every part matches; a library that
//! doesn't fit is left alone and named on Settings with the part that
//! differs.
//!
//! Why so strict: past `init`, the two sides pass Rust values to each other
//! (`egui::Ui`, trait objects, strings), and Rust promises nothing about
//! how those are laid out between two builds. They are laid out the same
//! when the same compiler built the same crates with the same features and
//! options, and that is what the stamp checks:
//!
//! - `api`: `API_VERSION`, raised by hand when the interface changes;
//! - `source`: a digest of this crate's source and of the patched crates
//!   (`build.rs`), so both sides were built from the same definitions;
//! - `types`: the `TypeId`s of the types that cross. A `TypeId` comes from
//!   the type's path and its crate's identity, which cargo derives from the
//!   crate's version, features, dependencies, profile and compiler: two
//!   builds agree on it only when all of those agree;
//! - `layout`: the sizes and alignments of the same types;
//! - `hashing`: what egui's id hasher makes of a fixed value (it differs
//!   between CPU feature sets, and ids hashed by one side are looked up by
//!   the other);
//! - `debug`: whether debug assertions are on (egui's `Style` has a field
//!   only then);
//! - `rustc`, `target`: for the message, and against a compiler that
//!   happens to agree on the rest.
//!
//! A plugin's panic never unwinds into Oak: every call Oak makes into a
//! plugin goes through a guard written here and so compiled into the
//! plugin, which catches it (`guard`), tells the host, and from then on
//! answers for the plugin with nothing. Oak's own panics pass through a
//! plugin's frames as they pass through Oak's.
//!
//! Plugins are never unloaded: what they registered, the values they left
//! in egui's memory and their threads all point into the library.

use crate::egui;
use crate::host::Host;
use crate::plugin::Plugin;
use std::any::TypeId;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The interface's version: raised whenever a trait, a struct or a function
/// a plugin can name changes in a way an older plugin wouldn't survive.
/// (`Stamp::source` catches any change at all; this one is for the message.)
pub const API_VERSION: u32 = 1;

/// The function every plugin library exports.
pub const ENTRY_SYMBOL: &[u8] = b"oak_plugin_entry\0";

/// `Entry::magic`.
pub const MAGIC: [u8; 8] = *b"OAKPLUG\0";

/// What a library was built with. `#[repr(C)]` and made of plain numbers
/// and bytes: it is read before anything is known about the library.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub api: u32,
    /// 1 with debug assertions on.
    pub debug: u32,
    pub source: u64,
    pub types: u64,
    pub layout: u64,
    pub hashing: u64,
    /// `rustc -V`, the SDK's version and the target, each ended by a 0.
    pub rustc: [u8; 96],
    pub sdk: [u8; 32],
    pub target: [u8; 64],
}

/// FNV-1a: the same numbers from the same bytes in every build.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Adds type `T` to the two digests: its `TypeId`, and its size and
/// alignment.
fn probe<T: ?Sized + 'static>(types: &mut Fnv, layout: &mut Fnv, size: usize, align: usize) {
    use std::hash::{Hash, Hasher};
    TypeId::of::<T>().hash(types);
    layout.write_usize(size);
    layout.write_usize(align);
}

macro_rules! probes {
    ($types:ident, $layout:ident; $($t:ty),* $(,)?) => {
        $(probe::<$t>(&mut $types, &mut $layout, size_of::<$t>(), align_of::<$t>());)*
    };
}

fn text<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let n = s.len().min(N - 1);
    out[..n].copy_from_slice(&s.as_bytes()[..n]);
    out
}

fn read(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

impl Stamp {
    /// The stamp of the build this code is in: Oak's when Oak calls it, a
    /// plugin's when the plugin does.
    pub fn current() -> Stamp {
        use std::hash::Hasher;
        let (mut types, mut layout) = (Fnv::new(), Fnv::new());
        // What crosses between Oak and a plugin, and what those are made of.
        probes!(types, layout;
            egui::Ui, egui::Context, egui::Response, egui::Painter, egui::Style, egui::Visuals,
            egui::Memory, egui::InputState, egui::RawInput, egui::Id, egui::LayerId, egui::Sense,
            egui::TextEdit<'static>, egui::RichText, egui::WidgetText, egui::FontId, egui::Frame,
            egui::TextureHandle, egui::ColorImage, egui::Shape, egui::Mesh, egui::Galley,
            egui::text::LayoutJob, egui::Rect, egui::Pos2, egui::Vec2, egui::Color32, egui::Stroke,
            egui::Modal, egui::Area, egui::ScrollArea, egui::epaint::mutex::Mutex<u8>,
            serde_json::Value, serde_json::Map<String, serde_json::Value>,
            String, Vec<u8>, std::path::PathBuf, std::time::Instant, std::time::Duration,
            std::sync::Arc<std::sync::Mutex<u8>>, Box<dyn std::any::Any + Send>,
            std::collections::HashMap<String, String>,
            crate::theme::Theme, crate::plugin::Info, crate::host::Response, crate::host::Request, crate::host::NewConversation,
            crate::usage::Rec, crate::cloud::Account, crate::cloud::Call, crate::chat::Chat,
            crate::plugin::Parts, crate::plugin::Registrar, crate::plugin::TabLook, crate::plugin::Role, crate::plugin::Tags,
        );
        // The trait objects: their `TypeId`s only (they have no size).
        {
            use std::hash::Hash;
            TypeId::of::<dyn Host>().hash(&mut types);
            TypeId::of::<dyn Plugin>().hash(&mut types);
            TypeId::of::<dyn crate::host::App>().hash(&mut types);
        }
        // The patched crates (`vendor/README.md`): naming these fails to
        // build against the unpatched ones.
        let _: bool = egui::epaint::OAK_PATCHED;
        let _: fn() -> *const () = parking_lot_core::oak_parking_state;
        let mut hashing = Fnv::new();
        hashing.write_u64(egui::Id::unique("oak-plugin-abi").value());
        hashing.write_u64(egui::Id::unique(("oak", 7u32, 0.5f32.to_bits())).value());
        Stamp {
            api: API_VERSION,
            debug: cfg!(debug_assertions) as u32,
            source: u64::from_str_radix(env!("OAK_SDK_SOURCE"), 16).unwrap_or(0),
            types: types.finish(),
            layout: layout.finish(),
            hashing: hashing.finish(),
            rustc: text(env!("OAK_SDK_RUSTC")),
            sdk: text(env!("CARGO_PKG_VERSION")),
            target: text(env!("OAK_SDK_TARGET")),
        }
    }

    pub fn rustc(&self) -> String {
        read(&self.rustc)
    }

    pub fn sdk(&self) -> String {
        read(&self.sdk)
    }

    pub fn target(&self) -> String {
        read(&self.target)
    }

    /// Why a library stamped `plugin` doesn't fit a host stamped `self`:
    /// the first part that differs, said so that it can be acted on. None
    /// when they fit.
    pub fn mismatch(&self, plugin: &Stamp) -> Option<String> {
        let host = self;
        if host == plugin {
            return None;
        }
        Some(if host.target != plugin.target {
            format!("it was built for {}, and this Oak for {}", plugin.target(), host.target())
        } else if host.rustc != plugin.rustc {
            format!("it was built with {}, and this Oak with {}; build both with the same compiler", plugin.rustc(), host.rustc())
        } else if host.api != plugin.api {
            format!("it uses plugin interface {} (oak-sdk {}), and this Oak has interface {} (oak-sdk {})", plugin.api, plugin.sdk(), host.api, host.sdk())
        } else if host.source != plugin.source {
            format!("it was built against another version of oak-sdk ({}, this Oak has {}); build it against this Oak's source", plugin.sdk(), host.sdk())
        } else if host.debug != plugin.debug {
            let kind = |d: u32| if d == 1 { "a debug build" } else { "a release build" };
            format!("it is {}, and this Oak is {}; build both with the same profile", kind(plugin.debug), kind(host.debug))
        } else if host.types != plugin.types {
            "its copy of egui or of another shared crate isn't the one this Oak has (another version, other features, another profile, or the patched crates under another path); build it with Oak's Cargo.lock, profiles and patches".to_string()
        } else if host.layout != plugin.layout {
            "the types it shares with Oak are laid out differently; build it with Oak's Cargo.lock, profiles and compiler options".to_string()
        } else {
            "its hashing differs from this Oak's (other target CPU features); build both with the same RUSTFLAGS".to_string()
        })
    }
}

/// What `oak_plugin_entry` returns: the library's stamp, and how to start
/// it. `#[repr(C)]`; `size` and `magic` say it is one of these at all.
#[repr(C)]
pub struct Entry {
    pub magic: [u8; 8],
    /// `size_of::<Entry>()` as the library sees it.
    pub size: u32,
    pub stamp: Stamp,
    /// Starts the plugin. Only for a host whose stamp is `stamp`.
    pub init: unsafe extern "C" fn(link: *const Link, out: *mut Loaded) -> bool,
}

/// What Oak hands a plugin at `init`.
pub struct Link {
    pub host: &'static dyn Host,
    /// The host's `parking_lot_core::oak_parking_state()`.
    pub parking: *const (),
}

/// What `init` hands back.
#[derive(Default)]
pub struct Loaded {
    pub plugin: Option<Box<dyn Plugin>>,
    pub error: String,
}

static HOST: OnceLock<&'static dyn Host> = OnceLock::new();

/// The host, from inside a plugin (or from Oak itself, which sets it at
/// startup). Panics before `init`: there is no host to ask yet.
pub fn host() -> &'static dyn Host {
    *HOST.get().expect("oak_sdk::host() before the plugin was started")
}

/// Whether there is a host (false in a plugin's own unit tests).
pub fn has_host() -> bool {
    HOST.get().is_some()
}

/// Makes `host` the one `host()` gives. Oak calls it for itself; a plugin
/// gets its host through `start`.
pub fn set_host(host: &'static dyn Host) {
    let _ = HOST.set(host);
}

/// A plugin's `Entry`, made once. `export_plugin!` calls it with the
/// function that starts the plugin.
pub fn entry(init: unsafe extern "C" fn(*const Link, *mut Loaded) -> bool) -> *const Entry {
    static ENTRY: OnceLock<Entry> = OnceLock::new();
    ENTRY.get_or_init(|| Entry { magic: MAGIC, size: size_of::<Entry>() as u32, stamp: Stamp::current(), init })
}

/// Starts a plugin: what its `init` does, with `make` building it.
///
/// # Safety
///
/// `link` and `out` must be valid, and come from a host whose stamp equals
/// this library's.
pub unsafe fn start(link: *const Link, out: *mut Loaded, make: fn() -> Box<dyn Plugin>) -> bool {
    // SAFETY: the caller's.
    let (link, out) = unsafe { (&*link, &mut *out) };
    // Before anything here can wait on a lock: the host's table of parked
    // threads, so locks the two sides share wake each other's threads.
    // SAFETY: the stamps matched, so the host's parking_lot_core is this one.
    if !unsafe { parking_lot_core::oak_adopt_parking_state(link.parking) } {
        out.error = "it had already used its own locks when Oak started it".to_string();
        return false;
    }
    set_host(link.host);
    // This library's panics, in Oak's crash log (its std is its own, and
    // so is its panic hook).
    std::panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("(not a string)");
        let at = info.location().map_or("an unknown place".to_string(), |l| l.to_string());
        let text = format!("a plugin panicked at {at}:\n{message}\n\n{}", std::backtrace::Backtrace::force_capture());
        if let Some(h) = HOST.get() {
            h.log_crash(&text);
        }
    }));
    match std::panic::catch_unwind(make) {
        Ok(plugin) => {
            out.plugin = Some(Box::new(crate::plugin::Guarded::new(plugin)));
            true
        }
        Err(e) => {
            out.error = format!("it panicked while starting: {}", panic_text(e.as_ref()));
            false
        }
    }
}

/// What a caught panic said.
pub fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_else(|| "(no message)".to_string())
}

/// Whether one of a plugin's parts has panicked: from then on Oak gets
/// nothing from it (`guard`). One per plugin, shared by all it registered.
#[derive(Default)]
pub struct Health {
    dead: AtomicBool,
    name: OnceLock<String>,
}

impl Health {
    pub fn set_name(&self, name: &str) {
        let _ = self.name.set(name.to_string());
    }

    pub fn dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
}

/// Runs `f`, a call into a plugin's own code, so that its panic stops here:
/// the host is told (once), the plugin is marked dead, and `default`
/// answers this call and every later one.
pub fn guard<R>(health: &Health, default: impl FnOnce() -> R, f: impl FnOnce() -> R) -> R {
    if health.dead() {
        return default();
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => {
            if !health.dead.swap(true, Ordering::Relaxed)
                && let Some(h) = HOST.get()
            {
                h.plugin_failed(health.name.get().map_or("", |s| s.as_str()), &panic_text(e.as_ref()));
            }
            default()
        }
    }
}

/// Exports a plugin from its crate: `export_plugin!(MyPlugin::default())`,
/// with any expression that makes the plugin.
///
/// The crate must be a `cdylib`, and depend on `oak-sdk` and on `oak`
/// itself: the second is never used in code, and makes cargo build the
/// plugin's copies of the shared crates exactly as it builds Oak's (the
/// macro names it, so leaving it out doesn't build).
#[macro_export]
macro_rules! export_plugin {
    ($make:expr) => {
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn oak_plugin_entry() -> *const $crate::abi::Entry {
            unsafe extern "C" fn init(link: *const $crate::abi::Link, out: *mut $crate::abi::Loaded) -> bool {
                fn make() -> ::std::boxed::Box<dyn $crate::Plugin> {
                    ::std::boxed::Box::new($make)
                }
                // SAFETY: Oak calls this only after comparing the stamps.
                unsafe { $crate::abi::start(link, out, make) }
            }
            // Oak's own crate, for the dependency graph only.
            const _: &str = ::oak::PLUGIN_PIN;
            $crate::abi::entry(init)
        }
    };
}
