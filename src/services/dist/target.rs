//! Compile-time release target identifier. The strings here must match the
//! `target` matrix in `.github/workflows/release.yml` so that
//! `symora-v<ver>-<target>.tar.gz` resolves to a real asset.

use anyhow::{Result, anyhow};

/// The release-target triple corresponding to the running binary.
pub fn current_target() -> Result<&'static str> {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        Ok("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        Ok("aarch64-unknown-linux-gnu")
    } else {
        Err(anyhow!(
            "no published release target for this platform (arch={}, os={}). \
             Build from source: 'cargo install --path .' inside a checkout.",
            std::env::consts::ARCH,
            std::env::consts::OS,
        ))
    }
}
