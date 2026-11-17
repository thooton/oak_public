//! Plugins: the dynamic libraries Oak loads, what they registered, and Oak
//! as they reach it.
//!
//! A plugin is a library (`.so`, `.dll`, `.dylib`) in one of `dirs()`: the
//! `plugins` folder in Oak's folder, the one beside the executable, and
//! the folders `OAK_PLUGINS` names. The interface is plain C
//! (`oak_sdk::abi`, `sdk/include/oak_plugin.h`): `load` opens each
//! library, asks its `oak_plugin` who it is, and calls its `init` with
//! Oak's table of functions (`api`), through which it hands over its parts
//! (`parts`). The rest of Oak asks for those here: `parts()`.
//!
//! Each library is loaded from a copy of it in `plugin-copies` in Oak's
//! folder (`copy_of`), so the file in the plugins folder is never held
//! open and can be rebuilt or replaced while Oak runs (Windows refuses to
//! write a loaded DLL; Linux crashes a process whose mapped library is
//! rewritten in place). `reload` (Home's Plugins tab) looks again: plugins
//! added, changed on disk, switched on or stopped start, each from a fresh
//! copy, and those changed, removed or switched off are retired first:
//! their `exiting` hooks run and their parts leave `parts()`. Plugins that
//! didn't change keep running untouched. Libraries are never unloaded: a
//! retired plugin's threads may still run in it and what it registered may
//! still be borrowed, so every set of parts is kept for good too (`parts()`
//! hands out `&'static`).
//!
//! The table only grows at its end, so a plugin built for an older Oak
//! runs in this one; one built for a newer Oak says so and isn't started.
//!
//! `OAK_PLUGINS=off` (or `--no-plugins`) loads none, and reloads none.
//!
//! Plugins can be switched off (Home's Plugins tab): their ids are kept in
//! `plugins.json` in Oak's folder. A switched-off plugin's library is
//! opened to read who it is, but its `init` is never called, so it
//! registers and runs nothing. A change takes effect at the next start or
//! reload (`pending`).

mod api;
mod parts;

use crate::{api as accounts, store};
use oak_sdk::abi::{ENTRY_SYMBOL, OAK_ABI, OAK_API_VERSION, OakInit, OakPlugin};
pub(crate) use parts::{CloudBackend, FontDef, HomeTab, Info, NewKind, Parts, SessionKit, TabKind, Tags, Vault};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// A library file as it was on disk: its size and when it was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp { len: m.len(), modified: m.modified().ok() })
}

/// A library Oak looked at.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    /// The file in a plugins folder (not the copy loaded).
    pub(crate) path: PathBuf,
    /// Who it is, once it said (a library that isn't a plugin never does).
    pub(crate) info: Option<Info>,
    /// Why it isn't running: it doesn't fit, it failed to start, or it
    /// panicked since.
    pub(crate) trouble: Option<String>,
    /// It was switched off at the last start or reload, so it wasn't started.
    pub(crate) disabled: bool,
    /// The file as it was when it was opened.
    stamp: Option<Stamp>,
}

/// Which plugins are switched off, by id: as the last start or reload
/// applied, and as saved since (what the next one does).
struct Switches {
    applied: Vec<String>,
    saved: Vec<String>,
}

static SWITCHES: Mutex<Option<Switches>> = Mutex::new(None);

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SwitchFile {
    #[serde(default)]
    disabled: Vec<String>,
}

fn switch_file() -> PathBuf {
    store::dir().join("plugins.json")
}

/// The ids switched off, as saved (read once, then kept here).
fn with_switches<R>(f: impl FnOnce(&mut Switches) -> R) -> R {
    let mut s = SWITCHES.lock().unwrap();
    let s = s.get_or_insert_with(|| {
        let disabled = store::read_json::<SwitchFile>(&switch_file()).unwrap_or_default().disabled;
        Switches { applied: disabled.clone(), saved: disabled }
    });
    f(s)
}

/// Whether the plugin `id` is on, as saved (for the next start or reload).
pub(crate) fn enabled(id: &str) -> bool {
    with_switches(|s| !s.saved.iter().any(|d| d == id))
}

/// Switches the plugins `ids` on or off for the next start or reload, and
/// saves it. Switching one on switches on what it needs too (those found).
pub(crate) fn set_enabled(ids: &[String], on: bool) {
    let found = found();
    let mut ids: Vec<String> = ids.to_vec();
    if on {
        let mut i = 0;
        while i < ids.len() {
            let needs = found.iter().filter_map(|f| f.info.as_ref()).find(|info| info.id == ids[i]).map(|info| info.needs.clone()).unwrap_or_default();
            for n in needs {
                if found.iter().any(|f| f.info.as_ref().is_some_and(|i| i.id == n)) && !ids.contains(&n) {
                    ids.push(n);
                }
            }
            i += 1;
        }
    }
    let saved = with_switches(|s| {
        s.saved.retain(|d| !ids.contains(d));
        if !on {
            s.saved.extend(ids);
            s.saved.sort();
        }
        s.saved.clone()
    });
    if let Err(e) = store::write_json(&switch_file(), &SwitchFile { disabled: saved }) {
        crate::log_crash(&format!("plugins.json couldn't be saved: {e}"));
    }
}

/// Whether the plugins switched on or off since the last start or reload
/// differ from what runs: a reload or a restart applies them.
pub(crate) fn pending() -> bool {
    let found = found();
    with_switches(|s| {
        found.iter().filter_map(|f| f.info.as_ref()).any(|i| s.applied.contains(&i.id) != s.saved.contains(&i.id))
    })
}

/// A library opened in this process.
struct Opened {
    /// The file in a plugins folder (not the copy).
    path: PathBuf,
    stamp: Option<Stamp>,
    /// Who it is and its `init`, or why it can't run.
    plugin: Result<(Info, OakInit), String>,
    /// Its `init` was called: starting the plugin again takes a fresh copy
    /// (the same file again would be the same instance, statics and all).
    used: bool,
}

/// A plugin that runs, and what it registered.
struct Running {
    id: String,
    name: String,
    path: PathBuf,
    stamp: Option<Stamp>,
    parts: Parts,
}

struct State {
    /// The latest library opened from each file found.
    opened: Vec<Opened>,
    /// In the order they started; one started again keeps its place (cloud
    /// providers are numbered by it).
    running: Vec<Running>,
    /// The copies loaded by this process, kept on disk while it runs.
    copies: Vec<PathBuf>,
}

