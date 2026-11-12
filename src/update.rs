//! Oak's version, and updating Oak from its GitHub releases.
//!
//! Every push to main is built and tested by `.github/workflows/client.yml`
//! and, when it passes, published as release `v<version>` (marked latest),
//! with the Windows and Linux binaries and their SHA-256 sums. Here Oak
//! looks at the latest release now and then (`start`), and when it is newer
//! than this build, Home shows "Update available  <this> → <new>" left of
//! Duck.ai. A click downloads the build for this machine, checks its sum and
//! puts it in place of Oak's executable (`install`); then the same place
//! says "Restart to update", and a click closes Oak (through the close
//! dialog when conversations are at work, so they pause and resume) and
//! starts the new build (`restart`, `relaunch_if_asked`).
//!
//! Oak asks GitHub's API directly, for the repository the build was made
//! in (`OAK_UPDATE_REPO`, else `GITHUB_REPOSITORY`, when it was built: a
//! build that names none looks for no updates). For a private repository
//! it signs in with the GitHub login the machine already has for git
//! (`github_token`: `GITHUB_TOKEN`, `gh auth token`, or git's stored
//! credential for github.com, never prompting); with none it asks
//! anonymously, which is enough for a public one. The asset itself comes
//! from GitHub's storage, which the API redirects to (the token isn't sent
//! there).
//!
//! Plugins aren't part of an update: their interface is stable
//! (`docs/PLUGINS.md`), so the ones installed keep running in the new Oak.
//!
//! Replacing a running executable: Linux renames the new file over it (the
//! running process keeps the old one open). Windows can't overwrite a
//! running .exe but can rename it, so it goes to `<name>.<n>.old`, the new
//! one takes its name, and the leftovers are removed at the next start.

use crate::tools::Head;
use crate::api;
use eframe::egui;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// This build's version: "0.1.276" for a release, "0.1.276-dev" for any
/// other build of commit 276 (see build.rs).
pub(crate) const VERSION: &str = env!("OAK_VERSION");
/// The commit it was built from (short hash).
pub(crate) const COMMIT: &str = env!("OAK_COMMIT");
/// The GitHub repository the releases come from.
const REPO: &str = env!("OAK_UPDATE_REPO");

/// The release asset with this machine's build, if there is one.
const ASSET: Option<&str> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some("oak-windows-x86_64.exe")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("oak-linux-x86_64")
} else {
    None
};

/// How often to look for a new release (well within GitHub's 60 requests
/// an hour without a login), and how soon to look again after a look that
/// failed. The refresh button by the version looks at once.
const CHECK_EVERY: Duration = Duration::from_secs(10 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// A release newer than this build, with this machine's asset.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Release {
    /// "0.1.280" (the tag without its "v").
    pub(crate) version: String,
    pub(crate) notes: String,
    pub(crate) url: String,
    asset: u64,
    sum: Option<u64>,
}

/// Where updating is.
#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) enum Phase {
    /// Up to date, or not known yet.
    #[default]
    None,
    Available(Release),
    /// Downloading it: how far (0 to 1, or below 0 when the size isn't known).
    Downloading(Release, f32),
    /// It is in place of Oak's executable: a restart starts it.
    Ready(Release),
    /// Downloading or installing it failed, and why (a click tries again).
    Failed(Release, String),
}

#[derive(Clone, Default)]
pub(crate) struct Status {
    pub(crate) phase: Phase,
    pub(crate) checking: bool,
    /// When the last look for a release ended, and what it found wrong.
    pub(crate) checked: Option<(Instant, Result<(), String>)>,
    /// When the refresh button last asked for a look (Home says how it went
    /// for a moment after).
    pub(crate) asked: Option<Instant>,
}

static STATUS: Mutex<Status> = Mutex::new(Status { phase: Phase::None, checking: false, checked: None, asked: None });
/// Start the executable again once Oak has closed (`relaunch_if_asked`).
static RESTART: AtomicBool = AtomicBool::new(false);

pub(crate) fn status() -> Status {
    STATUS.lock().unwrap().clone()
}

/// This build is a release (not a local or `-dev` build).
pub(crate) fn is_release() -> bool {
    !VERSION.contains('-')
}

/// Oak's executable, as it was when Oak started (on Windows the running
/// file is renamed away by an update; this path then holds the new one).
fn exe() -> Option<&'static PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok().and_then(|p| p.canonicalize().ok().or(Some(p)))).as_ref()
}

/// At start: notes where Oak's executable is, removes what an earlier
/// update left (Windows), and looks for a release now and then. Not under
/// test, or with `OAK_NO_UPDATE` set, or where there is no build to update to.
pub(crate) fn start(ctx: &egui::Context) {
    let Some(exe) = exe() else { return };
    remove_leftovers(exe);
    if cfg!(test) || std::env::var_os("OAK_NO_UPDATE").is_some() || ASSET.is_none() || REPO.is_empty() {
        return;
    }
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("updates".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        loop {
            let ok = check(&ctx);
            std::thread::sleep(if ok { CHECK_EVERY } else { RETRY_AFTER });
        }
    });
}

