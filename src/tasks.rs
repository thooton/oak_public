// Commands run through bash (Git Bash on Windows, the system's bash
// elsewhere); a `Proc` becomes a task once it has a task ID (TASK_NEW, or
// an interrupted or too-long EXECUTE). On Unix, bash leads its own process
// group: an interrupt is SIGINT to it, a kill SIGKILL to it. Windows has no
// SIGINT, so an interrupt also raises Ctrl+C in native programs' hidden
// consoles through a helper (`oak.exe --ctrl-c <pid>`); the whole process
// tree lives in a job object, which is how it is killed.

use crate::tools::{Head, one_line};
use crate::api::now_ms;
use std::collections::BTreeMap;
use std::io::Read;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
    OpenJobObjectW, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS, WaitForSingleObject};

/// Children inherit whether Ctrl+C is ignored, so make sure it is not.
#[cfg(windows)]
pub fn init() {
    unsafe { SetConsoleCtrlHandler(None, 0) };
}

/// Children inherit ignored signals, so make sure SIGINT reaches commands
/// as it would from a terminal. (Rust's runtime puts SIGPIPE back itself.)
#[cfg(unix)]
pub fn init() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

/// The `--ctrl-c <pid>...` helper. MSYS starts native programs in process
/// groups of their own, where Ctrl+C is disabled, so they get Ctrl+Break:
/// sent per group, it reaches only processes that lead one.
#[cfg(windows)]
pub fn ctrl_c_helper(pids: &[u32]) {
    unsafe {
        FreeConsole();
        if AttachConsole(pids[0]) != 0 {
            SetConsoleCtrlHandler(None, 1);
            for &pid in pids {
                GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            }
        }
    }
}

/// Git for Windows' root folder, if it is installed.
#[cfg(windows)]
fn git_root() -> Option<&'static PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut candidates = vec![PathBuf::from(r"C:\Program Files\Git"), PathBuf::from(r"C:\Program Files (x86)\Git")];
        if let Some(local) = dirs::data_local_dir() {
            candidates.push(local.join(r"Programs\Git"));
        }
        if let Ok(out) = Command::new("where").arg("git").creation_flags(CREATE_NO_WINDOW).output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(root) = Path::new(line.trim()).parent().and_then(|p| p.parent()) {
                    candidates.push(root.to_path_buf());
                }
            }
        }
        candidates.into_iter().find(|p| p.join(r"usr\bin\bash.exe").exists())
    })
    .as_ref()
}

/// MSYS bash itself rather than Git's bin\bash.exe launcher, so the PID OAK
/// holds is bash's own; the environment is what the launcher would set up.
#[cfg(windows)]
fn bash(args: &[&str]) -> Command {
    let mut cmd = match git_root() {
        Some(root) => {
            let mut cmd = Command::new(root.join(r"usr\bin\bash.exe"));
            let path = std::env::var("PATH").unwrap_or_default();
            let (mingw, usr) = (root.join(r"mingw64\bin"), root.join(r"usr\bin"));
            cmd.env("MSYSTEM", "MINGW64").env("PATH", format!("{};{};{path}", mingw.display(), usr.display()));
            cmd
        }
        None => Command::new("bash"),
    };
    cmd.args(args).creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// The system's bash, as the leader of a new process group so that an
/// interrupt reaches everything the command started.
#[cfg(unix)]
fn bash(args: &[&str]) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("bash");
    cmd.args(args).process_group(0);
    cmd
}

/// What starting bash failed with, for the model.
fn no_bash(e: std::io::Error) -> String {
    if cfg!(windows) {
        format!("Couldn't start Git Bash (is Git for Windows installed?): {e}")
    } else {
        format!("Couldn't start bash (is it installed and on PATH?): {e}")
    }
}

struct State {
    exit: Option<i32>,
    /// When it exited (ms since 1970).
    ended_ms: Option<i64>,
    ended: Option<Instant>,
    cursor: usize,
    notify: Vec<Box<dyn FnOnce() + Send>>,
    /// Said instead of the exit code: "was stopped when Oak closed".
    note: Option<String>,
}

/// The files of a command: its output (stdout and stderr, as they came),
/// the exit code its bash writes on the way out (read when Oak wasn't
/// there to see it exit), and the script bash runs.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Files {
    log: PathBuf,
    exit: PathBuf,
    script: PathBuf,
}

impl Files {
    fn remove(&self) {
        for f in [&self.log, &self.exit, &self.script] {
            if !f.as_os_str().is_empty() {
                let _ = std::fs::remove_file(f);
            }
        }
    }
}

pub struct Proc {
    pub command: String,
    pub started: Instant,
    /// When it started (ms since 1970), for the records kept across restarts.
    started_ms: i64,
    lines: Mutex<Vec<String>>,
    state: Mutex<State>,
    pid: u32,
    #[cfg(windows)]
    job: usize,
    /// The job's name, to open it again after a restart.
    #[cfg(windows)]
    job_name: String,
    /// Bash, reaped when the Proc goes (see Drop).
    #[cfg(unix)]
    child: Mutex<Option<std::process::Child>>,
    files: Files,
    /// A task's output is kept (its records outlive Oak); a command's
    /// goes with the Proc.
    keep: AtomicBool,
    /// Left running when Oak closed (or about to be): Oak no longer kills
    /// it on the way out.
    detached: AtomicBool,
    /// Oak started it (and so may signal its process group when it goes);
    /// not one found running again after a restart.
    #[cfg(unix)]
    owned: bool,
    /// Bash has exited; the reader takes what is left of the output.
    exited: AtomicBool,
    drained: AtomicBool,
}

/// Closing the job kills what is left of the command's process tree (not
/// a detached one's: its job no longer kills on close).
#[cfg(windows)]
impl Drop for Proc {
    fn drop(&mut self) {
        if self.job != 0 {
            unsafe { CloseHandle(self.job as HANDLE) };
        }
        if !self.keep.load(Ordering::SeqCst) {
            self.files.remove();
        }
    }
}

