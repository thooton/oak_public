//! How long startup takes, step by step. Once its first frame is on
//! screen, Oak writes startup.log in its folder (replacing the last one):
//! each step's time since the process was created and the processor time
//! the process had spent by then, so a slow start shows where it went and
//! whether it was waiting (on the disk, a virus scanner, a driver) or
//! working. Only the binary's own `main` starts the trace; tests don't.

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Trace {
    begun: Instant,
    /// From the process's creation to `begin` (loading the binary and
    /// what it links, as the system reports it).
    before: Option<Duration>,
    /// Each step: its name, its time since `begun`, the processor time by
    /// then.
    steps: Vec<(String, Duration, Option<Duration>)>,
    notes: Vec<String>,
    frames: u32,
}

static TRACE: Mutex<Option<Trace>> = Mutex::new(None);

/// Starts the trace: first thing in `main`.
pub(crate) fn begin() {
    let before = since_created();
    *TRACE.lock().unwrap() = Some(Trace { begun: Instant::now(), before, steps: vec![], notes: vec![], frames: 0 });
    step("main");
}

/// Marks the end of a step of startup.
pub(crate) fn step(name: impl Into<String>) {
    if let Some(t) = TRACE.lock().unwrap().as_mut() {
        let at = t.begun.elapsed();
        t.steps.push((name.into(), at, cpu_time()));
    }
}

/// A line of what startup found (how many conversations, which adapters).
pub(crate) fn note(text: impl Into<String>) {
    if let Some(t) = TRACE.lock().unwrap().as_mut() {
        t.notes.push(text.into());
    }
}

/// Each frame, from its start: the first is built, and once the next one
/// starts the first is on screen, so the trace is written and ends.
pub(crate) fn frame(ctx: &eframe::egui::Context) {
    let mut trace = TRACE.lock().unwrap();
    let Some(t) = trace.as_mut() else { return };
    t.frames += 1;
    if t.frames == 1 {
        drop(trace);
        step("first frame begun");
        ctx.request_repaint();
        return;
    }
    let at = t.begun.elapsed();
    t.steps.push(("first frame on screen".into(), at, cpu_time()));
    let text = render(t);
    *trace = None;
    drop(trace);
    let _ = std::fs::write(crate::store::dir().join("startup.log"), text);
}

fn render(t: &Trace) -> String {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let base = t.before.unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let mut out = format!(
        "Oak {} ({}) on {}, started {}\n\n",
        crate::update::VERSION,
        crate::update::COMMIT,
        std::env::consts::OS,
        crate::time::full_time(now - ms(t.begun.elapsed() + base) as i64, crate::time::local_offset_ms())
    );
    out += "  at (ms)  step (ms)  cpu (ms)  step\n";
    let mut last = Duration::ZERO;
    if let Some(before) = t.before {
        out += &format!("{:>9.0}  {:>9.0}  {:>8}  process created to main\n", ms(before), ms(before), "");
    }
    for (name, at, cpu) in &t.steps {
        let cpu = cpu.map_or(String::new(), |c| format!("{:.0}", ms(c)));
        out += &format!("{:>9.0}  {:>9.0}  {cpu:>8}  {name}\n", ms(base + *at), ms(at.saturating_sub(last)));
        last = *at;
    }
    if !t.notes.is_empty() {
        out += "\n";
        for n in &t.notes {
            out += &format!("{n}\n");
        }
    }
    out
}

/// How long ago the system created this process.
#[cfg(windows)]
fn since_created() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the current process's pseudo handle, and four FILETIMEs to fill.
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    // FILETIME: 100 ns units since 1601; the Unix epoch is 11644473600 s later.
    let created = ((created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64).checked_sub(116_444_736_000_000_000)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?;
    now.checked_sub(Duration::from_nanos(created * 100))
}

#[cfg(target_os = "linux")]
fn since_created() -> Option<Duration> {
    // The process's start in clock ticks since boot (field 22 of its stat,
    // counted after the command name in parentheses), and the time since boot.
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let ticks: f64 = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
    let up: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    // SAFETY: sysconf only reads a configuration value.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    (hz > 0.0).then(|| Duration::from_secs_f64((up - ticks / hz).max(0.0)))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn since_created() -> Option<Duration> {
    None
}

/// The processor time (user and kernel, all threads) the process has spent.
#[cfg(windows)]
fn cpu_time() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: as in `since_created`.
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let units = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
    Some(Duration::from_nanos((units(kernel) + units(user)) * 100))
}

#[cfg(unix)]
fn cpu_time() -> Option<Duration> {
    // SAFETY: getrusage fills the struct it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return None;
    }
    let tv = |t: libc::timeval| Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64);
    Some(tv(u.ru_utime) + tv(u.ru_stime))
}

#[cfg(not(any(windows, unix)))]
fn cpu_time() -> Option<Duration> {
    None
}