static STATE: Mutex<State> = Mutex::new(State { opened: Vec::new(), running: Vec::new(), copies: Vec::new() });
/// What the running plugins registered: a leaked `Parts`, replaced whole
/// by a reload (the one before stays valid, for whoever still borrows it).
static PARTS: AtomicPtr<Parts> = AtomicPtr::new(std::ptr::null_mut());
static LOADED: AtomicBool = AtomicBool::new(false);
static FOUND: Mutex<Vec<Found>> = Mutex::new(Vec::new());
static OFF: AtomicBool = AtomicBool::new(false);
/// A reload happened since the window last asked (`take_reloaded`).
static RELOADED: AtomicBool = AtomicBool::new(false);
/// What the plugin being started has handed over so far.
static REGISTERING: Mutex<Option<Parts>> = Mutex::new(None);

/// What the plugins registered (nothing, before `load` or with none).
pub(crate) fn parts() -> &'static Parts {
    static NONE: OnceLock<Parts> = OnceLock::new();
    let p = PARTS.load(Ordering::Acquire);
    if p.is_null() {
        NONE.get_or_init(Parts::default)
    } else {
        // SAFETY: a leaked box, never freed.
        unsafe { &*p }
    }
}

/// Takes a part from the plugin being started (`api`'s `add_` functions;
/// outside a plugin's `init` they do nothing).
fn registering(f: impl FnOnce(&mut Parts)) {
    if let Some(parts) = REGISTERING.lock().unwrap().as_mut() {
        f(parts);
    }
}

/// The libraries looked at, in the order they were.
pub(crate) fn found() -> Vec<Found> {
    FOUND.lock().unwrap().clone()
}

/// `--no-plugins`: `load` loads none.
pub fn disable() {
    OFF.store(true, Ordering::Relaxed);
}

/// Oak started without plugins (`--no-plugins`, `OAK_PLUGINS=off`).
pub(crate) fn none_loaded() -> bool {
    OFF.load(Ordering::Relaxed)
}

/// The folders plugins are looked for in.
pub(crate) fn dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Some(list) = std::env::var_os("OAK_PLUGINS") {
        dirs.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    dirs.push(store::dir().join("plugins"));
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("plugins"))) {
        dirs.push(beside);
    }
    let mut seen = vec![];
    dirs.retain(|d| {
        let real = d.canonicalize().unwrap_or_else(|_| d.clone());
        let new = !seen.contains(&real);
        seen.push(real);
        new
    });
    dirs
}

/// Whether `path` is named as a dynamic library is on this platform.
fn is_library(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(std::env::consts::DLL_EXTENSION))
}

/// The libraries in the plugins folders, in the order they are looked at.
fn libraries() -> Vec<PathBuf> {
    let mut all = vec![];
    for dir in dirs() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_library(p)).collect();
        files.sort();
        all.extend(files);
    }
    all
}

/// Where the copies plugins are loaded from are kept.
fn copies_dir() -> PathBuf {
    store::dir().join("plugin-copies")
}

/// The copy of the library at `path` to load: one already there from an
/// earlier start when the file hasn't changed since, else made now. Named
/// after the file, its place, size and time, so a copy is never written
/// over, with a number after when this process has that one loaded
/// already (`taken`: loading it again would give back the same instance).
/// None: no copy could be made.
fn copy_of(path: &Path, stamp: Option<Stamp>, taken: &[PathBuf]) -> Option<PathBuf> {
    let stamp = stamp?;
    let dir = copies_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let mut key = Sha256::new();
    key.update(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).as_os_str().as_encoded_bytes());
    key.update(stamp.len.to_le_bytes());
    let time = stamp.modified.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).unwrap_or_default();
    key.update(time.as_nanos().to_le_bytes());
    let key = accounts::hex(&key.finalize()[..8]);
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let ext = std::env::consts::DLL_EXTENSION;
    let whole = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() == stamp.len);
    let mut n = 1;
    loop {
        let name = if n == 1 { format!("{stem}-{key}.{ext}") } else { format!("{stem}-{key}-{n}.{ext}") };
        n += 1;
        let copy = dir.join(&name);
        if taken.contains(&copy) {
            continue;
        }
        if whole(&copy) {
            return Some(copy);
        }
        // Written beside it and renamed, so no Oak ever loads half a copy.
        let part = dir.join(format!("{name}.{}.part", std::process::id()));
        let made = std::fs::copy(path, &part).is_ok_and(|len| len == stamp.len) && std::fs::rename(&part, &copy).is_ok();
        let _ = std::fs::remove_file(&part);
        return (made || whole(&copy)).then_some(copy);
    }
}

/// Deletes, on a thread, the copies no Oak has loaded (Windows refuses
/// for those another Oak has; on Linux deleting one doesn't trouble the
/// Oak that loaded it), and half-made ones left long ago. `keep`: this
/// process's.
fn clear_copies(keep: Vec<PathBuf>) {
    std::thread::spawn(move || {
        let Ok(read) = std::fs::read_dir(copies_dir()) else { return };
        for e in read.flatten() {
            let p = e.path();
            if keep.contains(&p) {
                continue;
            }
            // (Another Oak may be writing a fresh one.)
            let stale = || e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(3600));
            if is_library(&p) || stale() {
                let _ = std::fs::remove_file(&p);
            }
        }
    });
}

/// Loads the library at `path`, for good.
fn library(path: &Path) -> Result<&'static libloading::Library, String> {
    // SAFETY: loading a library runs its initializers; that is what the
    // user asked for by putting it in a plugins folder.
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("it couldn't be loaded: {e}"))?;
    // Never unloaded: what it registers and its threads point into it.
    Ok(Box::leak(Box::new(lib)))
}

/// Opens the library at `path` (from a copy, `copy_of`; from the file
/// itself when there is none, or the copy doesn't load) and asks its
/// plugin who it is. `copies`: those this process loaded, which gets the
/// one used.
fn open(path: &Path, stamp: Option<Stamp>, copies: &mut Vec<PathBuf>) -> Opened {
    let lib = match copy_of(path, stamp, copies) {
        Some(copy) => match library(&copy) {
            Ok(lib) => {
                copies.push(copy);
                Ok(lib)
            }
            // (The original may find what it needs beside it.)
            Err(_) => library(path),
        },
        None => library(path),
    };
    Opened { path: path.to_path_buf(), stamp, plugin: lib.and_then(describe), used: false }
}