/// As on Windows, what is left of the command's process group (say, a
/// server it started with `&`) dies with it. Bash is reaped only here: till
/// then it stays a zombie, which keeps its PID (the group's ID) from going
/// to another process, so the signal can't hit anything else.
#[cfg(unix)]
impl Drop for Proc {
    fn drop(&mut self) {
        if self.owned && self.pid != 0 && !self.detached.load(Ordering::SeqCst) {
            unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.wait();
        }
        GROUPS.lock().unwrap().retain(|&g| g != self.pid);
        if !self.keep.load(Ordering::SeqCst) {
            self.files.remove();
        }
    }
}

/// The process groups of commands not yet dropped, for `kill_all`.
#[cfg(unix)]
static GROUPS: Mutex<Vec<u32>> = Mutex::new(vec![]);

/// When OAK quits, what its commands left running goes too, as closing
/// the job objects does it on Windows (not the detached tasks).
pub fn kill_all() {
    #[cfg(unix)]
    for g in GROUPS.lock().unwrap().drain(..) {
        unsafe { libc::kill(-(g as i32), libc::SIGKILL) };
    }
}

/// Numbers the script files of commands run at the same time.
static SCRIPTS: AtomicUsize = AtomicUsize::new(0);

/// Names this run's commands apart from those of an earlier Oak that are
/// still running: its process ID and start.
fn run_stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| format!("{}-{}", std::process::id(), now_ms()))
}

/// The Instant `ms` (ms since 1970) was, or now if the clock can't go back
/// that far.
fn instant_at(ms: i64) -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_millis((now_ms() - ms).max(0) as u64)).unwrap_or(now)
}

/// Where commands' output goes.
fn output_dir() -> PathBuf {
    let d = crate::store::dir().join("task-output");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// A path as bash takes it.
fn bash_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

pub fn spawn(command: &str, cwd: &Path) -> Result<Arc<Proc>, String> {
    // The command goes to bash as a script file, not as `bash -c <command>`:
    // Windows passes arguments as one command-line string that each program
    // splits itself, and MSYS splits it differently from how Rust quotes it,
    // so a command mixing quotes and backslashes (say, a heredoc of code)
    // could reach bash cut short. A command line also stops at 32k chars.
    // The EXIT trap keeps bash from exec-ing the last command, so bash stays
    // the leader of the process group an interrupt signals; it also writes
    // the exit code down, for a task that outlives Oak.
    let n = SCRIPTS.fetch_add(1, Ordering::SeqCst);
    let name = format!("oak-cmd-{}-{n}", run_stamp());
    let out_dir = output_dir();
    let files = Files { log: out_dir.join(format!("{name}.log")), exit: out_dir.join(format!("{name}.exit")), script: std::env::temp_dir().join(format!("{name}.sh")) };
    std::fs::write(&files.script, format!("trap 'echo $? > \"$OAK_EXIT_FILE\"' EXIT\n{command}\n"))
        .map_err(|e| format!("Couldn't write the command to {}: {e}", files.script.display()))?;
    // The output goes to a file, not a pipe: a task left running when Oak
    // closes keeps writing it (a pipe would break), and Oak reads it on.
    // Both streams share it, in the order they were written. (Appending
    // on Unix; MSYS programs can't write to an append-only handle, but
    // theirs share one file position anyway.)
    let log = std::fs::OpenOptions::new().create(true).write(true).truncate(false).append(cfg!(unix)).open(&files.log).and_then(|f| Ok((f.try_clone()?, f)));
    let (out, err) = match log {
        Ok(f) => f,
        Err(e) => {
            files.remove();
            return Err(format!("Couldn't create the output file {}: {e}", files.log.display()));
        }
    };
    let started_ms = now_ms();
    let child = bash(&[&bash_path(&files.script)])
        .current_dir(cwd)
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("OAK_EXIT_FILE", bash_path(&files.exit))
        // `"$OAK" --window ...` drives GUI apps on a hidden desktop (Windows).
        .env("OAK", std::env::current_exe().map(|p| bash_path(&p)).unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| {
            files.remove();
            no_bash(e)
        })?;
    #[cfg(windows)]
    let job_name = format!("Local\\oak-task-{name}");
    #[cfg(windows)]
    let job = unsafe {
        let wide: Vec<u16> = job_name.encode_utf16().chain([0]).collect();
        let job = CreateJobObjectW(std::ptr::null(), wide.as_ptr());
        set_kill_on_close(job as usize, true);
        AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
        job as usize
    };
    let proc = Arc::new(Proc {
        command: command.to_string(),
        started: Instant::now(),
        started_ms,
        lines: Mutex::new(vec![]),
        state: Mutex::new(State { exit: None, ended_ms: None, ended: None, cursor: 0, notify: vec![], note: None }),
        pid: child.id(),
        #[cfg(windows)]
        job,
        #[cfg(windows)]
        job_name,
        #[cfg(unix)]
        child: Mutex::new(None),
        files,
        keep: AtomicBool::new(false),
        detached: AtomicBool::new(false),
        #[cfg(unix)]
        owned: true,
        exited: AtomicBool::new(false),
        drained: AtomicBool::new(false),
    });
    let reader = read_on(&proc, 0);
    #[cfg(unix)]
    let pid = child.id();
    #[cfg(unix)]
    {
        GROUPS.lock().unwrap().push(pid);
        *proc.child.lock().unwrap() = Some(child);
    }
    let p = proc.clone();
    std::thread::spawn(move || {
        #[cfg(windows)]
        let code = {
            let mut child = child;
            exit_code(&mut child)
        };
        #[cfg(unix)]
        let code = exit_code(pid);
        p.finish(code, None, &reader);
    });
    Ok(proc)
}

/// Reads the command's output file on from byte `from` as it grows, into
/// its lines, till bash has exited and the file has no more.
fn read_on(p: &Arc<Proc>, from: u64) -> std::thread::Thread {
    let p = p.clone();
    std::thread::spawn(move || {
        use std::io::{Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&p.files.log) else {
            p.drained.store(true, Ordering::SeqCst);
            return;
        };
        let _ = f.seek(SeekFrom::Start(from));
        let (mut buf, mut pending) = (vec![0u8; 64 * 1024], Vec::<u8>::new());
        let mut idle = Duration::from_millis(5);
        loop {
            // (Read after seeing the exit: what bash wrote is all there.)
            let done = p.exited.load(Ordering::SeqCst);
            match f.read(&mut buf) {
                Ok(n) if n > 0 => {
                    pending.extend_from_slice(&buf[..n]);
                    if let Some(end) = pending.iter().rposition(|&b| b == b'\n') {
                        let rest = pending.split_off(end + 1);
                        let mut lines = p.lines.lock().unwrap();
                        lines.extend(split_lines(&pending));
                        pending = rest;
                    }
                    idle = Duration::from_millis(5);
                }
                _ if done => {
                    if !pending.is_empty() {
                        p.lines.lock().unwrap().extend(split_lines(&pending));
                    }
                    break;
                }
                _ => {
                    std::thread::park_timeout(idle);
                    idle = (idle * 2).min(Duration::from_millis(100));
                }
            }
        }
        p.drained.store(true, Ordering::SeqCst);
    })
    .thread()
    .clone()
}

/// Output bytes as lines (the last may lack its newline).
fn split_lines(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.split(|&b| b == b'\n').map(|l| String::from_utf8_lossy(l).trim_end_matches('\r').to_string())
}

/// The kill-on-close limit of job `job`, on or off.
#[cfg(windows)]
fn set_kill_on_close(job: usize, on: bool) {
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = if on { JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE } else { 0 };
        SetInformationJobObject(job as HANDLE, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32);
    }
}