/// Looks for a release now (Home's refresh button), in the background.
pub(crate) fn check_soon(ctx: &egui::Context) {
    {
        let mut s = STATUS.lock().unwrap();
        if ASSET.is_none() || REPO.is_empty() || s.checking {
            return;
        }
        s.asked = Some(Instant::now());
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || check(&ctx));
}

/// Looks for a release newer than this build; true when the look worked.
fn check(ctx: &egui::Context) -> bool {
    {
        let mut s = STATUS.lock().unwrap();
        if s.checking {
            return true;
        }
        s.checking = true;
    }
    ctx.request_repaint();
    let found = latest();
    let ok = found.is_ok();
    {
        let mut s = STATUS.lock().unwrap();
        s.checking = false;
        s.checked = Some((Instant::now(), found.as_ref().map(|_| ()).map_err(Clone::clone)));
        if let Ok(found) = found {
            s.phase = next_phase(&s.phase, found);
        }
    }
    ctx.request_repaint();
    ok
}

/// What a look for a release that found `found` makes of `phase`: one being
/// downloaded or put in place stays; a failed one stays failed (with its
/// error) until clicked, unless a newer one came out.
fn next_phase(phase: &Phase, found: Option<Release>) -> Phase {
    match (phase, found) {
        (Phase::Downloading(..) | Phase::Ready(_), _) => phase.clone(),
        (Phase::Failed(f, _), Some(r)) if f.version == r.version => phase.clone(),
        (_, Some(r)) => Phase::Available(r),
        (_, None) => Phase::None,
    }
}

/// The latest release, when it is newer than this build and has a build
/// for this machine. Looks for the machine's GitHub login first (it may
/// have signed in since the last look).
fn latest() -> Result<Option<Release>, String> {
    *TOKEN.lock().unwrap() = github_token();
    let resp = github(&format!("/repos/{REPO}/releases/latest"), "application/vnd.github+json", Duration::from_secs(30));
    let v: serde_json::Value = match resp {
        Ok(r) => r.json().map_err(|e| format!("GitHub's answer couldn't be read: {}", api::error_chain(&e)))?,
        // Signed in: no release yet. Not signed in: GitHub says the same
        // of a private repository.
        Err(e) if e.contains("HTTP 404") && TOKEN.lock().unwrap().is_some() => return Ok(None),
        Err(e) if e.contains("HTTP 404") => return Err(NO_LOGIN.into()),
        Err(e) => return Err(e),
    };
    Ok(release_from(&v, VERSION))
}

const NO_LOGIN: &str = "GitHub shows no release, and this machine has no GitHub login for Oak to use (a private repository needs one). Sign in with `gh auth login` (or let git save its login for github.com, as a push does), or set GITHUB_TOKEN.";

/// The machine's GitHub login, found by the last look for a release.
static TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// A GitHub token this machine already has: `OAK_GITHUB_TOKEN`,
/// `GITHUB_TOKEN` or `GH_TOKEN`, then the GitHub CLI's (`gh auth token`),
/// then the one git keeps for github.com (Git Credential Manager and the
/// like), asked for without any prompt or window.
fn github_token() -> Option<String> {
    let env = ["OAK_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"].into_iter().find_map(|v| std::env::var(v).ok().filter(|t| !t.trim().is_empty()));
    if let Some(t) = env {
        return Some(t.trim().to_string());
    }
    let gh = quiet(std::process::Command::new("gh").args(["auth", "token", "--hostname", "github.com"]), None)
        .map(|out| out.trim().to_string())
        .filter(|t| !t.is_empty() && !t.contains(char::is_whitespace));
    if gh.is_some() {
        return gh;
    }
    let mut git = std::process::Command::new("git");
    git.args(["-c", "credential.interactive=false", "-c", "core.askPass=", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "");
    let out = quiet(&mut git, Some("protocol=https\nhost=github.com\n\n"))?;
    out.lines().find_map(|l| l.strip_prefix("password=")).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// What `cmd` prints, when it runs and succeeds within ten seconds; no
/// console window, stdin from `input` (or nothing).
fn quiet(cmd: &mut std::process::Command, input: Option<&str>) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.unwrap_or_default().as_bytes());
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if start.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// The release GitHub describes as `v`, when it is newer than `current` and
/// has this machine's asset.
fn release_from(v: &serde_json::Value, current: &str) -> Option<Release> {
    let tag = v["tag_name"].as_str()?;
    let version = tag.trim_start_matches('v').to_string();
    if !newer(&version, current) {
        return None;
    }
    let name = ASSET?;
    let assets = v["assets"].as_array()?;
    let id = |n: &str| assets.iter().find(|a| a["name"] == n).and_then(|a| a["id"].as_u64());
    Some(Release {
        version,
        notes: v["body"].as_str().unwrap_or_default().trim().to_string(),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        asset: id(name)?,
        sum: id(&format!("{name}.sha256")),
    })
}

/// A version's numbers, "-dev" and the like left off: "0.1.276-dev" is
/// [0, 1, 276].
fn numbers(v: &str) -> Option<Vec<u64>> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split(['-', '+']).next()?;
    v.split('.').map(|n| n.parse().ok()).collect()
}

