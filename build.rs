//! Bundles the system prompts: every file in prompts/ but tools.md and
//! chat.md (a chat's whole prompt, `agent::prompt::CHAT`) is a role
//! (`agent::prompt::ROLES`), named after its file with the first letter
//! capitalized, that the New screen offers; super.md, a super agent's
//! (`agent::prompt::SUPER_ROLE`), is offered only as its Super agent
//! choice. Normal comes first, as the default; the others follow in
//! alphabetical order. Also bundles ripgrep (`ripgrep()`)
//! and sets the version (`version()`).

use std::fmt::Write;
use std::io::Read;
use std::path::{Path, PathBuf};

fn main() {
    version();
    ripgrep();
    println!("cargo:rerun-if-changed=prompts");
    let dir = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("prompts");
    let mut roles: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .expect("reading prompts/")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
        .filter(|(id, _)| id != "tools" && id != "chat" && !id.is_empty() && !id.starts_with('.'))
        .collect();
    roles.sort_by(|a, b| (a.0 != "normal", &a.0).cmp(&(b.0 != "normal", &b.0)));
    assert!(!roles.is_empty(), "prompts/ has no role besides tools.md");
    let mut out = String::from("/// The roles, from prompts/ (made by build.rs).\npub(crate) const ROLES: &[Role] = &[\n");
    for (id, path) in &roles {
        let mut name = id.chars();
        let name: String = name.next().map(|c| c.to_uppercase().chain(name).collect()).unwrap_or_default();
        let path = path.to_str().expect("a prompt's path isn't UTF-8");
        // (A super agent's role is the New screen's Super agent, not a
        // System prompt choice; its tabs have Oak's tree.)
        let (name, icon, offered) = if id == "super" { ("Super agent".to_string(), "\u{f1bb}", false) } else { (name, "", true) };
        writeln!(out, "    Role {{ id: {id:?}, name: {name:?}, text: include_str!({path:?}), icon: {icon:?}, offered: {offered} }},").unwrap();
    }
    out.push_str("];\n");
    let file = Path::new(&std::env::var("OUT_DIR").unwrap()).join("roles.rs");
    std::fs::write(file, out).unwrap();
}