/// Waits for bash to exit. Deaths by signal are reported the way a shell
/// would (128 + signal): MSYS exits with the signal in the second byte,
/// Windows with STATUS_CONTROL_C_EXIT.
#[cfg(windows)]
fn exit_code(child: &mut std::process::Child) -> i32 {
    shell_code(child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1))
}

#[cfg(windows)]
fn shell_code(code: i32) -> i32 {
    match code {
        c if c >= 256 && c % 256 == 0 && c < 65 * 256 => 128 + c / 256,
        -1073741510 => 130,
        c => c,
    }
}

/// Waits for bash to exit without reaping it (see Proc's Drop). Deaths by
/// signal are reported the way a shell would: 128 + signal.
#[cfg(unix)]
fn exit_code(pid: u32) -> i32 {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if r == 0 {
            let status = unsafe { info.si_status() };
            return if info.si_code == libc::CLD_EXITED { status } else { 128 + status };
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return -1;
        }
    }
}

impl Proc {
    pub fn exit(&self) -> Option<i32> {
        self.state.lock().unwrap().exit
    }

    pub fn elapsed(&self) -> Duration {
        let end = self.state.lock().unwrap().ended_ms.unwrap_or_else(now_ms);
        Duration::from_millis((end - self.started_ms).max(0) as u64)
    }

