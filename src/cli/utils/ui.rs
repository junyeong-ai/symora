//! TTY-aware prompts and progress for lifecycle commands (`setup`, `self`).
//!
//! Output goes to stderr so structured JSON on stdout stays clean.

use std::io::{self, BufRead, Write};

/// Whether stdin is connected to a terminal. When false, every prompt
/// resolves to the supplied default — this is what makes
/// `curl … | bash -s -- symora setup --yes` behave predictably.
pub fn stdin_is_tty() -> bool {
    is_tty(libc_stdin_fd())
}

fn libc_stdin_fd() -> i32 {
    0
}

#[cfg(unix)]
fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(not(unix))]
fn is_tty(_fd: i32) -> bool {
    false
}

/// Prompt for free-form input with a default.
pub fn prompt(prompt: &str, default: &str, assume_yes: bool) -> String {
    if assume_yes || !stdin_is_tty() {
        return default.to_string();
    }
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let _ = write!(handle, "{prompt}");
    let _ = handle.flush();
    drop(handle);

    let mut buf = String::new();
    if io::stdin().lock().read_line(&mut buf).is_err() {
        return default.to_string();
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Prompt for a yes/no answer. `default_yes` controls both the displayed
/// `[Y/n]` vs `[y/N]` hint and the no-input fallback.
pub fn confirm(question: &str, default_yes: bool, assume_yes: bool) -> bool {
    if assume_yes {
        return default_yes;
    }
    if !stdin_is_tty() {
        return default_yes;
    }
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let answer = prompt(&format!("{question} {suffix}: "), "", false);
    if answer.is_empty() {
        return default_yes;
    }
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "1" | "true"
    )
}

/// Print a structured stderr progress line. `kind` is rendered as a short tag
/// so progress reads cleanly without ANSI when piped.
pub fn step(kind: Step, msg: impl AsRef<str>) {
    let tag = match kind {
        Step::Info => "·",
        Step::Run => "→",
        Step::Ok => "✓",
        Step::Warn => "!",
        Step::Skip => "—",
    };
    eprintln!("  {tag} {}", msg.as_ref());
}

/// Print a section heading before a group of related steps.
pub fn section(title: impl AsRef<str>) {
    eprintln!();
    eprintln!("• {}", title.as_ref());
}

#[derive(Copy, Clone)]
pub enum Step {
    Info,
    Run,
    Ok,
    Warn,
    Skip,
}