/// `candidate` is a later version than `current`. A build of the same
/// commit as a release (a `-dev` build) isn't behind it.
pub(crate) fn newer(candidate: &str, current: &str) -> bool {
    match (numbers(candidate), numbers(current)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// A GET from GitHub's API (`path` from its root), with the machine's
/// GitHub login when it has one (and without, if GitHub turns that down).
/// Errors say "HTTP <code>". A redirect to another host (an asset's
/// download) is followed without the token.
fn github(path: &str, accept: &str, timeout: Duration) -> Result<reqwest::blocking::Response, String> {
    let send = |token: Option<&str>| {
        let mut req = api::http()
            .get(format!("https://api.github.com{path}"))
            .timeout(timeout)
            .header("Accept", accept)
            .header("User-Agent", format!("oak/{VERSION}"))
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().map_err(|e| format!("GitHub couldn't be reached: {}", api::error_chain(&e)))
    };
    let token = TOKEN.lock().unwrap().clone();
    let mut resp = send(token.as_deref())?;
    if resp.status().as_u16() == 401 && token.is_some() {
        // A stale or wrong login: as if there were none.
        *TOKEN.lock().unwrap() = None;
        resp = send(None)?;
    }
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        let why = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().or(v["error"].as_str()).map(str::to_string))
            .unwrap_or_else(|| text.head(200));
        return Err(format!("GitHub: HTTP {}: {why}", status.as_u16()));
    }
    Ok(resp)
}

/// Downloads the release on offer and puts it in place of Oak's executable,
/// in the background (Home's update button).
pub(crate) fn install(ctx: &egui::Context) {
    let release = {
        let mut s = STATUS.lock().unwrap();
        let r = match &s.phase {
            Phase::Available(r) | Phase::Failed(r, _) => r.clone(),
            _ => return,
        };
        s.phase = Phase::Downloading(r.clone(), -1.0);
        r
    };
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let progress = |f: f32| {
            if let Phase::Downloading(_, p) = &mut STATUS.lock().unwrap().phase {
                *p = f;
            }
            ctx.request_repaint();
        };
        let done = download_and_replace(&release, &progress);
        STATUS.lock().unwrap().phase = match done {
            Ok(()) => Phase::Ready(release),
            Err(e) => Phase::Failed(release, e),
        };
        ctx.request_repaint();
    });
}

fn download_and_replace(r: &Release, progress: &dyn Fn(f32)) -> Result<(), String> {
    let exe = exe().ok_or("Oak's executable couldn't be found.")?;
    let sum_id = r.sum.ok_or("The release has no SHA-256 sum for this build.")?;
    let sum = String::from_utf8_lossy(&download(sum_id, &|_| {})?).split_whitespace().next().unwrap_or_default().to_lowercase();
    let bytes = download(r.asset, progress)?;
    let got = api::hex(&Sha256::digest(&bytes));
    if sum.len() != 64 || got != sum {
        return Err(format!("The download doesn't match its SHA-256 sum (expected {sum}, got {got}); nothing was changed."));
    }
    replace_exe(exe, &bytes).map_err(|e| format!("Couldn't put the new version in place of {}: {e}", exe.display()))
}

/// Release asset `id`'s bytes, telling `progress` how far it is.
fn download(id: u64, progress: &dyn Fn(f32)) -> Result<Vec<u8>, String> {
    let mut resp = github(&format!("/repos/{REPO}/releases/assets/{id}"), "application/octet-stream", Duration::from_secs(15 * 60))?;
    let total = resp.content_length().unwrap_or(0);
    let mut bytes = Vec::with_capacity(total as usize);
    let mut buf = vec![0u8; 256 << 10];
    let mut last = Instant::now();
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("The download broke off: {e}"))?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        if total > 0 && last.elapsed() > Duration::from_millis(100) {
            last = Instant::now();
            progress(bytes.len() as f32 / total as f32);
        }
    }
    if total > 0 && (bytes.len() as u64) < total {
        return Err("The download broke off.".into());
    }
    Ok(bytes)
}