    /// Bash exited with `code` (`note` says how, when Oak didn't see it):
    /// once the reader has the rest of the output, the exit is set and
    /// those waiting for it are told.
    fn finish(&self, code: i32, note: Option<String>, reader: &std::thread::Thread) {
        self.exited.store(true, Ordering::SeqCst);
        reader.unpark();
        // Don't wait on a grandchild that keeps writing, though.
        let t = Instant::now();
        while !self.drained.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&self.files.script);
        let notify = {
            let mut s = self.state.lock().unwrap();
            s.exit = Some(code);
            s.ended = Some(Instant::now());
            s.ended_ms = Some(now_ms());
            s.note = note;
            std::mem::take(&mut s.notify)
        };
        for f in notify {
            f();
        }
    }

    /// Leaves the command running when Oak closes (its output still goes
    /// to its file, and the next Oak reads on).
    pub fn detach(&self) {
        if self.exit().is_some() {
            return;
        }
        self.detached.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        set_kill_on_close(self.job, false);
        #[cfg(unix)]
        GROUPS.lock().unwrap().retain(|&g| g != self.pid);
    }

    /// What is kept of task `id` across restarts.
    fn record(&self, id: u64, summary: &str, removed: bool) -> Record {
        let s = self.state.lock().unwrap();
        Record {
            id,
            command: self.command.clone(),
            summary: summary.to_string(),
            started_ms: self.started_ms,
            ended_ms: s.ended_ms.or(removed.then(now_ms)),
            exit: s.exit,
            note: s.note.clone(),
            cursor: s.cursor,
            pid: self.pid,
            #[cfg(windows)]
            job: self.job_name.clone(),
            #[cfg(unix)]
            job: String::new(),
            files: self.files.clone(),
            removed,
            detached: self.detached.load(Ordering::SeqCst) && s.exit.is_none(),
        }
    }

    pub fn unread(&self) -> usize {
        self.lines.lock().unwrap().len() - self.state.lock().unwrap().cursor
    }

    /// Output lines not returned before, at most about `max` characters.
    /// The bool says whether more are waiting.
    pub fn take_new(&self, max: usize) -> (String, bool) {
        let lines = self.lines.lock().unwrap();
        let mut s = self.state.lock().unwrap();
        let mut out = String::new();
        while s.cursor < lines.len() && (out.is_empty() || out.len() + lines[s.cursor].len() < max) {
            let line = &lines[s.cursor];
            if line.len() > max {
                out.push_str(&line[..line.floor_char_boundary(max)]);
                out.push_str(" [line truncated]");
            } else {
                out.push_str(line);
            }
            out.push('\n');
            s.cursor += 1;
        }
        (out, s.cursor < lines.len())
    }

    /// Runs `f` when the command exits (at once if it already has).
    pub fn on_exit(&self, f: Box<dyn FnOnce() + Send>) {
        let mut s = self.state.lock().unwrap();
        if s.exit.is_some() {
            drop(s);
            f();
        } else {
            s.notify.push(f);
        }
    }

    /// SIGINT to bash's process group, as a terminal's Ctrl+C would send,
    /// plus a console Ctrl+Break for the native Windows programs in the job.
    #[cfg(windows)]
    pub fn interrupt(&self) {
        let script = format!(
            "for d in /proc/[0-9]*; do read w < $d/winpid; if [ \"$w\" = {} ]; then read g < $d/pgid; kill -INT -- -$g; break; fi; done 2>/dev/null",
            self.pid
        );
        let _ = bash(&["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        // The list's two u32 counts fill the first usize; process IDs follow.
        let mut list = vec![0usize; 1025];
        let ok = unsafe {
            let size = (list.len() * size_of::<usize>()) as u32;
            QueryInformationJobObject(self.job as HANDLE, JobObjectBasicProcessIdList, list.as_mut_ptr() as _, size, std::ptr::null_mut())
        };
        let count = if ok != 0 { (list[0] >> 32).min(1024) } else { 0 };
        let pids = list[1..=count].iter().map(|p| p.to_string());
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe).arg("--ctrl-c").arg(self.pid.to_string()).args(pids).creation_flags(DETACHED_PROCESS).status();
        }
    }

    #[cfg(windows)]
    pub fn kill(&self) {
        if self.job != 0 {
            unsafe { TerminateJobObject(self.job as HANDLE, 137) };
        }
    }

    /// SIGINT to bash's process group, as a terminal's Ctrl+C would send.
    #[cfg(unix)]
    pub fn interrupt(&self) {
        if self.pid != 0 && self.exit().is_none() {
            unsafe { libc::kill(-(self.pid as i32), libc::SIGINT) };
        }
    }

    /// SIGKILL to bash's process group.
    #[cfg(unix)]
    pub fn kill(&self) {
        if self.pid != 0 && self.exit().is_none() {
            unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
        }
    }

    fn wait(&self, d: Duration) -> bool {
        let t = Instant::now();
        while self.exit().is_none() && t.elapsed() < d {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.exit().is_some()
    }

    /// SIGINT, then SIGKILL if it is still going two seconds later.
    pub fn stop(&self) -> &'static str {
        if self.exit().is_some() {
            return "had already exited";
        }
        self.interrupt();
        if self.wait(Duration::from_secs(2)) {
            return "stopped after SIGINT";
        }
        self.kill();
        self.wait(Duration::from_secs(2));
        "killed (did not stop within 2s of SIGINT)"
    }

    pub fn status(&self) -> String {
        let s = self.state.lock().unwrap();
        match (s.exit, &s.note) {
            (Some(_), Some(note)) => note.clone(),
            (Some(code), None) => format!("exited with code {code}"),
            (None, _) => "running".into(),
        }
    }

    /// When the command exited, if it has.
    pub fn ended(&self) -> Option<Instant> {
        self.state.lock().unwrap().ended
    }

    /// The last `max` output lines (all of them, read or not), and how many
    /// lines there are in all.
    pub fn tail(&self, max: usize) -> (Vec<String>, usize) {
        let lines = self.lines.lock().unwrap();
        (last(&lines, max), lines.len())
    }
}

/// The last `max` of `lines`.
fn last(lines: &[String], max: usize) -> Vec<String> {
    lines[lines.len().saturating_sub(max)..].to_vec()
}

/// The commands EXECUTE calls are waiting on, by call ID, so the call's row
/// can show their output as it comes in. Only the worker holds (and so
/// drops) the Proc: the UI reads it under this lock, never clones it.
static STREAMING: Mutex<BTreeMap<String, Arc<Proc>>> = Mutex::new(BTreeMap::new());

/// While it lives, call `id`'s output streams to its row (`streaming`).
pub struct Streaming(String);

impl Streaming {
    pub fn new(id: &str, p: &Arc<Proc>) -> Streaming {
        if !id.is_empty() {
            STREAMING.lock().unwrap().insert(id.to_string(), p.clone());
        }
        Streaming(id.to_string())
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        // Out of the lock first: dropping the last Arc reaps the command.
        let p = STREAMING.lock().unwrap().remove(&self.0);
        drop(p);
    }
}

/// The output so far of the command EXECUTE call `id` is waiting on: its
/// last `max` lines, how many lines there are in all, and how long it has
/// run. None once the call has its result.
pub fn streaming(id: &str, max: usize) -> Option<(Vec<String>, usize, Duration)> {
    let map = STREAMING.lock().unwrap();
    let p = map.get(id)?;
    let (lines, total) = p.tail(max);
    Some((lines, total, p.elapsed()))
}

/// How many output lines the command EXECUTE call `id` is waiting on has
/// printed so far (for when its row needs laying out again).
pub fn streaming_len(id: &str) -> Option<usize> {
    STREAMING.lock().unwrap().get(id).map(|p| p.lines.lock().unwrap().len())
}

/// A task as the UI lists it.
#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub id: u64,
    pub command: String,
    /// The model's summary of the command; may be empty.
    pub summary: String,
    pub started: Instant,
    /// When it exited (or was removed); None while it runs.
    pub ended: Option<Instant>,
    /// "running", "exited with code 0", ...
    pub status: String,
    pub elapsed: Duration,
    pub exit: Option<i32>,
    /// The model removed it (TASK_DELETE).
    pub removed: bool,
}

/// The most output lines the UI keeps of a removed task.
const RETIRED_LINES: usize = 2_000;

/// A task the model removed: what the UI still shows of it. (The process
/// itself goes, and with it whatever it left running.)
struct Retired {
    info: TaskInfo,
    lines: Vec<String>,
    total: usize,
    record: Record,
}

fn info(id: u64, p: &Proc) -> TaskInfo {
    TaskInfo {
        id,
        command: p.command.clone(),
        summary: String::new(),
        started: p.started,
        ended: p.ended(),
        status: p.status(),
        elapsed: p.elapsed(),
        exit: p.exit(),
        removed: false,
    }
}

