//! What plugins keep from one run of Oak to the next (`prefs_get`,
//! `prefs_set` in the plugin interface): JSON under a key, by plugin id,
//! all in `plugin-prefs.json` in Oak's folder. A change is written a
//! moment later by a thread of its own (a slider dragged writes once),
//! and at the latest as Oak closes (`flush`).

use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// The prefs in one file.
struct Store {
    path: PathBuf,
    /// Plugin id to its keys; None until read.
    all: Option<Map<String, Value>>,
    /// Changed since it was last written.
    dirty: bool,
}

impl Store {
    fn new(path: PathBuf) -> Store {
        Store { path, all: None, dirty: false }
    }

    fn loaded(&mut self) -> &mut Map<String, Value> {
        let path = &self.path;
        self.all.get_or_insert_with(|| crate::store::read_json::<Map<String, Value>>(path).unwrap_or_default())
    }

    fn get(&mut self, plugin: &str, key: &str) -> Option<String> {
        self.loaded().get(plugin).and_then(|keys| keys.get(key)).map(|v| v.to_string())
    }

    /// Whether it changed anything.
    fn set(&mut self, plugin: &str, key: &str, json: &str) -> Result<bool, String> {
        let value = if json.trim().is_empty() { None } else { Some(serde_json::from_str::<Value>(json).map_err(|e| format!("not JSON: {e}"))?) };
        let all = self.loaded();
        let keys = all.entry(plugin.to_string()).or_insert_with(|| Value::Object(Map::new()));
        if !keys.is_object() {
            *keys = Value::Object(Map::new());
        }
        let keys = keys.as_object_mut().unwrap();
        let changed = match value {
            Some(v) => keys.insert(key.to_string(), v.clone()).is_none_or(|old| old != v),
            None => keys.remove(key).is_some(),
        };
        if keys.is_empty() {
            all.remove(plugin);
        }
        self.dirty |= changed;
        Ok(changed)
    }

    /// What to write, when something changed.
    fn take(&mut self) -> Option<Vec<u8>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(serde_json::to_vec_pretty(self.loaded()).unwrap_or_default())
    }
}

/// Oak's prefs, and whether a writer is waiting to write them.
static PREFS: LazyLock<Mutex<(Store, bool)>> = LazyLock::new(|| Mutex::new((Store::new(crate::store::dir().join("plugin-prefs.json")), false)));

/// How long a change waits before it is written (more may come).
const SETTLE: Duration = Duration::from_millis(400);

/// The JSON under `key` for `plugin`, as text.
pub fn get(plugin: &str, key: &str) -> Option<String> {
    PREFS.lock().unwrap().0.get(plugin, key)
}

/// Keeps `json` under `key` for `plugin` (empty: forgets it). Err: it
/// isn't JSON (nothing is kept).
pub fn set(plugin: &str, key: &str, json: &str) -> Result<(), String> {
    let mut p = PREFS.lock().unwrap();
    if p.0.set(plugin, key, json)? && !p.1 {
        p.1 = true;
        std::thread::Builder::new().name("plugin prefs".into()).spawn(write_later).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The writer: waits for changes to settle and writes them, until none
/// came while it wrote.
fn write_later() {
    loop {
        std::thread::sleep(SETTLE);
        write();
        let mut p = PREFS.lock().unwrap();
        if !p.0.dirty {
            p.1 = false;
            return;
        }
    }
}

/// Writes what changed, now.
fn write() {
    let (path, data) = {
        let mut p = PREFS.lock().unwrap();
        let path = p.0.path.clone();
        (path, p.0.take())
    };
    if let Some(data) = data
        && let Err(e) = crate::store::write_atomic(&path, data)
    {
        crate::log_crash(&format!("plugin-prefs.json wasn't written: {e}"));
    }
}

/// Writes what changed before Oak closes.
pub fn flush() {
    write();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys are kept per plugin and read back from the file; empty JSON
    /// forgets one; what isn't JSON is refused; the same value again
    /// changes nothing.
    #[test]
    fn prefs_are_kept_per_plugin() {
        let dir = std::env::temp_dir().join(format!("oak-prefs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plugin-prefs.json");
        let mut s = Store::new(path.clone());
        assert_eq!(s.set("duck", "size", "3"), Ok(true));
        assert_eq!(s.set("duck", "name", "\"Quack\""), Ok(true));
        assert_eq!(s.set("other", "size", "{\"w\": 1}"), Ok(true));
        assert_eq!(s.get("duck", "size").as_deref(), Some("3"));
        assert_eq!(s.get("other", "size").as_deref(), Some("{\"w\":1}"));
        assert_eq!(s.get("duck", "missing"), None);
        assert!(s.set("duck", "bad", "{oops").is_err());
        std::fs::write(&path, s.take().unwrap()).unwrap();
        assert_eq!(s.take(), None, "written: nothing more to write");
        assert_eq!(s.set("duck", "size", "3"), Ok(false));

        let mut again = Store::new(path);
        assert_eq!(again.get("duck", "name").as_deref(), Some("\"Quack\""));
        assert_eq!(again.set("duck", "name", ""), Ok(true));
        assert_eq!(again.get("duck", "name"), None);
        assert_eq!(again.set("other", "size", ""), Ok(true));
        assert!(!again.loaded().contains_key("other"), "a plugin with no keys left goes");
        let _ = std::fs::remove_dir_all(dir);
    }
}