/// Who the plugin in `lib` is, and its `init`. Err: why it can't run in
/// this Oak.
fn describe(lib: &'static libloading::Library) -> Result<(Info, OakInit), String> {
    // SAFETY: the symbol's type is the one the interface gives it.
    let entry = unsafe { lib.get::<unsafe extern "C" fn() -> *const OakPlugin>(ENTRY_SYMBOL) }.map_err(|_| "it isn't an Oak plugin (it exports no oak_plugin)".to_string())?;
    // SAFETY: as above.
    let plugin = unsafe { entry() };
    if plugin.is_null() {
        return Err("it isn't an Oak plugin (oak_plugin gave nothing)".into());
    }
    // SAFETY: `size` and `abi` are the first fields of every version of
    // the descriptor, read before the rest is trusted.
    let (size, abi) = unsafe { ((*plugin).size, (*plugin).abi) };
    if abi != OAK_ABI {
        return Err(format!("it was built for another plugin interface ({abi}; this Oak's is {OAK_ABI})"));
    }
    // (What the first version of the descriptor had, up to its `init`.)
    if (size as usize) < std::mem::offset_of!(OakPlugin, init) + size_of::<Option<OakInit>>() {
        return Err("it isn't an Oak plugin (its descriptor is too small)".into());
    }
    // SAFETY: as much of the descriptor as the plugin's build has, from a
    // library that stays loaded.
    let d: OakPlugin = unsafe { oak_sdk::abi::read(plugin) };
    if d.api > OAK_API_VERSION {
        return Err(format!("it needs a newer Oak (it uses version {} of the plugin interface; this Oak has version {OAK_API_VERSION})", d.api));
    }
    let init = d.init.ok_or("it isn't an Oak plugin (it has no init)")?;
    // SAFETY: the descriptor's texts are the plugin's, valid for good.
    let info = unsafe {
        Info {
            id: api::text(d.id),
            name: api::text(d.name),
            version: api::text(d.version),
            about: api::text(d.about),
            needs: api::text(d.needs).split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect(),
        }
    };
    Ok((info, init))
}

/// Loads every plugin and takes what they register. Once, at startup,
/// before the window.
pub fn load() {
    if LOADED.swap(true, Ordering::SeqCst) {
        return;
    }
    let off = OFF.load(Ordering::Relaxed) || std::env::var("OAK_PLUGINS").is_ok_and(|v| v == "off" || v == "0");
    OFF.store(off, Ordering::Relaxed);
    apply(true);
}

/// What a reload did, by the plugins' names.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Changes {
    pub(crate) started: Vec<String>,
    pub(crate) restarted: Vec<String>,
    pub(crate) stopped: Vec<String>,
    /// Those that were to start and didn't (`Found::trouble` says why).
    pub(crate) failed: Vec<String>,
}

/// Home's Plugins tab's Reload, on the UI thread: the plugins folders and
/// the switches, looked at again (see the top of this file). Nothing
/// without plugins (`none_loaded`) or before `load`.
pub(crate) fn reload() -> Changes {
    if none_loaded() || !LOADED.load(Ordering::SeqCst) {
        return Changes::default();
    }
    apply(false)
}

/// Whether plugins were reloaded since this was last asked: the window
/// applies its theme and font again (a plugin's may have changed).
pub(crate) fn take_reloaded() -> bool {
    RELOADED.swap(false, Ordering::Relaxed)
}