/// Puts `bytes` in place of the executable at `exe`, which may be running.
fn replace_exe(exe: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "oak".into());
    let new = exe.with_file_name(format!("{name}.new"));
    std::fs::write(&new, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        // The running one goes aside under a name not in use (an older
        // one may still be running too), and the new one takes its place.
        let old = (0..100).map(|n| exe.with_file_name(format!("{name}.{n}.old"))).find(|p| !p.exists() || std::fs::remove_file(p).is_ok());
        let old = old.ok_or_else(|| std::io::Error::other("no free name for the old executable"))?;
        if let Err(e) = std::fs::rename(exe, &old) {
            let _ = std::fs::remove_file(&new);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&new, exe) {
            let _ = std::fs::rename(&old, exe);
            let _ = std::fs::remove_file(&new);
            return Err(e);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(&new, exe).inspect_err(|_| {
        let _ = std::fs::remove_file(&new);
    })
}

/// Removes the executables earlier updates set aside, and a download left
/// half-written (those still running stay, until a later start).
fn remove_leftovers(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().map(|n| n.to_string_lossy().into_owned())) else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        let leftover = f == format!("{name}.new") || (f.starts_with(&format!("{name}.")) && f.ends_with(".old"));
        if leftover {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Closes Oak to start the new version (the close dialog asks first when
/// conversations are at work; cancelling it calls `cancel_restart`).
pub(crate) fn restart(ctx: &egui::Context) {
    RESTART.store(true, Ordering::SeqCst);
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}

pub(crate) fn cancel_restart() {
    RESTART.store(false, Ordering::SeqCst);
}

/// After Oak has closed and saved everything: starts the executable again
/// (now the new version), with the same arguments, if a restart was asked for.
pub(crate) fn relaunch_if_asked() {
    if !RESTART.load(Ordering::SeqCst) {
        return;
    }
    let Some(exe) = exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(newer("0.1.280", "0.1.276"));
        assert!(newer("v0.1.280", "0.1.276-dev"));
        assert!(newer("0.2.0", "0.1.999"));
        assert!(newer("0.1.10", "0.1.9"), "numbers, not text");
        assert!(!newer("0.1.276", "0.1.276"));
        assert!(!newer("0.1.276", "0.1.276-dev"), "a dev build of a release's commit isn't behind it");
        assert!(!newer("0.1.275", "0.1.276-dev"));
        assert!(!newer("command-latest", "0.1.1"));
        assert!(newer("0.1.1", "unknown"));
    }

    #[test]
    fn a_release_is_offered_only_when_newer_and_built_for_this_machine() {
        let Some(name) = ASSET else { return };
        let v = serde_json::json!({
            "tag_name": "v0.1.300",
            "body": "Notes",
            "html_url": "https://github.com/o/r/releases/tag/v0.1.300",
            "assets": [
                {"id": 1, "name": name},
                {"id": 2, "name": format!("{name}.sha256")},
                {"id": 3, "name": "something-else"},
            ],
        });
        let r = release_from(&v, "0.1.299").expect("newer, with this machine's build");
        assert_eq!((r.version.as_str(), r.asset, r.sum, r.notes.as_str()), ("0.1.300", 1, Some(2), "Notes"));
        assert_eq!(release_from(&v, "0.1.300"), None, "not newer");
        let mut other = v.clone();
        other["assets"] = serde_json::json!([{"id": 3, "name": "something-else"}]);
        assert_eq!(release_from(&other, "0.1.1"), None, "no build for this machine");
    }

    #[test]
    fn a_new_look_keeps_what_is_under_way() {
        let r = |v: &str| Release { version: v.into(), notes: String::new(), url: String::new(), asset: 1, sum: Some(2) };
        assert_eq!(next_phase(&Phase::None, Some(r("1.0.1"))), Phase::Available(r("1.0.1")));
        assert_eq!(next_phase(&Phase::Available(r("1.0.1")), None), Phase::None);
        assert_eq!(next_phase(&Phase::Ready(r("1.0.1")), Some(r("1.0.2"))), Phase::Ready(r("1.0.1")));
        assert_eq!(next_phase(&Phase::Downloading(r("1.0.1"), 0.5), None), Phase::Downloading(r("1.0.1"), 0.5));
        let failed = Phase::Failed(r("1.0.1"), "no".into());
        assert_eq!(next_phase(&failed, Some(r("1.0.1"))), failed);
        assert_eq!(next_phase(&failed, Some(r("1.0.2"))), Phase::Available(r("1.0.2")));
    }

    #[test]
    fn replacing_the_executable_leaves_only_the_new_one() {
        let dir = std::env::temp_dir().join(format!("oak-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "oak.exe" } else { "oak" });
        std::fs::write(&exe, b"old").unwrap();
        replace_exe(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        remove_leftovers(&exe);
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, vec![exe.file_name().unwrap().to_string_lossy().into_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
