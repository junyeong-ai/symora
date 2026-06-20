//! Thin process-spawning helpers used by every dist op.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

/// Run a command, inherit stdout/stderr to the user, and bubble up a
/// non-zero exit as an error.
pub fn run_streaming(program: &str, args: &[&str]) -> Result<()> {
    run_streaming_in(program, args, None)
}

/// Same as [`run_streaming`] but pinned to a working directory.
pub fn run_streaming_in(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn '{program}'"))?;
    if !status.success() {
        return Err(anyhow!(
            "'{program}' exited with status {}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// Run a command silently, returning stdout on success.
pub fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to spawn '{program}'"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "'{program}' exited with status {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .with_context(|| format!("'{program}' produced non-UTF-8 stdout"))
}

/// Whether a command is available on `$PATH`.
pub fn have(program: &str) -> bool {
    which(program).is_some()
}

/// Resolve a command via `$PATH` without depending on the `which` crate.
/// Returns a path only when it is an EXECUTABLE file — "available to run", not
/// merely present — so `have` agrees with the spawn path (`resolve_command`
/// applies the same `is_executable_file` check) and a non-executable file on
/// PATH is never reported as installed.
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// `curl -fsSL --retry 3 --retry-delay 2 --retry-connrefused -o <out> <url>`
pub fn curl_download(url: &str, out: &Path) -> Result<()> {
    if !have("curl") {
        return Err(anyhow!(
            "curl is required for network operations (https://curl.se/download.html)"
        ));
    }
    let out_str = out
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", out.display()))?;
    run_streaming(
        "curl",
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
            "--retry-delay",
            "2",
            "--retry-connrefused",
            "--output",
            out_str,
            url,
        ],
    )
    .with_context(|| format!("download failed: {url}"))
}

/// `curl -fsSL --head` returning the effective URL after redirects.
/// Used to follow `/releases/latest` to `/releases/tag/vX.Y.Z` without
/// hitting GitHub's API rate limits.
pub fn curl_resolve_redirect(url: &str) -> Result<String> {
    if !have("curl") {
        return Err(anyhow!("curl is required to resolve {url}"));
    }
    let out = run_capture(
        "curl",
        &[
            "--fail",
            "--silent",
            "--location",
            "--head",
            "--output",
            "/dev/null",
            "--write-out",
            "%{url_effective}",
            url,
        ],
    )?;
    Ok(out.trim().to_string())
}