/// Looks at the plugins folders and the switches and makes what runs fit
/// them: at startup (`first`) everything starts; at a reload what didn't
/// change keeps running, the rest is retired and started again.
fn apply(first: bool) -> Changes {
    let switched_off = with_switches(|s| {
        s.applied = s.saved.clone();
        s.saved.clone()
    });
    let before = found();
    let mut guard = STATE.lock().unwrap();
    let st = &mut *guard;

    // What is in the folders: opened when new or changed since.
    let mut found: Vec<Found> = vec![];
    let mut wanted: Vec<usize> = vec![];
    for path in if none_loaded() { vec![] } else { libraries() } {
        let stamp = stamp(&path);
        let at = st.opened.iter().position(|o| o.path == path);
        let i = match at {
            Some(i) if stamp.is_some() && st.opened[i].stamp == stamp => i,
            _ => {
                let o = open(&path, stamp, &mut st.copies);
                if first {
                    crate::startup::step(format!("plugin file {} opened", path.file_name().unwrap_or_default().to_string_lossy()));
                }
                match at {
                    Some(i) => {
                        st.opened[i] = o;
                        i
                    }
                    None => {
                        st.opened.push(o);
                        st.opened.len() - 1
                    }
                }
            }
        };
        match &st.opened[i].plugin {
            Ok((info, _)) => {
                let disabled = !info.id.is_empty() && switched_off.contains(&info.id);
                let trouble = if info.id.is_empty() {
                    Some("it has no id".to_string())
                } else if found.iter().any(|f| f.info.as_ref().is_some_and(|i| i.id == info.id)) {
                    Some(format!("a plugin with the id {} is loaded already", info.id))
                } else {
                    None
                };
                if trouble.is_none() && !disabled {
                    wanted.push(found.len());
                }
                found.push(Found { path, info: Some(info.clone()), trouble, disabled, stamp });
            }
            Err(why) => found.push(Found { path, info: None, trouble: Some(why.clone()), disabled: false, stamp }),
        }
    }
    st.opened.retain(|o| found.iter().any(|f| f.path == o.path));
    let info = |i: usize| found[i].info.clone().unwrap_or_default();

    // Those that can run: wanted, and what they need can run too.
    let mut runnable = wanted.clone();
    loop {
        let ids: Vec<String> = runnable.iter().map(|&i| info(i).id).collect();
        let n = runnable.len();
        runnable.retain(|&i| info(i).needs.iter().all(|need| ids.contains(need)));
        if runnable.len() == n {
            break;
        }
    }

    // What runs and can go on as it is keeps running; the rest is retired,
    // the last started first.
    let keeps = |r: &Running| {
        r.stamp.is_some()
            && before.iter().any(|f| f.path == r.path && f.trouble.is_none())
            && runnable.iter().any(|&i| found[i].path == r.path && found[i].stamp == r.stamp && info(i).id == r.id)
    };
    let keep: Vec<bool> = st.running.iter().map(keeps).collect();
    for (r, _) in st.running.iter().zip(&keep).rev().filter(|(_, k)| !**k) {
        for h in &r.parts.hooks {
            h.exiting();
        }
    }
    // Each running plugin's place, empty for those retired.
    let mut slots: Vec<(String, String, Option<Running>)> = st.running.drain(..).zip(keep).map(|(r, k)| (r.id.clone(), r.name.clone(), k.then_some(r))).collect();
    let wanted_ids: Vec<String> = wanted.iter().map(|&i| info(i).id).collect();
    let infos: Vec<Info> = (0..found.len()).map(info).collect();
    let paths: Vec<(PathBuf, Option<Stamp>)> = found.iter().map(|f| (f.path.clone(), f.stamp)).collect();
    *FOUND.lock().unwrap() = found;

    // The rest start, those needed first.
    let mut changes = Changes::default();
    let mut done: Vec<String> = slots.iter().filter(|(_, _, r)| r.is_some()).map(|(id, _, _)| id.clone()).collect();
    let mut todo: Vec<usize> = runnable.into_iter().filter(|&i| !done.contains(&infos[i].id)).collect();
    let mut new_hooks = vec![];
    while let Some(k) = todo.iter().position(|&i| infos[i].needs.iter().all(|n| done.contains(n))) {
        let at = todo.remove(k);
        let (info, (path, stamp)) = (&infos[at], paths[at].clone());
        let Some(o) = st.opened.iter().position(|o| o.path == path) else { continue };
        if st.opened[o].used {
            st.opened[o] = open(&path, stamp, &mut st.copies);
        }
        st.opened[o].used = true;
        let init = match &st.opened[o].plugin {
            Ok((again, init)) if again.id == info.id => Ok(*init),
            Ok(_) => Err("it changed while it was being loaded (reload again)".to_string()),
            Err(why) => Err(why.clone()),
        };
        let ok = init.as_ref().is_ok_and(|init| {
            *REGISTERING.lock().unwrap() = Some(Parts::default());
            // SAFETY: the plugin's `init`, with the table it was built for
            // (or a later one, which starts the same).
            unsafe { init(&api::TABLE) }
        });
        if first {
            crate::startup::step(format!("plugin {} started", info.id));
        }
        let parts = REGISTERING.lock().unwrap().take().unwrap_or_default();
        let mut found = FOUND.lock().unwrap();
        let name = name_of(info);
        let again = slots.iter().any(|(id, _, _)| *id == info.id);
        if ok && found[at].trouble.is_none() {
            new_hooks.extend(parts.hooks.iter().cloned());
            let running = Running { id: info.id.clone(), name: name.clone(), path, stamp, parts };
            match slots.iter_mut().find(|(id, _, r)| *id == info.id && r.is_none()) {
                Some(slot) => slot.2 = Some(running),
                None => slots.push((info.id.clone(), name.clone(), Some(running))),
            }
            done.push(info.id.clone());
            if again { changes.restarted.push(name) } else { changes.started.push(name) }
        } else {
            // (One that panicked said why already: `failed`.)
            if found[at].trouble.is_none() {
                found[at].trouble = Some(init.err().unwrap_or_else(|| "it failed to start".into()));
            }
            changes.failed.push(name);
        }
    }
    // Those wanted that didn't start for want of others say which.
    let mut found = FOUND.lock().unwrap();
    for (at, f) in found.iter_mut().enumerate().filter(|(_, f)| f.trouble.is_none()) {
        let info = &infos[at];
        if !wanted_ids.contains(&info.id) || done.contains(&info.id) {
            continue;
        }
        let missing: Vec<&str> = info.needs.iter().filter(|n| !done.contains(n)).map(|s| s.as_str()).collect();
        let off: Vec<&str> = missing.iter().copied().filter(|n| switched_off.iter().any(|d| d == n)).collect();
        f.trouble = Some(if off.len() == missing.len() {
            format!("it needs the plugin{} {}, which {} switched off", oak_sdk::plural(off.len()), off.join(", "), if off.len() == 1 { "is" } else { "are" })
        } else {
            format!("it needs the plugin{} {}, which {} loaded", oak_sdk::plural(missing.len()), missing.join(", "), if missing.len() == 1 { "isn't" } else { "aren't" })
        });
    }
    drop(found);
    for (id, name, r) in &slots {
        if r.is_none() && !done.contains(id) && !changes.failed.contains(name) {
            changes.stopped.push(name.clone());
        }
    }

    // What runs now, in place of what ran.
    st.running = slots.into_iter().filter_map(|(_, _, r)| r).collect();
    let mut all = Parts::default();
    for r in &st.running {
        all.absorb(r.parts.clone());
    }
    let old_vault = parts().vault.clone();
    PARTS.store(Box::into_raw(Box::new(all)), Ordering::Release);
    crate::agent::prompt::add_roles(&parts().roles);
    clear_copies(st.copies.clone());
    drop(guard);
    *DISK.lock().unwrap() = None;
    if !first {
        if let Some(ctx) = CONTEXT.get() {
            for h in &new_hooks {
                h.started(ctx);
            }
            ctx.request_repaint();
        }
        // A vault that went, or came already connected: its accounts follow
        // (one that connects later says so itself).
        let changed = match (&old_vault, &parts().vault) {
            (None, None) => false,
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            _ => true,
        };
        if changed {
            std::thread::spawn(|| {
                if vault().is_none_or(|v| v.connected()) {
                    accounts::vault_changed();
                }
            });
        }
        RELOADED.store(true, Ordering::Relaxed);
    }
    changes
}

/// What a reload would pick up in the plugins folders: libraries added,
/// changed or removed since they were last looked at.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct OnDisk {
    pub(crate) added: Vec<PathBuf>,
    pub(crate) changed: Vec<PathBuf>,
    pub(crate) removed: Vec<PathBuf>,
}

impl OnDisk {
    pub(crate) fn any(&self) -> bool {
        !(self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty())
    }
}

/// The last `on_disk`, and when it was looked.
static DISK: Mutex<Option<(Instant, OnDisk)>> = Mutex::new(None);

/// How often `on_disk` looks at the folders again.
pub(crate) const DISK_EVERY: Duration = Duration::from_secs(2);

/// What changed in the plugins folders (looked at again at most every
/// `DISK_EVERY`).
pub(crate) fn on_disk() -> OnDisk {
    let mut last = DISK.lock().unwrap();
    if let Some((at, d)) = &*last
        && at.elapsed() < DISK_EVERY
    {
        return d.clone();
    }
    let d = if none_loaded() {
        OnDisk::default()
    } else {
        let found = found();
        let now = libraries();
        OnDisk {
            added: now.iter().filter(|p| !found.iter().any(|f| &f.path == *p)).cloned().collect(),
            changed: found.iter().filter(|f| now.contains(&f.path) && (f.stamp.is_none() || stamp(&f.path) != f.stamp)).map(|f| f.path.clone()).collect(),
            removed: found.iter().filter(|f| !now.contains(&f.path)).map(|f| f.path.clone()).collect(),
        }
    };
    *last = Some((Instant::now(), d.clone()));
    d
}

