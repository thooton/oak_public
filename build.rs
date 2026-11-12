//! Bundles the system prompts: every file in prompts/ but tools.md is a role
//! the New screen offers (`agent::prompt::ROLES`), named after its file with
//! the first letter capitalized. Normal comes first, as the default; the
//! others follow in alphabetical order.

use std::fmt::Write;
use std::path::Path;

fn main() {
    version();
    println!("cargo:rerun-if-changed=prompts");
    let dir = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("prompts");
    let mut roles: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .expect("reading prompts/")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
        .filter(|(id, _)| id != "tools" && !id.is_empty() && !id.starts_with('.'))
        .collect();
    roles.sort_by(|a, b| (a.0 != "normal", &a.0).cmp(&(b.0 != "normal", &b.0)));
    assert!(!roles.is_empty(), "prompts/ has no role besides tools.md");
    let mut out = String::from("/// The roles, from prompts/ (made by build.rs).\npub(crate) const ROLES: &[Role] = &[\n");
    for (id, path) in &roles {
        let mut name = id.chars();
        let name: String = name.next().map(|c| c.to_uppercase().chain(name).collect()).unwrap_or_default();
        let path = path.to_str().expect("a prompt's path isn't UTF-8");
        writeln!(out, "    Role {{ id: {id:?}, name: {name:?}, text: include_str!({path:?}), icon: \"\", offered: true }},").unwrap();
    }
    out.push_str("];\n");
    let file = Path::new(&std::env::var("OUT_DIR").unwrap()).join("roles.rs");
    std::fs::write(file, out).unwrap();
}

/// Oak's version, for the version shown on Home and the updater
/// (`src/update.rs`): MAJOR.MINOR from Cargo.toml and, as the patch number,
/// the number of commits up to HEAD, so every commit on main is a version of
/// its own and a build of any commit can be told apart from the release
/// made of it. The release workflow sets `OAK_RELEASE=1`; any other build
/// is marked `-dev`. `OAK_BUILD_VERSION` overrides it all (to try the
/// updater against an older version, say).
///
/// Sets `OAK_VERSION`, `OAK_COMMIT` (short hash, or "unknown") and
/// `OAK_UPDATE_REPO` (owner/name on GitHub the updater looks at).
fn version() {
    for var in ["OAK_RELEASE", "OAK_BUILD_VERSION", "OAK_UPDATE_REPO", "GITHUB_REPOSITORY"] {
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
    let repo = std::env::var("OAK_UPDATE_REPO")
        .or_else(|_| std::env::var("GITHUB_REPOSITORY"))
        .ok()
        .filter(|r| !r.trim().is_empty())
        // (None: a build that doesn't say where its releases are looks
        // for no updates, `src/update.rs`.)
        .unwrap_or_default();
    println!("cargo:rustc-env=OAK_VERSION={version}");
    println!("cargo:rustc-env=OAK_COMMIT={commit}");
    println!("cargo:rustc-env=OAK_UPDATE_REPO={repo}");
}
