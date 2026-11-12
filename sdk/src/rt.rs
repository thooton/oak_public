//! Between the C interface and the Rust wrapper: the table Oak gave, text
//! and bytes across it, and what happens to a panic.
//!
//! A panic never unwinds into Oak. A closure Oak calls back (a row's
//! body) catches it and keeps it (`carry`); when Oak's function returns,
//! the wrapper throws it again (`rethrow`), so it travels up the plugin's
//! own frames as usual. Where Oak called the plugin in the first place
//! (`outer`), it ends: Oak is told (`plugin_failed`), calls none of the
//! plugin's parts again, and says so on Settings.

use crate::abi::{OAK_ABI, OAK_API_VERSION, OakApi, OakBytes, OakPlugin, OakSink, OakStr};
use crate::plugin::{Plugin, Registrar};
use std::any::Any;
use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

static API: AtomicPtr<OakApi> = AtomicPtr::new(std::ptr::null_mut());
static DEAD: AtomicBool = AtomicBool::new(false);
static PLUGIN: OnceLock<(Box<dyn Plugin>, Leaked)> = OnceLock::new();

/// The plugin's descriptor and the text it points into, kept for good.
struct Leaked(OakPlugin, #[allow(dead_code)] String);

// (Its pointers are to strings that are never freed or changed.)
unsafe impl Send for Leaked {}
unsafe impl Sync for Leaked {}

/// Oak's functions. Panics before Oak started the plugin.
pub fn api() -> &'static OakApi {
    let p = API.load(Ordering::Acquire);
    assert!(!p.is_null(), "Oak hasn't started this plugin yet");
    unsafe { &*p }
}

/// Whether Oak started the plugin (false in the plugin's own tests).
pub fn has_api() -> bool {
    !API.load(Ordering::Acquire).is_null()
}

/// Text, lent to Oak for one call.
pub fn s(text: &str) -> OakStr {
    OakStr { ptr: text.as_ptr() as *const c_char, len: text.len() }
}

pub fn bytes(b: &[u8]) -> OakBytes {
    OakBytes { ptr: b.as_ptr(), len: b.len() }
}

/// Text Oak lent for a call.
///
/// # Safety
/// `s` must point to `len` readable bytes (or be empty).
pub unsafe fn str_of<'a>(s: OakStr) -> &'a str {
    if s.ptr.is_null() || s.len == 0 {
        return "";
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(s.ptr as *const u8, s.len) }).unwrap_or("")
}

/// # Safety
/// `b` must point to `len` readable bytes (or be empty).
pub unsafe fn bytes_of<'a>(b: OakBytes) -> &'a [u8] {
    if b.ptr.is_null() || b.len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(b.ptr, b.len) } }
}

unsafe extern "C" fn sink_bytes(user: *mut c_void, ptr: *const u8, len: usize) {
    let out = unsafe { &mut *(user as *mut Vec<u8>) };
    if !ptr.is_null() && len > 0 {
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, len) });
    }
}

unsafe extern "C" fn sink_parts(user: *mut c_void, ptr: *const u8, len: usize) {
    let out = unsafe { &mut *(user as *mut Vec<Vec<u8>>) };
    out.push(if ptr.is_null() || len == 0 { vec![] } else { unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec() });
}

/// What `call` hands its sink, and what `call` returns with it.
pub fn bytes_from<R>(call: impl FnOnce(OakSink, *mut c_void) -> R) -> (R, Vec<u8>) {
    let mut out: Vec<u8> = vec![];
    let r = call(sink_bytes, &mut out as *mut Vec<u8> as *mut c_void);
    (r, out)
}

pub fn text_from<R>(call: impl FnOnce(OakSink, *mut c_void) -> R) -> (R, String) {
    let (r, out) = bytes_from(call);
    (r, String::from_utf8_lossy(&out).into_owned())
}

/// Each piece `call` hands its sink, apart.
pub fn parts_from<R>(call: impl FnOnce(OakSink, *mut c_void) -> R) -> (R, Vec<Vec<u8>>) {
    let mut out: Vec<Vec<u8>> = vec![];
    let r = call(sink_parts, &mut out as *mut Vec<Vec<u8>> as *mut c_void);
    (r, out)
}

/// Hands `text` to a sink Oak gave.
///
/// # Safety
/// `out` and `user` must be the pair a caller gave for this call.
pub unsafe fn give(out: OakSink, user: *mut c_void, data: &[u8]) {
    unsafe { out(user, data.as_ptr(), data.len()) }
}