/// `oak --plugins`: says where plugins are looked for, which were found
/// and which of them run. 1 when one doesn't.
pub fn report() -> i32 {
    println!("Oak {} (plugin interface {OAK_API_VERSION}) looks for plugins in:", crate::update::VERSION);
    for d in dirs() {
        println!("  {}{}", d.display(), if d.is_dir() { "" } else { "  (not there)" });
    }
    let found = found();
    if found.is_empty() {
        println!("None found.");
    }
    for f in &found {
        let who = f.info.as_ref().map_or_else(|| "?".to_string(), |i| format!("{} {} ({})", name_of(i), i.version, i.id));
        match &f.trouble {
            None if f.disabled => println!("off      {who}\n         {}", f.path.display()),
            None => println!("ok       {who}\n         {}", f.path.display()),
            Some(why) => println!("NOT RUN  {who}\n         {}\n         {why}", f.path.display()),
        }
    }
    let p = parts();
    println!(
        "Registered: {} Home tab{}, {} tab kind{}, {} New kind{}, {} cloud backend{}, {} session kit{}, {} theme{}, {} font{}, {} role{}, {} settings section{}, {} gate{}, {} hook set{}, {}.",
        p.home_tabs.len(), oak_sdk::plural(p.home_tabs.len()),
        p.tab_kinds.len(), oak_sdk::plural(p.tab_kinds.len()),
        p.new_kinds.len(), oak_sdk::plural(p.new_kinds.len()),
        p.cloud_backends.len(), oak_sdk::plural(p.cloud_backends.len()),
        p.session_kits.len(), oak_sdk::plural(p.session_kits.len()),
        p.themes.len(), oak_sdk::plural(p.themes.len()),
        p.fonts.len(), oak_sdk::plural(p.fonts.len()),
        p.roles.len(), oak_sdk::plural(p.roles.len()),
        p.settings.len(), oak_sdk::plural(p.settings.len()),
        p.gates.len(), oak_sdk::plural(p.gates.len()),
        p.hooks.len(), oak_sdk::plural(p.hooks.len()),
        if p.vault.is_some() { "a vault" } else { "no vault" },
    );
    found.iter().any(|f| f.trouble.is_some()) as i32
}

/// A plugin's name, as Home's Plugins tab and `--plugins` show it.
pub(crate) fn name_of(info: &Info) -> String {
    if info.name.is_empty() { info.id.clone() } else { info.name.clone() }
}

/// The plugin `id` can't go on (it panicked, and told Oak): Home's Plugins
/// tab says so. Its own code stops answering; Oak keeps running.
fn failed(id: &str, why: &str) {
    let line = format!("it stopped: {why} (crash.log in Oak's folder has more)");
    if let Some(f) = FOUND.lock().unwrap().iter_mut().find(|f| f.info.as_ref().is_some_and(|i| i.id == id)) {
        f.trouble = Some(line);
    }
    if let Some(ctx) = context() {
        ctx.request_repaint();
    }
}

/// The window's context (set when the window is up), for plugins' threads.
static CONTEXT: OnceLock<eframe::egui::Context> = OnceLock::new();

thread_local! {
    /// The context of the frame being drawn on this thread.
    static FRAME: std::cell::RefCell<Option<eframe::egui::Context>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn context() -> Option<eframe::egui::Context> {
    FRAME.with(|f| f.borrow().clone()).or_else(|| CONTEXT.get().cloned())
}

/// A frame starts in `ctx`, drawn with `theme`: what plugins draw until
/// the next goes there.
pub(crate) fn begin_frame(ctx: &eframe::egui::Context, theme: &crate::theme::Theme) {
    FRAME.with(|f| *f.borrow_mut() = Some(ctx.clone()));
    api::begin_frame(theme);
}

/// The window is up: plugins' `started` hooks run.
pub(crate) fn started(ctx: &eframe::egui::Context) {
    let _ = CONTEXT.set(ctx.clone());
    for h in &parts().hooks {
        h.started(ctx);
    }
}

/// Oak is closing.
pub(crate) fn exiting() {
    for h in &parts().hooks {
        h.exiting();
    }
}

/// The plugin tab kind that owns `key`, if one does.
pub(crate) fn tab_kind(key: &str) -> Option<&'static Arc<TabKind>> {
    parts().tab_kinds.iter().find(|k| k.owns(key))
}

/// The vault, when a plugin brought one.
pub(crate) fn vault() -> Option<&'static Arc<Vault>> {
    parts().vault.as_ref()
}

/// What plugins tell each other: a value under a name.
static SHARED: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

fn shared(name: &str) -> Option<Value> {
    SHARED.lock().unwrap().as_ref().and_then(|m| m.get(name).cloned())
}

fn share(name: &str, value: Option<Value>) {
    let mut shared = SHARED.lock().unwrap();
    let map = shared.get_or_insert_with(HashMap::new);
    match value {
        Some(v) => {
            map.insert(name.to_string(), v);
        }
        None => {
            map.remove(name);
        }
    }
}

/// An HTTP answer: its status and headers, and its body as it arrives
/// (an event stream is read line by line while it is open). Oak's own
/// requests and the ones plugins' parts answer are both this.
pub(crate) struct Response {
    pub(crate) status: u16,
    /// Names in lower case.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Box<dyn Read + Send>,
}

impl Response {
    /// 200 to 299.
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The whole body.
    pub(crate) fn bytes(mut self) -> Result<Vec<u8>, String> {
        let mut out = vec![];
        self.body.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// The whole body, as text (what isn't UTF-8 replaced).
    pub(crate) fn text(self) -> Result<String, String> {
        self.bytes().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// The whole body, as JSON.
    pub(crate) fn json(self) -> Result<Value, String> {
        serde_json::from_slice(&self.bytes()?).map_err(|e| e.to_string())
    }
}

/// A reqwest answer as a `Response`: its body is read as it arrives.
pub(crate) fn response(resp: reqwest::blocking::Response) -> Response {
    let headers = resp.headers().iter().map(|(n, v)| (n.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
    Response { status: resp.status().as_u16(), headers, body: Box::new(resp) }
}

/// The answers plugins hold, by their handles. Each has a lock of its
/// own: a read that waits for more of a stream holds only that.
static ANSWERS: Mutex<Option<HashMap<u64, Arc<Mutex<Response>>>>> = Mutex::new(None);
static NEXT_ANSWER: AtomicU64 = AtomicU64::new(1);

/// Keeps an answer for a plugin; its handle.
fn keep_answer(answer: Response) -> u64 {
    let handle = NEXT_ANSWER.fetch_add(1, Ordering::Relaxed);
    ANSWERS.lock().unwrap().get_or_insert_with(HashMap::new).insert(handle, Arc::new(Mutex::new(answer)));
    handle
}

/// Works with the answer under `handle`.
fn with_answer<R>(handle: u64, f: impl FnOnce(&mut Response) -> R) -> Option<R> {
    let answer = ANSWERS.lock().unwrap().as_ref().and_then(|all| all.get(&handle).cloned())?;
    let mut answer = answer.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut answer))
}

/// A body another thread is still reading: read through its lock.
struct SharedBody(Arc<Mutex<Response>>);

impl Read for SharedBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).body.read(buf)
    }
}

/// Takes the answer under `handle` back (a part answered with it, or the
/// plugin closed it).
fn take_answer(handle: u64) -> Option<Response> {
    let answer = ANSWERS.lock().unwrap().as_mut().and_then(|all| all.remove(&handle))?;
    Some(match Arc::try_unwrap(answer) {
        Ok(answer) => answer.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(shared) => {
            let (status, headers) = {
                let a = shared.lock().unwrap_or_else(|e| e.into_inner());
                (a.status, a.headers.clone())
            };
            Response { status, headers, body: Box::new(SharedBody(shared)) }
        }
    })
}