/// A task as kept across restarts (with its output in `files.log`).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Record {
    id: u64,
    command: String,
    summary: String,
    started_ms: i64,
    ended_ms: Option<i64>,
    exit: Option<i32>,
    note: Option<String>,
    /// How many of its output lines the model has read.
    cursor: usize,
    pid: u32,
    /// Its job object's name (Windows).
    job: String,
    files: Files,
    removed: bool,
    /// Left running when Oak closed.
    detached: bool,
}

/// A conversation's task records.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Saved {
    /// When they were saved (ms since 1970): for a task found gone after a
    /// restart, about when it stopped.
    saved_ms: i64,
    tasks: Vec<Record>,
}

/// How long the records of a task that ended stay (and its output).
const KEEP_ENDED_MS: i64 = 7 * 86_400_000;

/// Said of a task that was running when Oak closed and was stopped.
pub const STOPPED_ON_CLOSE: &str = "was stopped when Oak closed";

/// A task's output lines from its file; `whole` keeps a last line without
/// its newline (the command is done writing). Also the bytes read.
fn read_log(path: &Path, whole: bool) -> (Vec<String>, u64) {
    let bytes = std::fs::read(path).unwrap_or_default();
    let end = if whole { bytes.len() } else { bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) };
    let lines = if end == 0 { vec![] } else { split_lines(&bytes[..end]).collect() };
    (lines, end as u64)
}

/// The exit code a task's bash wrote on its way out, and when.
fn exit_file(path: &Path) -> Option<(i32, i64)> {
    let code = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    let at = std::fs::metadata(path).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
    Some((code, at))
}

/// Task record `r`'s bash, if it still runs (left running when Oak
/// closed): a handle to wait on (Windows), checked to be the same process
/// by when it started.
#[cfg(windows)]
fn find_running(r: &Record) -> Option<usize> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};
    if r.pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, r.pid);
        if h.is_null() {
            return None;
        }
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        let created_ms = ((created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64) / 10_000;
        let created_ms = created_ms as i64 - 11_644_473_600_000;
        if !ok || (created_ms - r.started_ms).abs() > 10_000 || WaitForSingleObject(h, 0) == 0 {
            CloseHandle(h);
            return None;
        }
        Some(h as usize)
    }
}

/// Task record `r`'s bash, if it still runs (left running when Oak closed).
#[cfg(unix)]
fn find_running(r: &Record) -> Option<u32> {
    if r.pid == 0 || unsafe { libc::kill(r.pid as i32, 0) } != 0 {
        return None;
    }
    // (Where there is /proc, also that it is the same bash.)
    let stem = r.files.script.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    match std::fs::read(format!("/proc/{}/cmdline", r.pid)) {
        Ok(cmd) if !String::from_utf8_lossy(&cmd).contains(&stem) => None,
        _ => Some(r.pid),
    }
}

/// Whether `pid` still runs (a zombie, ended but not yet reaped by its
/// parent, does not).
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next()) != Some('Z'),
        Err(_) => true,
    }
}

/// Task record `r` as a Proc again: still running (Oak reads its output
/// on and waits for it again), or ended (when Oak was closed, `closed_ms`).
fn revive(r: &Record, closed_ms: i64) -> Arc<Proc> {
    let running = if r.exit.is_none() { find_running(r) } else { None };
    let (lines, read_to) = read_log(&r.files.log, running.is_none());
    let (exit, ended_ms, note) = match (r.exit, running.is_some()) {
        (Some(code), _) => (Some(code), r.ended_ms, r.note.clone()),
        (None, true) => (None, None, None),
        (None, false) => match exit_file(&r.files.exit) {
            Some((code, at)) => (Some(code), Some(at), Some(format!("exited with code {code} while Oak was closed"))),
            None => (Some(137), Some(closed_ms.max(r.started_ms)), Some(STOPPED_ON_CLOSE.to_string())),
        },
    };
    let cursor = r.cursor.min(lines.len());
    #[cfg(windows)]
    let job = running.map_or(0, |h| unsafe {
        let wide: Vec<u16> = r.job.encode_utf16().chain([0]).collect();
        let mut job = OpenJobObjectW(0x1F_003F, 0, wide.as_ptr());
        if job.is_null() {
            // (Only bash itself is in a new job, not what it started.)
            job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            AssignProcessToJobObject(job, h as HANDLE);
        }
        set_kill_on_close(job as usize, true);
        job as usize
    });
    let p = Arc::new(Proc {
        command: r.command.clone(),
        started: instant_at(r.started_ms),
        started_ms: r.started_ms,
        lines: Mutex::new(lines),
        state: Mutex::new(State { exit, ended_ms, ended: ended_ms.map(instant_at), cursor, notify: vec![], note }),
        pid: r.pid,
        #[cfg(windows)]
        job,
        #[cfg(windows)]
        job_name: r.job.clone(),
        #[cfg(unix)]
        child: Mutex::new(None),
        files: r.files.clone(),
        keep: AtomicBool::new(true),
        detached: AtomicBool::new(false),
        #[cfg(unix)]
        owned: false,
        exited: AtomicBool::new(running.is_none()),
        drained: AtomicBool::new(running.is_none()),
    });
    let Some(running) = running else {
        let _ = std::fs::remove_file(&r.files.script);
        return p;
    };
    let reader = read_on(&p, read_to);
    let q = p.clone();
    #[cfg(windows)]
    std::thread::spawn(move || {
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE};
        let h = running as HANDLE;
        let mut code = 0u32;
        unsafe {
            WaitForSingleObject(h, INFINITE);
            GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
        }
        q.finish(shell_code(code as i32), None, &reader);
    });
    #[cfg(unix)]
    {
        GROUPS.lock().unwrap().push(running);
        std::thread::spawn(move || {
            // Not Oak's child any more: it can only be watched.
            while alive(running) {
                std::thread::sleep(Duration::from_millis(500));
            }
            match exit_file(&q.files.exit) {
                Some((code, _)) => q.finish(code, None, &reader),
                None => q.finish(137, Some("was killed".into()), &reader),
            }
        });
    }
    p
}