thread_local! {
    /// A panic caught in a closure Oak called, on its way back up.
    static CARRIED: RefCell<Option<Box<dyn Any + Send>>> = const { RefCell::new(None) };
}

/// Runs a closure Oak calls back: a panic in it is kept for `rethrow`.
pub fn carry<R>(f: impl FnOnce() -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(p) => {
            CARRIED.with(|c| *c.borrow_mut() = Some(p));
            None
        }
    }
}

/// After one of Oak's functions returned: throws on what a closure it
/// called panicked with.
pub fn rethrow() {
    if let Some(p) = CARRIED.with(|c| c.borrow_mut().take()) {
        resume_unwind(p);
    }
}

/// What a panic said.
pub fn panic_text(p: &(dyn Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "it panicked".to_string())
}

/// Points this process's wrappers at `table`, as Oak's `init` call does:
/// for Oak's own tests, and a plugin's that bring a table of their own.
pub fn attach(table: &'static OakApi) {
    API.store(table as *const OakApi as *mut OakApi, Ordering::Release);
}

/// Hands `plugin`'s parts to the Oak attached (what `init` does after
/// `attach`).
pub fn register(plugin: &dyn Plugin) {
    plugin.register(&mut Registrar::new());
}

/// Whether the plugin panicked before: its parts answer with nothing.
pub fn dead() -> bool {
    DEAD.load(Ordering::Acquire)
}

/// Runs what Oak asked a part for. A panic ends here: Oak is told, and
/// `default` is the answer, now and from then on.
pub fn outer<R>(default: impl FnOnce() -> R, f: impl FnOnce() -> R) -> R {
    if dead() {
        return default();
    }
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            CARRIED.with(|c| c.borrow_mut().take());
            DEAD.store(true, Ordering::Release);
            let why = panic_text(&*p);
            if has_api() {
                let id = PLUGIN.get().map(|(p, _)| p.info().id).unwrap_or_default();
                unsafe { (api().plugin_failed)(s(&id), s(&why)) };
            }
            default()
        }
    }
}

/// `export_plugin!`'s body: the plugin's descriptor, made once.
pub fn entry(make: fn() -> Box<dyn Plugin>) -> *const OakPlugin {
    let made = PLUGIN.get_or_init(|| {
        let plugin = make();
        let info = plugin.info();
        // One string holds every text of the descriptor, for good.
        let parts = [info.id.as_str(), info.name.as_str(), info.version.as_str(), info.about.as_str(), &info.needs.join(",")].map(str::to_string);
        let all: String = parts.concat();
        let mut at = 0;
        let mut next = |len: usize| {
            let r = OakStr { ptr: unsafe { all.as_ptr().add(at) } as *const c_char, len };
            at += len;
            r
        };
        let d = OakPlugin {
            size: std::mem::size_of::<OakPlugin>() as u32,
            abi: OAK_ABI,
            api: OAK_API_VERSION,
            reserved: 0,
            id: next(parts[0].len()),
            name: next(parts[1].len()),
            version: next(parts[2].len()),
            about: next(parts[3].len()),
            needs: next(parts[4].len()),
            init: Some(init),
        };
        // (Moving the String doesn't move its bytes.)
        (plugin, Leaked(d, all))
    });
    &made.1.0
}

unsafe extern "C" fn init(api: *const OakApi) -> bool {
    if api.is_null() {
        return false;
    }
    let (abi, version) = unsafe { ((*api).abi, (*api).api) };
    if abi != OAK_ABI || version < OAK_API_VERSION {
        return false;
    }
    API.store(api as *mut OakApi, Ordering::Release);
    // A panic's place goes to crash.log: the message alone reaches Settings.
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let id = PLUGIN.get().map(|(p, _)| p.info().id).unwrap_or_default();
        let line = format!("plugin {id}: {info}");
        unsafe { (crate::rt::api().log_crash)(s(&line)) };
        before(info);
    }));
    let Some((plugin, _)) = PLUGIN.get() else { return false };
    outer(
        || false,
        || {
            plugin.register(&mut Registrar::new());
            true
        },
    )
}

/// Exports the plugin: `oak_sdk::export_plugin!(MyPlugin)` with a value
/// of a type that implements `Plugin`.
#[macro_export]
macro_rules! export_plugin {
    ($make:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn oak_plugin() -> *const $crate::abi::OakPlugin {
            $crate::rt::entry(|| Box::new($make))
        }
    };
}
