//! Distribution helpers — release downloads, checksums, archive extraction,
//! skill source fetching, lifecycle paths. All network I/O shells out to
//! `curl`, all archive work to `tar`, all verification to `sha256sum` /
//! `shasum` / `gh attestation verify`. Keeping these out of the dependency
//! tree means the binary stays focused on code intelligence and the
//! verification path is the same one users would invoke by hand.

pub mod archive;
pub mod paths;
pub mod process;
pub mod release;
pub mod skill;
pub mod target;
pub mod tempdir;
pub mod verify;

pub use archive::extract_symora_archive;
pub use paths::{config_dir, daemon_dir, display, home, skill_dir};
pub use process::{have, run_streaming, run_streaming_in};
pub use release::{ReleaseAsset, download_release, is_valid_version, resolve_latest_version};
pub use skill::{
    SKILL_NAME, SkillOrigin, SkillSource, SkillVersionDelta, compare_skill_versions,
    prepare_skill_source, read_skill_version,
};
pub use target::current_target;
pub use tempdir::TempDir;
pub use verify::{verify_attestation, verify_sha256};