/// Oak's HTTP client for a plugin's request: its own, or one like it that
/// answers a redirect instead of following it (`redirects` false), or
/// goes around the proxy (`direct`).
fn http_client(redirects: bool, direct: bool) -> reqwest::blocking::Client {
    static CLIENTS: [OnceLock<reqwest::blocking::Client>; 4] = [const { OnceLock::new() }; 4];
    if redirects && !direct {
        return accounts::http();
    }
    CLIENTS[redirects as usize * 2 + direct as usize]
        .get_or_init(|| {
            let mut b = reqwest::blocking::Client::builder()
                .timeout(None)
                .connect_timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(60))
                .tcp_keepalive(Duration::from_secs(30));
            if !redirects {
                b = b.redirect(reqwest::redirect::Policy::none());
            }
            if direct {
                b = b.no_proxy();
            }
            b.build().unwrap()
        })
        .clone()
}

/// Sends a plugin's request with Oak's client. Err: it couldn't be sent or
/// no answer came; an answer with an error status is Ok.
fn http(method: &str, url: &str, headers: &[(String, String)], body: &[u8], timeout: Duration, redirects: bool, direct: bool) -> Result<Response, String> {
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| format!("{method} isn't an HTTP method"))?;
    let client = http_client(redirects, direct);
    let build = || {
        let mut req = client.request(method.clone(), url).timeout(timeout);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        if body.is_empty() { req } else { req.body(body.to_vec()) }
    };
    crate::provider::send(build).map(response).map_err(|e| accounts::error_chain(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_hover_test::{Headless, click, painted, rect_of};
    use crate::{Page, Screen};
    use eframe::egui::PointerButton::Primary;

    /// Builds the C example (`plugins/hello-c`) with a gcc-style C
    /// compiler (`OAK_TEST_CC`, else `CC`, else `gcc` on Windows and `cc`
    /// elsewhere) into a scratch folder; the folder. On Windows CI sets
    /// none: a `CC` of gcc there would also build the crates' own C
    /// (aws-lc, zstd) with MinGW, which MSVC's linker can't link.
    fn built_c_example() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = std::env::temp_dir().join(format!("oak-hello-c-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let lib = out.join(format!("liboak_hello_c.{}", std::env::consts::DLL_EXTENSION));
        compile_c(&root.join("plugins/hello-c/hello.c"), &lib);
        out
    }

    /// Compiles the C file `source` into the library `lib` (as
    /// `built_c_example` says).
    fn compile_c(source: &Path, lib: &Path) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cc = std::env::var("OAK_TEST_CC")
            .or_else(|_| std::env::var("CC"))
            .unwrap_or_else(|_| if cfg!(windows) { "gcc" } else { "cc" }.into());
        // (Windows has no use for -fPIC, and its compilers warn about it.)
        let pic: &[&str] = if cfg!(windows) { &[] } else { &["-fPIC"] };
        let done = std::process::Command::new(&cc)
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-shared"])
            .args(pic)
            .arg("-I")
            .arg(root.join("sdk/include"))
            .arg("-o")
            .arg(lib)
            .arg(source)
            .output()
            .unwrap_or_else(|e| panic!("the C compiler {cc} didn't run: {e}"));
        assert!(done.status.success(), "{} doesn't compile:\n{}", source.display(), String::from_utf8_lossy(&done.stderr));
    }

    /// Points Oak at the scratch folder `Headless::start(name, ..)` will
    /// use, before the plugins load: what they read as they start (a
    /// saved connection, say) is the scratch folder's, never the user's.
    /// `OAK_PLUGIN_SEED` names a folder whose files are copied there first
    /// (a connection to a test server).
    fn scratch_folder(name: &str) {
        let dir = std::env::temp_dir().join(format!("oak-{name}-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(seed) = std::env::var_os("OAK_PLUGIN_SEED") {
            for file in std::fs::read_dir(&seed).unwrap_or_else(|e| panic!("OAK_PLUGIN_SEED can't be read: {e}")).flatten().filter(|f| f.path().is_file()) {
                std::fs::copy(file.path(), dir.join(file.file_name())).unwrap();
            }
        }
        unsafe { std::env::set_var("OAK_DIR", &dir) };
    }

    /// The example plugins, loaded into the whole app, headless: the Rust
    /// one's tab on Home draws and takes clicks, its theme and its row are
    /// on Settings, its role on the New screen; the C one (built here from
    /// the header alone) draws its tab, counts clicks and takes typing;
    /// Home's Plugins tab lists both and switches one off and on.
    /// Needs the Rust plugin built, and no other test in the process
    /// (plugins are loaded once, for all of it):
    /// `cargo build -p oak-hello && OAK_PLUGINS=target/debug cargo test --lib the_example_plugins -- --ignored`
    #[test]
    #[ignore]
    fn the_example_plugins_draw_in_the_window() {
        let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built example plugin");
        // (Absolute: the scratch OAK folder moves nothing, but be sure.)
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
        let c_dir = built_c_example();
        dirs.push(c_dir.clone());
        unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
        scratch_folder("plugin");
        load();
        let found = found();
        let hello = found.iter().find(|f| f.info.as_ref().is_some_and(|i| i.id == "hello")).unwrap_or_else(|| panic!("the example plugin wasn't found: {found:?}"));
        assert_eq!(hello.trouble, None);
        assert_eq!(crate::agent::prompt::role("haiku").name, "Haiku");

        let (dir, mut h, mut app) = Headless::start("plugin", &[]);
        let ctx = h.ctx.clone();
        let mut frames = |app: &mut crate::App, events: Vec<eframe::egui::Event>| {
            let mut out = h.events(app, events);
            for _ in 0..2 {
                out = h.events(app, vec![]);
            }
            painted(&out)
        };
        // Home has its tab; picked, the plugin draws the cell.
        app.page = Page::Home;
        app.home_menu = true;
        let shown = frames(&mut app, vec![]);
        let tab = rect_of(&shown, "Hello");
        let shown = frames(&mut app, click(tab.center(), Primary));
        assert_eq!(app.home_list, "hello");
        let texts = |shown: &[(String, eframe::egui::Rect)]| shown.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().join("|");
        assert!(texts(&shown).contains("Hello from a plugin") && texts(&shown).contains("clicked 0 times"), "{}", texts(&shown));
        // A click in it reaches the plugin's widget.
        let shown = frames(&mut app, click(rect_of(&shown, "Count").center(), Primary));
        assert!(texts(&shown).contains("clicked 1 times"), "{}", texts(&shown));
        // What it does to the window goes through `app`.
        let shown = frames(&mut app, click(rect_of(&shown, "New conversation").center(), Primary));
        assert!(matches!(app.screen, Screen::New) && app.page == Page::Chats);
        assert!(texts(&shown).contains("Haiku"), "its role is offered: {}", texts(&shown));

        // Settings: its theme and its row.
        app.page = Page::Settings;
        let shown = frames(&mut app, vec![]);
        let all = texts(&shown);
        for want in ["Moss", "A row the example plugin adds."] {
            assert!(all.contains(want), "{want} isn't on Settings: {all}");
        }
        let before = ctx.global_style().visuals.panel_fill;
        frames(&mut app, click(rect_of(&shown, "Moss").center(), Primary));
        assert_eq!(app.settings.theme, "moss");
        let after = ctx.global_style().visuals.panel_fill;
        assert_ne!(before, after, "the theme is applied");
        // Its row's button goes to its tab.
        app.home_list = String::new();
        let shown = frames(&mut app, vec![]);
        frames(&mut app, click(rect_of(&shown, "Open its tab").center(), Primary));
        assert!(app.page == Page::Home && app.home_list == "hello");

        // The C plugin: found, running, and drawing through the same table.
        let c = found.iter().find(|f| f.info.as_ref().is_some_and(|i| i.id == "hello-c")).unwrap_or_else(|| panic!("the C example wasn't found: {found:?}"));
        assert_eq!(c.trouble, None);
        assert_eq!(c.info.as_ref().unwrap().name, "Hello C");
        let shown = frames(&mut app, vec![]);
        let shown = frames(&mut app, click(rect_of(&shown, "Hello C").center(), Primary));
        assert_eq!(app.home_list, "hello-c");
        assert!(texts(&shown).contains("Hello, world, from C") && texts(&shown).contains("clicked 0 times in C"), "{}", texts(&shown));
        let shown = frames(&mut app, click(rect_of(&shown, "Count in C").center(), Primary));
        assert!(texts(&shown).contains("clicked 1 times in C"), "{}", texts(&shown));
        // Typing in its field reaches its buffer (the field shows "world").
        let field = rect_of(&shown, "world");
        frames(&mut app, click(field.center(), Primary));
        let shown = frames(&mut app, vec![eframe::egui::Event::Text("ly ".into())]);
        assert!(texts(&shown).contains("from C") && !texts(&shown).contains("Hello, world, from C"), "{}", texts(&shown));
        assert_eq!(super::found().iter().filter_map(|f| f.trouble.clone()).collect::<Vec<_>>(), Vec::<String>::new());

        // Home's Plugins tab lists both; a click on a row switches it off
        // for the next start (saved), and Restart to apply shows; another
        // switches it back.
        let shown = frames(&mut app, vec![]);
        let tab = rect_of(&shown, "Plugins");
        let shown = frames(&mut app, click(tab.center(), Primary));
        assert_eq!(app.home_list, crate::plugins_view::TAB);
        let in_list = |shown: &[(String, eframe::egui::Rect)], s: &str| shown.iter().find(|(x, r)| x == s && r.top() > tab.bottom()).map(|(_, r)| *r).unwrap_or_else(|| panic!("no {s} in the list: {}", texts(shown)));
        in_list(&shown, "Hello C");
        assert!(!texts(&shown).contains("Restart to apply"));
        let shown = frames(&mut app, click(in_list(&shown, "Hello").center(), Primary));
        assert!(!enabled("hello") && enabled("hello-c") && pending());
        assert!(texts(&shown).contains("Off after reload") && texts(&shown).contains("Restart to apply"), "{}", texts(&shown));
        let saved = std::fs::read_to_string(switch_file()).unwrap();
        assert!(saved.contains("\"hello\""), "{saved}");
        let shown = frames(&mut app, click(in_list(&shown, "Hello").center(), Primary));
        assert!(enabled("hello") && !pending());
        assert!(!texts(&shown).contains("Restart to apply"), "{}", texts(&shown));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(c_dir);
    }

    /// Whatever plugins `OAK_PLUGINS` holds, loaded into the whole app,
    /// headless: each loads, and each part that draws does so without
    /// panicking (every Home tab, every theme, Settings, Home's Plugins tab, every choice on
    /// the New screen, and the tabs `OAK_PLUGIN_TABS` names, separated by
    /// commas), in a scratch Oak folder (`scratch_folder`). For a plugin's
    /// own repository to run against its build:
    /// `OAK_PLUGINS=<folder> cargo test --lib every_plugin_draws -- --ignored`
    #[test]
    #[ignore]
    fn every_plugin_draws() {
        let dirs = std::env::var_os("OAK_PLUGINS").expect("OAK_PLUGINS names the folder with the built plugins");
        let dirs: Vec<PathBuf> = std::env::split_paths(&dirs).map(|d| d.canonicalize().unwrap()).collect();
        unsafe { std::env::set_var("OAK_PLUGINS", std::env::join_paths(dirs).unwrap()) };
        scratch_folder("plugins");
        load();
        let troubles = || found().iter().filter_map(|f| f.trouble.as_ref().map(|t| format!("{}: {t}", f.path.display()))).collect::<Vec<_>>();
        assert!(!found().is_empty(), "no plugin was found");
        assert_eq!(troubles(), Vec::<String>::new());

        let (dir, mut h, mut app) = Headless::start("plugins", &[]);
        let ctx = h.ctx.clone();
        // `OAK_PLUGIN_WAIT_MS`: how long each view is given to fetch what it
        // shows (plugins that talk to a server); `OAK_PLUGIN_SHOW=1` prints
        // the text each view drew.
        let wait = std::env::var("OAK_PLUGIN_WAIT_MS").ok().and_then(|ms| ms.parse().ok()).unwrap_or(0);
        let show = std::env::var_os("OAK_PLUGIN_SHOW").is_some();
        let mut frames = |app: &mut crate::App, what: &str| {
            let mut out = h.events(app, vec![]);
            for round in 0..2 {
                if round == 1 && wait > 0 {
                    std::thread::sleep(Duration::from_millis(wait));
                }
                for _ in 0..3 {
                    out = h.events(app, vec![]);
                }
            }
            if show {
                let texts: Vec<String> = painted(&out).into_iter().map(|(s, _)| s.replace('\n', " ")).filter(|s| !s.trim().is_empty()).collect();
                println!("== {what}\n{}", texts.join(" | "));
            }
            assert_eq!(troubles(), Vec::<String>::new(), "after drawing {what}");
        };
        let p = parts();
        frames(&mut app, "the first screen");
        for theme in std::iter::once(crate::theme::OAK.to_string()).chain(p.themes.iter().map(|t| t.id())).rev() {
            app.settings.theme = theme.clone();
            crate::theme::apply_theme(&ctx, &crate::theme::theme(&theme));
            for tab in p.home_tabs.iter().filter(|tab| !tab.link()) {
                app.page = Page::Home;
                app.home_list = tab.id();
                frames(&mut app, &format!("Home's {} tab in the {theme} theme", tab.id()));
            }
        }
        app.page = Page::Settings;
        frames(&mut app, "Settings");
        app.page = Page::Home;
        app.home_list = crate::plugins_view::TAB.to_string();
        frames(&mut app, "Home's Plugins tab");
        let kinds = p.new_kinds.iter().map(|k| k.id()).chain(p.cloud_backends.iter().map(|b| b.id()));
        for kind in kinds {
            app.plugin_go_new(&kind);
            frames(&mut app, &format!("the New screen's {kind}"));
        }
        for key in std::env::var("OAK_PLUGIN_TABS").unwrap_or_default().split(',').map(str::trim).filter(|k| !k.is_empty()) {
            assert!(tab_kind(key).is_some(), "no plugin has the tab {key}");
            app.open(key.to_string());
            assert!(matches!(&app.screen, Screen::Chat(k) if k == key), "the tab {key} didn't open");
            frames(&mut app, &format!("the tab {key}"));
        }
        // As Oak does when it quits: what the plugins started ends.
        exiting();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Reload, in the whole app, headless, with variants of the C example
    /// (built here): a library rebuilt in place while it runs (it was
    /// loaded from a copy) is shown as changed and restarted by the
    /// Plugins tab's Reload; one added starts, leaving the other running;
    /// one switched off, or deleted, stops; with nothing changed nothing
    /// happens. Alone in its process (plugins load once per process):
    /// `cargo test --lib reloading_picks_up -- --ignored`
    #[test]
    #[ignore]
    fn reloading_picks_up_what_changed() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(root.join("plugins/hello-c/hello.c")).unwrap();
        let work = std::env::temp_dir().join(format!("oak-reload-test-src-{}", std::process::id()));
        let folder = work.join("plugins");
        std::fs::create_dir_all(&folder).unwrap();
        let lib = |name: &str| folder.join(format!("{name}.{}", std::env::consts::DLL_EXTENSION));
        // The C example with its texts swapped as `swaps` says.
        let build = |name: &str, swaps: &[(&str, &str)]| {
            let mut s = source.clone();
            for (from, to) in swaps {
                assert!(s.contains(from), "hello.c has no {from}");
                s = s.replace(from, to);
            }
            let c = work.join(format!("{name}.c"));
            std::fs::write(&c, s).unwrap();
            compile_c(&c, &lib(name));
        };
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let tab_ids = || parts().home_tabs.iter().map(|t| t.id()).collect::<Vec<_>>();
        build("hello_c", &[]);
        unsafe { std::env::set_var("OAK_PLUGINS", &folder) };
        scratch_folder("reload");
        load();
        assert_eq!(found().iter().map(|f| (f.info.as_ref().map(|i| i.id.clone()), f.trouble.clone())).collect::<Vec<_>>(), vec![(Some("hello-c".into()), None)]);
        let copies = STATE.lock().unwrap().copies.clone();
        assert!(copies.len() == 1 && copies[0].starts_with(copies_dir()), "loaded from a copy: {copies:?}");

        let (dir, mut h, mut app) = Headless::start("reload", &[]);
        let mut frames = |app: &mut crate::App, events: Vec<eframe::egui::Event>| {
            let mut out = h.events(app, events);
            for _ in 0..2 {
                out = h.events(app, vec![]);
            }
            painted(&out)
        };
        let texts = |shown: &[(String, eframe::egui::Rect)]| shown.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>().join("|");
        app.page = Page::Home;
        app.home_menu = true;
        app.home_list = "hello-c".into();
        let shown = frames(&mut app, vec![]);
        assert!(texts(&shown).contains("Hello, world, from C|"), "{}", texts(&shown));

        // Rebuilt in place while it runs (Windows refuses to write a loaded
        // DLL): the Plugins tab says so, and Reload starts the new build.
        build("hello_c", &[("from C\"", "from C, rebuilt\"")]);
        *DISK.lock().unwrap() = None;
        assert_eq!(on_disk().changed, vec![lib("hello_c")]);
        app.home_list = crate::plugins_view::TAB.into();
        let shown = frames(&mut app, vec![]);
        assert!(texts(&shown).contains("Changed on disk"), "{}", texts(&shown));
        let shown = frames(&mut app, click(rect_of(&shown, "Reload").center(), Primary));
        assert!(texts(&shown).contains("Restarted Hello C") && !texts(&shown).contains("Changed on disk"), "{}", texts(&shown));
        assert_eq!(tab_ids(), names(&["hello-c"]));
        assert_eq!(STATE.lock().unwrap().copies.len(), 2);
        app.home_list = "hello-c".into();
        let shown = frames(&mut app, vec![]);
        assert!(texts(&shown).contains("Hello, world, from C, rebuilt"), "{}", texts(&shown));

        // Another added: it starts, and the first runs on untouched.
        let d: &[(&str, &str)] = &[("\"hello-c\", 7", "\"hello-d\", 7"), ("OAK_S(\"hello-c\")", "OAK_S(\"hello-d\")"), ("\"Hello C\", 7", "\"Hello D\", 7"), ("OAK_S(\"Hello C\")", "OAK_S(\"Hello D\")")];
        build("hello_d", d);
        assert_eq!(reload(), Changes { started: names(&["Hello D"]), ..Changes::default() });
        assert_eq!(tab_ids(), names(&["hello-c", "hello-d"]));
        app.home_list = "hello-d".into();
        let shown = frames(&mut app, vec![]);
        assert!(texts(&shown).contains("Hello, world, from C|"), "{}", texts(&shown));

        // Switched off, it stops at the reload; on again, it starts.
        set_enabled(&["hello-d".into()], false);
        assert!(pending());
        assert_eq!(reload(), Changes { stopped: names(&["Hello D"]), ..Changes::default() });
        assert!(!pending() && tab_ids() == names(&["hello-c"]));
        assert!(found().iter().any(|f| f.disabled && f.info.as_ref().is_some_and(|i| i.id == "hello-d")));
        set_enabled(&["hello-d".into()], true);
        assert_eq!(reload(), Changes { started: names(&["Hello D"]), ..Changes::default() });

        // Deleted (it can be: a copy is what is loaded), it stops and leaves the list.
        std::fs::remove_file(lib("hello_d")).unwrap();
        assert_eq!(reload(), Changes { stopped: names(&["Hello D"]), ..Changes::default() });
        assert_eq!(found().len(), 1);
        assert_eq!(tab_ids(), names(&["hello-c"]));
        // Nothing changed: nothing happens.
        assert_eq!(reload(), Changes::default());
        exiting();
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(work);
    }
}