/// Every conversation's tasks, to keep their records when Oak closes.
static ALL: Mutex<Vec<std::sync::Weak<Tasks>>> = Mutex::new(vec![]);

/// Whether the tasks still running when Oak closes stay running (the
/// close dialog's choice).
static KEEP_ON_CLOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn keep_on_close(on: bool) {
    KEEP_ON_CLOSE.store(on, Ordering::SeqCst);
}

pub fn keeping_on_close() -> bool {
    KEEP_ON_CLOSE.load(Ordering::SeqCst)
}

/// Oak is closing: every conversation's task records are saved, its
/// running tasks first left running if so chosen (else they are stopped
/// on the way out, and the next Oak says so).
pub fn close_all() {
    let keep = keeping_on_close();
    let all: Vec<Arc<Tasks>> = ALL.lock().unwrap().iter().filter_map(|t| t.upgrade()).collect();
    #[cfg(windows)]
    let mut jobs = vec![];
    for t in all {
        if keep {
            for p in t.map.lock().unwrap().values() {
                p.detach();
                #[cfg(windows)]
                if p.detached.load(Ordering::SeqCst) {
                    jobs.push(p.job_name.clone());
                }
            }
        }
        t.save();
    }
    #[cfg(windows)]
    if !jobs.is_empty() {
        start_holder(&jobs);
    }
}

/// Where the process holding the jobs of tasks left running is noted
/// (Windows): its ID and when it started.
#[cfg(windows)]
fn holder_file() -> PathBuf {
    crate::store::dir().join("tasks").join("holder.json")
}

/// A job's name stays only while a handle to it is open, so as Oak closes
/// with tasks left running, `oak --hold-jobs` keeps their jobs open till
/// the next Oak has opened them again (`release_holder`), or they are done.
#[cfg(windows)]
fn start_holder(names: &[String]) {
    if let Ok(exe) = std::env::current_exe()
        && let Some(pid) = spawn_holder(&exe, names)
    {
        let file = holder_file();
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        let _ = crate::store::write_atomic(&file, serde_json::json!({ "pid": pid, "started_ms": now_ms() }).to_string());
    }
}

/// Starts `exe --hold-jobs` and waits (a while) for it to have the jobs.
#[cfg(windows)]
fn spawn_holder(exe: &Path, names: &[String]) -> Option<u32> {
    let child = Command::new(exe).arg("--hold-jobs").args(names).creation_flags(DETACHED_PROCESS).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = child.ok()?;
    // Oak's own handles stay open till the holder has its own.
    let mut out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = [0u8; 4];
        let _ = tx.send(out.read(&mut b).is_ok_and(|n| n > 0));
    });
    rx.recv_timeout(Duration::from_secs(5)).ok().filter(|&held| held)?;
    Some(child.id())
}

/// `oak --hold-jobs <name>...`: holds the jobs named open (see
/// `start_holder`) till all their processes are gone, or the next Oak ends
/// this.
#[cfg(windows)]
pub fn hold_jobs(names: &[String]) {
    use std::io::Write;
    use windows_sys::Win32::System::JobObjects::{JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation};
    let mut jobs: Vec<HANDLE> = names
        .iter()
        .map(|n| {
            let wide: Vec<u16> = n.encode_utf16().chain([0]).collect();
            unsafe { OpenJobObjectW(0x1F_003F, 0, wide.as_ptr()) }
        })
        .filter(|j| !j.is_null())
        .collect();
    let _ = std::io::stdout().write_all(b"held\n");
    let _ = std::io::stdout().flush();
    while !jobs.is_empty() {
        std::thread::sleep(Duration::from_secs(2));
        jobs.retain(|&j| unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let ok = QueryInformationJobObject(j, JobObjectBasicAccountingInformation, &mut info as *mut _ as *mut _, std::mem::size_of_val(&info) as u32, std::ptr::null_mut());
            let live = ok != 0 && info.ActiveProcesses > 0;
            if !live {
                CloseHandle(j);
            }
            live
        });
    }
}

/// At start, once the conversations have opened their tasks' jobs again:
/// the holder from when Oak last closed can go.
pub fn release_holder() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        let file = holder_file();
        let Some(v) = crate::store::read_json::<serde_json::Value>(&file) else { return };
        let _ = std::fs::remove_file(&file);
        let r = Record { pid: v["pid"].as_u64().unwrap_or(0) as u32, started_ms: v["started_ms"].as_i64().unwrap_or(0), ..Default::default() };
        if let Some(h) = find_running(&r) {
            unsafe {
                CloseHandle(h as HANDLE);
                let h = OpenProcess(PROCESS_TERMINATE, 0, r.pid);
                if !h.is_null() {
                    TerminateProcess(h, 0);
                    CloseHandle(h);
                }
            }
        }
    }
}

/// Removes output files older than `KEEP_ENDED_MS` and a day (those of
/// commands an Oak that crashed never removed), once per run.
fn sweep_output() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let cutoff = std::time::SystemTime::now() - Duration::from_millis((KEEP_ENDED_MS + 86_400_000) as u64);
            let Ok(entries) = std::fs::read_dir(output_dir()) else { return };
            for e in entries.flatten() {
                if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        });
    });
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else if s < 3600 { format!("{}m{:02}s", s / 60, s % 60) } else { format!("{}h{:02}m", s / 3600, s / 60 % 60) }
}

/// How long a command ran, for the model: tenths of a second under a
/// minute, else as `fmt_duration`.
pub fn fmt_took(d: Duration) -> String {
    if d.as_secs() < 60 { format!("{:.1}s", d.as_secs_f64()) } else { fmt_duration(d) }
}

