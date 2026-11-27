// Oak's own programs for the agent's commands: `bin/` in Oak's folder,
// first on the PATH of every command (`tasks::bash`). It holds ripgrep
// (`rg`, `rg.exe` on Windows), the latest release when Oak was built,
// bundled into the binary by build.rs; the prompt tells the model to use it
// instead of grep. It is written there on the first command of each run
// that finds it missing or different (another version of Oak wrote it, or
// none has yet): to a temporary file, then renamed over it, so a command
// running the old one keeps it. On Windows a running rg.exe can't be
// replaced; the old one then stays until a later run.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/ripgrep.rs"));

/// The folder to put first on commands' PATH, with rg in it (written the
/// first time it is asked for). None when it can't be made.
pub fn dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = crate::store::dir().join("bin");
        match install(&dir, RG) {
            Ok(()) => Some(dir),
            Err(e) => {
                eprintln!("oak: couldn't put ripgrep {RG_VERSION} in {}: {e}", dir.display());
                // An rg written by an earlier run still serves.
                dir.join(exe_name()).is_file().then_some(dir)
            }
        }
    })
    .as_deref()
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "rg.exe" } else { "rg" }
}

/// Makes `dir` and puts `rg` in it as an executable, unless it is there
/// already, byte for byte. An empty `rg` (a build without it) writes nothing.
fn install(dir: &Path, rg: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if rg.is_empty() {
        return Ok(());
    }
    let path = dir.join(exe_name());
    if std::fs::read(&path).is_ok_and(|have| have == rg) {
        return Ok(());
    }
    let tmp = dir.join(format!(".{}.{}.tmp", exe_name(), std::process::id()));
    let written = (|| {
        std::fs::write(&tmp, rg)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// `PATH` with Oak's bin folder in front, as `std::env::join_paths` makes it.
#[cfg_attr(windows, allow(dead_code))]
pub fn path_with_bin() -> Option<std::ffi::OsString> {
    let bin = dir()?;
    let rest = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&rest))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rg_is_written_once_and_replaced_when_it_differs() {
        let dir = crate::temp_dir().join(format!("oak-toolbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rg = dir.join(exe_name());
        install(&dir, b"one").unwrap();
        assert_eq!(std::fs::read(&rg).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&rg).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let before = std::fs::metadata(&rg).unwrap().modified().unwrap();
        install(&dir, b"one").unwrap();
        assert_eq!(std::fs::metadata(&rg).unwrap().modified().unwrap(), before, "the same rg is left alone");
        install(&dir, b"two").unwrap();
        assert_eq!(std::fs::read(&rg).unwrap(), b"two");
        install(&dir, b"").unwrap();
        assert_eq!(std::fs::read(&rg).unwrap(), b"two", "a build without rg leaves the one there");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no temporary files are left: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bundled rg runs, and is the one commands find.
    #[test]
    fn commands_run_the_bundled_rg() {
        if RG.is_empty() {
            eprintln!("built without ripgrep");
            return;
        }
        let p = crate::tasks::spawn("command -v rg; rg --version", Path::new(".")).unwrap();
        let start = std::time::Instant::now();
        while p.exit().is_none() && start.elapsed() < std::time::Duration::from_secs(20) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (out, _) = p.take_new(10_000);
        assert_eq!(p.exit(), Some(0), "{out}");
        let mut lines = out.lines();
        let found = lines.next().unwrap_or("");
        let bin = dir().unwrap().join(exe_name());
        assert!(Path::new(found).ends_with("bin/rg") || Path::new(found).ends_with("bin/rg.exe") || Path::new(found) == bin, "{out}");
        assert!(out.contains(&format!("ripgrep {RG_VERSION}")) || RG_VERSION == "custom", "{out}");
    }
}
