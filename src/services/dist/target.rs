//! Compile-time release target identifier. The strings here must match the
//! `target` matrix in `.github/workflows/release.yml` so that
//! `symora-v<ver>-<target>.tar.gz` resolves to a real asset.

use anyhow::{Result, anyhow};

/// The release-target triple corresponding to the running binary.
pub fn current_target() -> Result<&'static str> {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Ok("x86_64-unknown-linux-gnu")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Ok("aarch64-unknown-linux-gnu")
    } else {
        Err(anyhow!(
            "no published release target for this platform (arch={}, os={}, abi={}). \
             Build from source: 'cargo install --path .' inside a checkout.",
            std::env::consts::ARCH,
            std::env::consts::OS,
            current_abi(),
        ))
    }
}

/// The ABI the running binary was built against. `std::env::consts` does not
/// expose it, and it is the difference between a release asset that runs and
/// one that does not.
const fn current_abi() -> &'static str {
    if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_names_an_asset_the_release_workflow_builds() {
        let workflow = include_str!("../../../.github/workflows/release.yml");
        match current_target() {
            Ok(target) => assert!(
                workflow.contains(&format!("- target: {target}")),
                "{target} is not built by the release workflow"
            ),
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains("Build from source"), "{message}");
            }
        }
    }

    #[test]
    fn an_unpublished_abi_is_named_in_the_refusal() {
        assert!(["musl", "gnu", "msvc", "none"].contains(&current_abi()));
    }
}
