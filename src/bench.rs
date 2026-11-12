// Benchmark hooks, all off unless their environment variable is set:
//
// - `OAK_API_BASE`: where API requests go instead of https://api.anthropic.com
//   (a recording proxy, for instance: `http://127.0.0.1:5701`).
// - `OAK_BENCH_LOG`: a file that gets one JSON line per timeline event
//   (requests, tools, saves, summaries, turns), timed in Unix milliseconds.
// - `OAK_SHARED_CLIENT=0`: a new HTTP client (so a new connection and TLS
//   handshake) for every request, for comparing that with the client OAK
//   keeps alive.

use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

const DEFAULT_API: &str = "https://api.anthropic.com";

/// The API's base URL, without a trailing slash.
pub fn api_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        std::env::var("OAK_API_BASE").ok().map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_API.into())
    })
}

/// One client for every request, unless `OAK_SHARED_CLIENT=0`.
pub fn shared_client() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OAK_SHARED_CLIENT").map_or(true, |v| v.trim() != "0"))
}

fn log_file() -> Option<&'static Mutex<std::fs::File>> {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = std::env::var_os("OAK_BENCH_LOG").filter(|p| !p.is_empty())?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    })
    .as_ref()
}

/// The timeline log is on.
pub fn on() -> bool {
    log_file().is_some()
}

/// Wall-clock time in Unix milliseconds, with the fraction.
pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// Appends an event for conversation `conv`; `extra`'s fields join it.
pub fn log(conv: &str, ev: &str, extra: Value) {
    let Some(file) = log_file() else { return };
    let mut line = json!({"t": now(), "ev": ev, "conv": conv});
    if let (Some(o), Value::Object(x)) = (line.as_object_mut(), extra) {
        o.extend(x);
    }
    let mut f = file.lock().unwrap();
    let _ = writeln!(f, "{line}");
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::time::Instant;

    fn ms(t: Instant) -> f64 {
        t.elapsed().as_secs_f64() * 1000.0
    }

    /// The work OAK does between turns, on a big conversation: saving it
    /// (after every message) and re-reading tracked files (after every
    /// command). `cargo test bench_between_turns -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_between_turns() {
        // ~1 MB (200 messages of 5 kB, as a long session has) and ~20 MB
        // (a very long one): how long a save holds the conversation (and
        // its caller), and how long until it is on disk.
        // And ~19 MB of a long session's turns (`frame_bench`): replies,
        // tool calls and their output.
        for (n, label) in [(200, "1 MB"), (4000, "20 MB"), (1500, "19 MB of turns")] {
            let messages: Vec<_> = if n == 1500 {
                serde_json::from_value(crate::frame_bench::long_conversation("x", n)["messages"].take()).unwrap()
            } else {
                (0..n).map(|i| json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": [{"type": "text", "text": "x".repeat(5000)}]})).collect()
            };
            let mut c = crate::store::Conversation { id: format!("unit-test-bench-{}-{n}", crate::api::now_ms()), messages: messages.into(), ..Default::default() };
            // As in a turn: a message added (or the last one changed, as
            // tool results are), then saved; the first save is the first
            // since loading.
            let (mut held, mut written): (Vec<f64>, Vec<f64>) = (0..11)
                .map(|i| {
                    if i % 2 == 0 {
                        c.messages.push(json!({"role": "user", "content": [{"type": "text", "text": "y".repeat(5000)}]}));
                    } else if let Some(m) = c.messages.last_mut() {
                        m["content"][0]["text"] = json!("z".repeat(5000));
                    }
                    let t = Instant::now();
                    c.save();
                    let held = ms(t);
                    crate::store::flush();
                    (held, ms(t))
                })
                .unzip();
            crate::store::remove_conversation(&c.id);
            let (first_held, first_written) = (held.remove(0), written.remove(0));
            held.sort_by(f64::total_cmp);
            written.sort_by(f64::total_cmp);
            println!(
                "save, {label} conversation: held {:.2} ms (median; first {first_held:.1} ms), on disk after {:.1} ms (first {first_written:.1} ms)",
                held[5], written[5]
            );
        }

        let folder = std::env::temp_dir().join(format!("oak-bench-files-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let mut tracked = crate::changes::Tracked::default();
        for i in 0..200 {
            let p = folder.join(format!("f{i}.rs"));
            std::fs::write(&p, "fn main() {}\n".repeat(400)).unwrap();
            tracked.record(&p);
        }
        let t = Instant::now();
        tracked.refresh();
        let refresh = ms(t);
        let t = Instant::now();
        let changed = tracked.check().len();
        println!("200 tracked files (5 kB each): refresh {refresh:.1} ms, check {:.1} ms ({changed} changed)", ms(t));
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
