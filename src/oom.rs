// What an out-of-memory kill takes on Linux: the commands Oak starts, not
// Oak. A desktop starts Oak as a systemd unit of its own (KDE:
// app-oak@<id>.service) whose OOMPolicy is `stop`, so when the kernel kills
// any process in that unit for memory (a build the agent ran, say), systemd
// stops the whole unit and Oak with it. So the agent's commands and Oak's
// helpers (MCP servers, Chrome, the Node.js REPL) run in a scope of their
// own beside Oak's unit, `oak-commands-<pid>.scope`, with
// OOMPolicy=continue: a kill there ends only what it killed. A command
// joins the scope between fork and exec by writing itself into the scope's
// `cgroup.procs` (what it starts follows), and raises its `oom_score_adj`
// to 1000, so the kernel kills the largest of them before anything else of
// the user's, Oak included. The scope is made in the background as Oak
// starts (`prepare`), by systemd-run around a `tail --pid` that ends with
// Oak, so it is gone once Oak and the last command are; a command started
// before it is ready waits for it. Without a systemd user session or
// cgroup v2 there is no scope and only the `oom_score_adj` applies.
// Elsewhere both do nothing.

use std::process::Command;

/// Starts making the commands' scope, so the first command needn't wait.
pub fn prepare() {
    #[cfg(target_os = "linux")]
    std::thread::spawn(|| {
        linux::procs();
    });
}

/// `cmd` runs in the commands' scope, first in line for an out-of-memory
/// kill (Linux only).
pub fn isolate(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "linux")]
    linux::isolate(cmd);
    cmd
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CStr, CString};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// The scope's `cgroup.procs`, or None when it couldn't be made.
    pub(super) fn procs() -> Option<&'static CStr> {
        static PROCS: OnceLock<Option<CString>> = OnceLock::new();
        PROCS
            .get_or_init(|| match make() {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!("oak: commands run in Oak's own cgroup: {e}");
                    None
                }
            })
            .as_deref()
    }

    fn make() -> Result<CString, String> {
        let pid = std::process::id();
        let unit = format!("oak-commands-{pid}.scope");
        let mut child = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "--property=OOMPolicy=continue"])
            .arg(format!("--unit={unit}"))
            .arg(format!("--description=Oak's commands (pid {pid})"))
            .args(["--", "tail", "-f", "/dev/null", "-s", "10"])
            .arg(format!("--pid={pid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("couldn't run systemd-run ({e})"))?;
        // systemd-run puts itself in the scope, then becomes the tail.
        let cgroup = format!("/proc/{}/cgroup", child.id());
        let suffix = format!("/{unit}");
        let start = Instant::now();
        loop {
            let path = std::fs::read_to_string(&cgroup)
                .ok()
                .and_then(|t| t.lines().find_map(|l| l.strip_prefix("0::").map(str::to_owned)));
            if let Some(path) = path.filter(|p| p.ends_with(&suffix)) {
                std::thread::spawn(move || child.wait());
                return CString::new(format!("/sys/fs/cgroup{path}/cgroup.procs")).map_err(|e| e.to_string());
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("systemd-run failed ({status})"));
            }
            if start.elapsed() > Duration::from_secs(3) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("systemd-run didn't make the scope in 3 s".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn isolate(cmd: &mut Command) {
        let procs = procs();
        // Only open, write and close between fork and exec: nothing there
        // may allocate or lock.
        unsafe {
            cmd.pre_exec(move || {
                if let Some(procs) = procs {
                    write(procs, b"0");
                }
                write(c"/proc/self/oom_score_adj", b"1000");
                Ok(())
            });
        }
    }

    /// Writes `what` to the file at `path`, ignoring failure.
    unsafe fn write(path: &CStr, what: &[u8]) {
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd >= 0 {
                libc::write(fd, what.as_ptr().cast(), what.len());
                libc::close(fd);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::isolate;
        use std::process::Command;

        #[test]
        fn commands_go_first_and_in_their_own_scope() {
            let out = isolate(Command::new("bash").args(["-c", "cat /proc/self/oom_score_adj; bash -c 'cat /proc/self/cgroup'"]))
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            let mut lines = text.lines();
            assert_eq!(lines.next(), Some("1000"));
            let scope = format!("/oak-commands-{}.scope", std::process::id());
            let cgroup = lines.find_map(|l| l.strip_prefix("0::")).unwrap_or("");
            match super::procs() {
                Some(_) => assert!(cgroup.ends_with(&scope), "in {cgroup}"),
                None => eprintln!("no systemd user session: the scope isn't checked"),
            }
        }
    }
}
