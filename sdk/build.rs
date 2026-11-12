//! What `abi::Stamp` says about the build: a digest of this crate's source
//! and of the patched crates' (so Oak and a plugin are known to share one
//! definition of everything that crosses between them), the compiler, and
//! the target.

use std::path::{Path, PathBuf};
use std::process::Command;

/// FNV-1a.
fn mix(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

/// Every file under `dir`, in name order.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // This crate, and the two patched crates beside it (`vendor/README.md`).
    let roots = [here.join("src"), here.join("../vendor/parking_lot_core/src"), here.join("../vendor/epaint/src")];
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for root in &roots {
        println!("cargo:rerun-if-changed={}", root.display());
        let mut list = vec![];
        files(root, &mut list);
        for f in list {
            // The path under its root and the text, line ends aside (a
            // checkout on Windows may have CRLF).
            let name = f.strip_prefix(root).unwrap_or(&f).to_string_lossy().replace('\\', "/");
            mix(&mut h, name.as_bytes());
            mix(&mut h, &[0]);
            let data: Vec<u8> = std::fs::read(&f).unwrap_or_default().into_iter().filter(|b| *b != b'\r').collect();
            mix(&mut h, &data);
            mix(&mut h, &[0]);
        }
    }
    println!("cargo:rustc-env=OAK_SDK_SOURCE={h:016x}");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(rustc).arg("-V").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    println!("cargo:rustc-env=OAK_SDK_RUSTC={version}");
    println!("cargo:rustc-env=OAK_SDK_TARGET={}", std::env::var("TARGET").unwrap_or_default());
    println!("cargo:rerun-if-changed=build.rs");
}