#[derive(Default)]
pub struct Tasks {
    map: Mutex<BTreeMap<u64, Arc<Proc>>>,
    /// Tasks the model removed, for the UI only.
    retired: Mutex<BTreeMap<u64, Retired>>,
    /// Each task's summary, for the UI.
    summaries: Mutex<BTreeMap<u64, String>>,
    /// Where the records are kept across restarts (none: not kept).
    file: Option<PathBuf>,
    /// Tasks left running when Oak closed: the session tells the model when
    /// they end (or that they did meanwhile).
    revived: Mutex<Vec<u64>>,
}

impl Tasks {
    /// Conversation `conv`'s tasks, as Oak last kept them: those left
    /// running when it closed and still running are watched again.
    pub fn open(conv: &str) -> Arc<Tasks> {
        let name: String = conv.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let file = (!cfg!(test) && !name.is_empty()).then(|| crate::store::dir().join("tasks").join(format!("{name}.json")));
        sweep_output();
        let t = Arc::new(Tasks::load(file));
        ALL.lock().unwrap().push(Arc::downgrade(&t));
        t
    }

    /// The tasks kept in `file` (stale records dropped, with their output).
    fn load(file: Option<PathBuf>) -> Tasks {
        let t = Tasks { file, ..Default::default() };
        let Some(saved) = t.file.as_deref().and_then(crate::store::read_json::<Saved>) else { return t };
        let now = now_ms();
        let mut dropped = false;
        for r in saved.tasks {
            if r.ended_ms.is_some_and(|e| now - e > KEEP_ENDED_MS) {
                r.files.remove();
                dropped = true;
                continue;
            }
            t.summaries.lock().unwrap().insert(r.id, r.summary.clone());
            if r.removed {
                let (lines, _) = read_log(&r.files.log, true);
                let ended_ms = r.ended_ms.unwrap_or(saved.saved_ms);
                let info = TaskInfo {
                    id: r.id,
                    command: r.command.clone(),
                    summary: String::new(),
                    started: instant_at(r.started_ms),
                    ended: Some(instant_at(ended_ms)),
                    status: r.note.clone().or(r.exit.map(|c| format!("exited with code {c}"))).unwrap_or_else(|| STOPPED_ON_CLOSE.into()),
                    elapsed: Duration::from_millis((ended_ms - r.started_ms).max(0) as u64),
                    exit: r.exit,
                    removed: true,
                };
                let total = lines.len();
                t.retired.lock().unwrap().insert(r.id, Retired { info, lines: last(&lines, RETIRED_LINES), total, record: r });
                continue;
            }
            let p = revive(&r, saved.saved_ms);
            if r.detached {
                t.revived.lock().unwrap().push(r.id);
            }
            p.keep.store(true, Ordering::SeqCst);
            t.map.lock().unwrap().insert(r.id, p);
        }
        if dropped {
            t.save();
        }
        t
    }

    /// Saves the records (when they are kept).
    pub fn save(&self) {
        let Some(file) = &self.file else { return };
        let mut tasks: Vec<Record> = {
            let summaries = self.summaries.lock().unwrap();
            let summary = |id: &u64| summaries.get(id).cloned().unwrap_or_default();
            self.map.lock().unwrap().iter().map(|(id, p)| p.record(*id, &summary(id), false)).collect()
        };
        tasks.extend(self.retired.lock().unwrap().values().map(|r| r.record.clone()));
        tasks.sort_by_key(|r| r.id);
        if tasks.is_empty() {
            let _ = std::fs::remove_file(file);
            return;
        }
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let saved = Saved { saved_ms: now_ms(), tasks };
        if let Ok(json) = serde_json::to_vec_pretty(&saved) {
            let _ = crate::store::write_atomic(file, json);
        }
    }

    /// The conversation is gone: so are its records and the tasks' output.
    pub fn forget(&self) {
        for p in self.map.lock().unwrap().values() {
            p.keep.store(false, Ordering::SeqCst);
            p.files.remove();
        }
        for r in self.retired.lock().unwrap().values() {
            r.record.files.remove();
        }
        if let Some(file) = &self.file {
            let _ = std::fs::remove_file(file);
        }
    }

    /// The tasks left running when Oak closed, for their session to watch
    /// (once).
    pub fn take_revived(&self) -> Vec<(u64, Arc<Proc>)> {
        let ids = std::mem::take(&mut *self.revived.lock().unwrap());
        ids.into_iter().filter_map(|id| Some((id, self.get(id)?))).collect()
    }

    /// Keeps what the UI shows of task `id`, which was just removed (and
    /// stopped).
    pub fn retire(&self, id: u64, p: &Proc) {
        let (lines, total) = p.tail(RETIRED_LINES);
        let mut info = info(id, p);
        info.removed = true;
        info.ended = info.ended.or_else(|| Some(Instant::now()));
        let summary = self.summaries.lock().unwrap().get(&id).cloned().unwrap_or_default();
        let record = p.record(id, &summary, true);
        self.retired.lock().unwrap().insert(id, Retired { info, lines, total, record });
        self.save();
    }

    /// Every task, running or not, removed ones included: running ones
    /// first, newest first, then the rest, the last to end first.
    pub fn infos(&self) -> Vec<TaskInfo> {
        let mut all: Vec<TaskInfo> = self.map.lock().unwrap().iter().map(|(id, p)| info(*id, p)).collect();
        all.extend(self.retired.lock().unwrap().values().map(|r| r.info.clone()));
        let summaries = self.summaries.lock().unwrap();
        for i in &mut all {
            i.summary = summaries.get(&i.id).cloned().unwrap_or_default();
        }
        all.sort_by(|a, b| match (a.ended, b.ended) {
            (None, None) => b.started.cmp(&a.started),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => y.cmp(&x),
        });
        all
    }

    /// Task `id`'s last `max` output lines, and how many there are in all.
    pub fn output(&self, id: u64, max: usize) -> Option<(Vec<String>, usize)> {
        if let Some(p) = self.get(id) {
            return Some(p.tail(max));
        }
        let retired = self.retired.lock().unwrap();
        let r = retired.get(&id)?;
        Some((last(&r.lines, max), r.total))
    }

