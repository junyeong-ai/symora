//! Stamps the binary with an identity derived from what makes it behave
//! the way it does.
//!
//! Two symora processes speak a wire format and a feature set defined by
//! this crate's sources, so they may only talk when both were built from
//! the same ones. The package version cannot say that — a daemon left over
//! from an earlier build of the same version would pass — and the
//! executable's size and timestamp overstate it, moving when nothing about
//! the wire did. Hashing the sources answers the question actually asked:
//! identical sources mean identical behavior and one identity.
//!
//! The set is deliberately wider than what rustc compiles — every Rust file
//! in the tree, reached by the module graph or not, and every byte of it
//! including comments. Narrowing it would mean deciding which edits are
//! behavioral, and each way of deciding wrong costs differently: an identity
//! that counts too much restarts a daemon that would have been fine, while
//! one that counts too little lets two processes that disagree about the
//! wire believe they match.

use std::fs;
use std::path::Path;

/// The roots walked for Rust sources and manifests. Everything else under
/// `src` — module guides, editor droppings — is excluded by extension
/// rather than by name, so a new kind of neighbour costs nothing.
const SOURCE_ROOTS: &[&str] = &["src", "Cargo.toml", "Cargo.lock", "build.rs"];

fn main() {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for root in SOURCE_ROOTS {
        println!("cargo:rerun-if-changed={root}");
        absorb_path(&mut hash, Path::new(root));
    }

    // What the same sources compile INTO also decides behavior: an
    // optional feature adds a command, a target changes the ABI two
    // processes would meet over, and a profile decides which of the two
    // binaries in a checkout a developer just started.
    for (key, value) in std::env::vars() {
        if key.starts_with("CARGO_FEATURE_") || key == "TARGET" || key == "PROFILE" {
            absorb_bytes(&mut hash, key.as_bytes());
            absorb_bytes(&mut hash, value.as_bytes());
        }
    }

    println!("cargo:rustc-env=SYMORA_BUILD_ID={hash:016x}");
}

fn absorb_path(hash: &mut u64, path: &Path) {
    if path.is_dir() {
        let mut entries: Vec<_> = match fs::read_dir(path) {
            Ok(entries) => entries.filter_map(Result::ok).map(|e| e.path()).collect(),
            Err(_) => return,
        };
        entries.sort();
        for entry in entries {
            absorb_path(hash, &entry);
        }
        return;
    }
    let compiled = path.extension().is_some_and(|ext| ext == "rs")
        || path
            .file_name()
            .is_some_and(|name| name == "Cargo.toml" || name == "Cargo.lock" || name == "build.rs");
    if !compiled {
        return;
    }
    absorb_bytes(hash, path.to_string_lossy().as_bytes());
    if let Ok(bytes) = fs::read(path) {
        absorb_bytes(hash, &bytes);
    }
}

fn absorb_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}