/// Oak's version, for the version shown on Home and the updater
/// (`src/update.rs`): MAJOR.MINOR from Cargo.toml and, as the patch number,
/// the number of commits up to HEAD, so every commit on master is a version of
/// its own and a build of any commit can be told apart from the release
/// made of it. The release workflow sets `OAK_RELEASE=1`; any other build
/// is marked `-dev`. `OAK_BUILD_VERSION` overrides it all (to try the
/// updater against an older version, say).
///
/// Sets `OAK_VERSION`, `OAK_COMMIT` (short hash, or "unknown") and
/// `OAK_UPDATE_REPO` (owner/name on GitHub the updater looks at: Oak's
/// public repository, wherever the build is made, unless `OAK_UPDATE_REPO`
/// names another; "none" builds one that looks for no updates).
fn version() {
    for var in ["OAK_RELEASE", "OAK_BUILD_VERSION", "OAK_UPDATE_REPO"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // A new commit, checkout or fetch moves HEAD or the branch it is on.
    let mut watch = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(head_ref);
    }
    for w in watch {
        if let Some(p) = git(&["rev-parse", "--git-path", &w])
            && Path::new(&p).exists()
        {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".into());
    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let release = std::env::var("OAK_RELEASE").is_ok_and(|v| v == "1");
    let version = match std::env::var("OAK_BUILD_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            let (major, minor) = (std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(), std::env::var("CARGO_PKG_VERSION_MINOR").unwrap());
            format!("{major}.{minor}.{count}{}", if release { "" } else { "-dev" })
        }
    };
    let repo = match std::env::var("OAK_UPDATE_REPO").map(|r| r.trim().to_string()) {
        Ok(r) if r == "none" => String::new(),
        Ok(r) if !r.is_empty() => r,
        // Not where the build was made (GITHUB_REPOSITORY): the private
        // repository's CI builds Oak too, to load its plugins into.
        _ => UPDATE_REPO.to_string(),
    };
    println!("cargo:rustc-env=OAK_VERSION={version}");
    println!("cargo:rustc-env=OAK_COMMIT={commit}");
    println!("cargo:rustc-env=OAK_UPDATE_REPO={repo}");
}

/// Where Oak's releases are (`.github/workflows/client.yml` publishes them).
const UPDATE_REPO: &str = "thooton/oak_public";

const RG_REPO: &str = "https://github.com/BurntSushi/ripgrep";

/// Bundles the latest ripgrep release (`src/toolbin.rs` puts it on the
/// commands' PATH): writes `ripgrep.rs` to OUT_DIR with `RG_VERSION` and
/// `RG` (the executable's bytes, empty when none could be had).
///
/// The latest tag comes from where `releases/latest` redirects (no API, so
/// no rate limit), the archive for the target with curl, checked against
/// the release's `.sha256`. Each version is kept in OUT_DIR, so a rebuild
/// downloads nothing, and an offline one uses the newest kept. The script
/// runs again with every commit (`version()`), which is when a newer
/// release is picked up. `OAK_RIPGREP` names an rg executable to bundle
/// instead (an offline build, a pinned version). Without either, a release
/// build (`OAK_RELEASE=1`) fails; any other bundles none, with a warning.
fn ripgrep() {
    println!("cargo:rerun-if-env-changed=OAK_RIPGREP");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let found = match std::env::var_os("OAK_RIPGREP").filter(|p| !p.is_empty()) {
        Some(p) => {
            let p = PathBuf::from(p);
            println!("cargo:rerun-if-changed={}", p.display());
            assert!(p.is_file(), "OAK_RIPGREP: {} isn't a file", p.display());
            Ok(("custom".to_string(), p))
        }
        None => download_ripgrep(&out.join("ripgrep"), windows),
    };
    let code = match found {
        Ok((version, exe)) => {
            let exe = exe.canonicalize().unwrap_or(exe);
            let exe = exe.to_str().expect("ripgrep's path isn't UTF-8");
            format!("pub(crate) const RG_VERSION: &str = {version:?};\npub(crate) const RG: &[u8] = include_bytes!({exe:?});\n")
        }
        Err(e) => {
            if std::env::var("OAK_RELEASE").is_ok_and(|v| v == "1") {
                panic!("Couldn't bundle ripgrep: {e} (OAK_RIPGREP can name an rg executable)");
            }
            println!("cargo:warning=Built without ripgrep: {e} (OAK_RIPGREP can name an rg executable)");
            "pub(crate) const RG_VERSION: &str = \"\";\npub(crate) const RG: &[u8] = &[];\n".to_string()
        }
    };
    std::fs::write(out.join("ripgrep.rs"), code).unwrap();
}

/// The latest release's rg for the target, kept in `cache/<version>/`:
/// (version, path).
fn download_ripgrep(cache: &Path, windows: bool) -> Result<(String, PathBuf), String> {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    // Linux takes the static (musl) builds: x86_64 has no other.
    let triple = match (os.as_str(), arch.as_str()) {
        ("linux", "x86_64" | "aarch64") => format!("{arch}-unknown-linux-musl"),
        ("windows", "x86_64" | "aarch64") => format!("{arch}-pc-windows-msvc"),
        ("macos", "x86_64" | "aarch64") => format!("{arch}-apple-darwin"),
        _ => return Err(format!("ripgrep publishes no build for {arch} {os}")),
    };
    let exe_name = if windows { "rg.exe" } else { "rg" };
    let kept = |version: &str| Some(cache.join(version).join(exe_name)).filter(|p| p.is_file());
    let version = match latest_ripgrep() {
        Ok(v) => v,
        Err(e) => {
            // Offline: the newest version kept, if any.
            let mut versions: Vec<String> = std::fs::read_dir(cache).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|v| kept(v).is_some()).collect();
            versions.sort_by_key(|v| v.split('.').map(|n| n.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>());
            let v = versions.pop().ok_or(e)?;
            println!("cargo:warning=Couldn't look for the latest ripgrep; bundling {v}, built before");
            v
        }
    };
    if let Some(p) = kept(&version) {
        return Ok((version, p));
    }
    let asset = format!("ripgrep-{version}-{triple}.{}", if windows { "zip" } else { "tar.gz" });
    let url = format!("{RG_REPO}/releases/download/{version}/{asset}");
    let archive = curl(&url)?;
    // `<hash>  <name>`, or CertUtil's words around the hash on Windows.
    let sums = String::from_utf8_lossy(&curl(&format!("{url}.sha256"))?).to_lowercase();
    let want = sums.split(|c: char| !c.is_ascii_hexdigit()).find(|w| w.len() == 64).ok_or_else(|| format!("{asset}.sha256 has no SHA-256"))?.to_string();
    let got: String = {
        use sha2::Digest;
        sha2::Sha256::digest(&archive).iter().map(|b| format!("{b:02x}")).collect()
    };
    if got != want {
        return Err(format!("{asset} has SHA-256 {got}, its .sha256 says {want}"));
    }
    let bytes = if windows { from_zip(&archive, exe_name) } else { from_tar_gz(&archive, exe_name) }
        .ok_or_else(|| format!("{asset} has no {exe_name}"))?;
    let dir = cache.join(&version);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = dir.join(exe_name);
    let tmp = dir.join(format!("{exe_name}.tmp"));
    std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path)).map_err(|e| format!("writing {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    // A build for this machine can try it.
    if std::env::var("HOST") == std::env::var("TARGET") {
        let ok = std::process::Command::new(&path).arg("--version").output().is_ok_and(|o| o.status.success());
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("the rg from {asset} doesn't run"));
        }
    }
    Ok((version, path))
}