    /// Adds task `id`; `summary` is the model's sentence for the user about
    /// what the command does (may be empty). Its record is kept, and saved
    /// again when it exits.
    pub fn add(self: &Arc<Self>, id: u64, p: Arc<Proc>, summary: &str) {
        p.keep.store(true, Ordering::SeqCst);
        self.map.lock().unwrap().insert(id, p.clone());
        let summary = one_line(summary);
        self.summaries.lock().unwrap().insert(id, summary);
        self.save();
        let tasks = Arc::downgrade(self);
        p.on_exit(Box::new(move || {
            if let Some(t) = tasks.upgrade() {
                t.save();
            }
        }));
    }

    pub fn get(&self, id: u64) -> Option<Arc<Proc>> {
        self.map.lock().unwrap().get(&id).cloned()
    }

    pub fn remove(&self, id: u64) -> Option<Arc<Proc>> {
        self.map.lock().unwrap().remove(&id)
    }

    /// How many tasks are still running.
    pub fn running(&self) -> usize {
        self.map.lock().unwrap().values().filter(|p| p.exit().is_none()).count()
    }

    /// Kills every task still running (ultrapause). They stay listed, as
    /// exited, so the model is told they finished as usual.
    pub fn kill_all(&self) {
        for p in self.map.lock().unwrap().values().filter(|p| p.exit().is_none()) {
            p.kill();
        }
    }

    pub fn list(&self) -> String {
        let map = self.map.lock().unwrap();
        if map.is_empty() {
            return "No tasks.".into();
        }
        map.iter()
            .map(|(id, p)| {
                let cmd: String = p.command.head(40);
                let cmd = if cmd.len() < p.command.len() { format!("{cmd}…") } else { cmd };
                format!("{id}\t{}\t{}\t{}", fmt_duration(p.elapsed()), p.status(), cmd.replace('\n', " "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interrupting still reaches the command when bash runs a script file.
    #[test]
    fn interrupt_stops_a_running_command() {
        let p = spawn("echo started; sleep 30; echo not reached", &std::env::temp_dir()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(p.stop(), "stopped after SIGINT");
        assert_eq!(p.exit(), Some(130));
        assert_eq!(p.take_new(usize::MAX).0, "started\n");
    }

    fn wait_exit(p: &Proc) {
        let t = Instant::now();
        while p.exit().is_none() && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Task records outlive the Tasks: an ended task comes back with its
    /// output, what the model read of it, and its summary; one left running
    /// is found again, read on, and waited for.
    #[test]
    fn tasks_are_kept_across_restarts() {
        let dir = std::env::temp_dir().join(format!("oak-tasks-test-{}", std::process::id()));
        let file = dir.join("conv.json");
        let t = Arc::new(Tasks::load(Some(file.clone())));
        let done = spawn("echo one; echo two >&2; exit 4", &std::env::temp_dir()).unwrap();
        t.add(1, done.clone(), "prints two lines");
        wait_exit(&done);
        assert_eq!(done.take_new(4).0, "one\n");
        let left = spawn("echo before; sleep 2; echo after; exit 3", &std::env::temp_dir()).unwrap();
        t.add(2, left.clone(), "");
        std::thread::sleep(Duration::from_millis(500));
        left.detach();
        t.save();
        let (log, _) = (done.files.log.clone(), drop(t));
        drop(done);
        assert!(log.exists(), "a task's output stays");

        let t = Tasks::load(Some(file.clone()));
        let p = t.get(1).unwrap();
        assert_eq!((p.exit(), p.unread()), (Some(4), 1));
        assert_eq!(p.take_new(usize::MAX).0, "two\n");
        assert_eq!(t.infos().iter().find(|i| i.id == 1).unwrap().summary, "prints two lines");
        let revived = t.take_revived();
        assert_eq!(revived.iter().map(|r| r.0).collect::<Vec<_>>(), [2]);
        let q = &revived[0].1;
        wait_exit(q);
        assert_eq!(q.exit(), Some(3));
        assert_eq!(q.take_new(usize::MAX).0, "before\nafter\n");
        t.forget();
        assert!(!log.exists() && !file.exists());
        drop(left);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A task found gone after a restart, with no exit code written (the
    /// job killed it when Oak closed), says it was stopped.
    #[test]
    fn a_task_killed_on_close_says_so() {
        let r = Record { id: 1, command: "sleep 9".into(), started_ms: now_ms() - 5000, pid: 0, ..Default::default() };
        let p = revive(&r, now_ms());
        assert_eq!(p.status(), STOPPED_ON_CLOSE);
        assert!(p.elapsed() >= Duration::from_secs(4));
    }

    /// What lets a task outlive Oak on Windows: with kill-on-close cleared,
    /// closing its job leaves the process running, and with the holder's
    /// handle the job can still be opened by name (without one, its name
    /// goes with Oak's handle).
    #[cfg(windows)]
    #[test]
    fn a_detached_job_outlives_its_handle() {
        let mut child = bash(&["-c", "sleep 5"]).spawn().unwrap();
        let name = format!("Local\\oak-task-test-{}", now_ms());
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), wide.as_ptr());
            set_kill_on_close(job as usize, true);
            AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
            set_kill_on_close(job as usize, false);
            // The holder's handle (`hold_jobs`, from Oak's build when
            // there is one); then Oak's goes.
            let exe = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("oak.exe");
            if exe.exists() {
                assert!(spawn_holder(&exe, std::slice::from_ref(&name)).is_some(), "the holder has the job");
            } else {
                assert!(!OpenJobObjectW(0x1F_003F, 0, wide.as_ptr()).is_null());
            }
            CloseHandle(job);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_none(), "still running");
        unsafe {
            let job = OpenJobObjectW(0x1F_003F, 0, wide.as_ptr());
            assert!(!job.is_null(), "the job is there to open again");
            TerminateJobObject(job, 137);
            CloseHandle(job);
        }
        assert_eq!(child.wait().unwrap().code(), Some(137));
    }
}