/// The tag `releases/latest` redirects to, e.g. 15.2.0.
fn latest_ripgrep() -> Result<String, String> {
    let head = std::process::Command::new("curl")
        .args(["-sSI", "--max-time", "20", &format!("{RG_REPO}/releases/latest")])
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    let head = String::from_utf8_lossy(&head.stdout);
    head.lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim().eq_ignore_ascii_case("location").then(|| value.trim().rsplit('/').next().unwrap_or("").to_string())
        })
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .ok_or_else(|| "couldn't find ripgrep's latest release (offline?)".to_string())
}

/// A download's bytes (redirects followed, a failure an error).
fn curl(url: &str) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new("curl")
        .args(["-sSfL", "--retry", "2", "--max-time", "120", url])
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("downloading {url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// The file named `name` (in any folder) of a .tar.gz.
fn from_tar_gz(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut tar = vec![];
    flate2::read::GzDecoder::new(archive).read_to_end(&mut tar).ok()?;
    let field = |b: &[u8]| String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(b)).into_owned();
    let mut at = 0;
    while at + 512 <= tar.len() {
        let h = &tar[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let size = usize::from_str_radix(field(&h[124..136]).trim(), 8).ok()?;
        let path = if &h[257..262] == b"ustar" && h[345] != 0 { format!("{}/{}", field(&h[345..500]), field(&h[..100])) } else { field(&h[..100]) };
        let data = at + 512;
        if matches!(h[156], b'0' | 0) && path.rsplit('/').next() == Some(name) {
            return tar.get(data..data + size).map(<[u8]>::to_vec);
        }
        at = data + size.div_ceil(512) * 512;
    }
    None
}

/// The file named `name` (in any folder) of a .zip, from its central
/// directory (local headers may leave the sizes to a trailing descriptor).
fn from_zip(zip: &[u8], name: &str) -> Option<Vec<u8>> {
    let u16_at = |i: usize| Some(u16::from_le_bytes(zip.get(i..i + 2)?.try_into().ok()?) as usize);
    let u32_at = |i: usize| Some(u32::from_le_bytes(zip.get(i..i + 4)?.try_into().ok()?) as usize);
    let end = (0..zip.len().saturating_sub(21)).rev().find(|&i| zip[i..].starts_with(b"PK\x05\x06"))?;
    let (count, mut at) = (u16_at(end + 10)?, u32_at(end + 16)?);
    for _ in 0..count {
        if !zip.get(at..)?.starts_with(b"PK\x01\x02") {
            return None;
        }
        let (method, packed, size) = (u16_at(at + 10)?, u32_at(at + 20)?, u32_at(at + 24)?);
        let (name_len, extra_len, comment_len, local) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?, u32_at(at + 42)?);
        let path = String::from_utf8_lossy(zip.get(at + 46..at + 46 + name_len)?);
        if path.rsplit(['/', '\\']).next() == Some(name) {
            let data = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let packed = zip.get(data..data + packed)?;
            return match method {
                0 => Some(packed.to_vec()),
                8 => {
                    let mut out = Vec::with_capacity(size);
                    flate2::read::DeflateDecoder::new(packed).read_to_end(&mut out).ok()?;
                    (out.len() == size).then_some(out)
                }
                _ => None,
            };
        }
        at += 46 + name_len + extra_len + comment_len;
    }
    None
}
