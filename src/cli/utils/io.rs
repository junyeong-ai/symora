//! Tiny file-IO helpers used by commands that splice line excerpts into
//! responses (refs, context, diagnostics).

use std::io::{BufRead, BufReader};
use std::path::Path;

/// Read a single line from a file (trimmed). 1-indexed.
pub fn read_line_at(path: &Path, line: u32) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    reader
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .transpose()?
        .map(|s| s.trim().to_string())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Line not found"))
}

/// Read N lines around `target_line` (1-indexed), prefixed with their line
/// numbers — e.g. `"10: fn foo() {\n11:   bar()\n12: }"`.
pub fn read_lines_around(path: &Path, target_line: u32, context: usize) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let target = target_line.saturating_sub(1) as usize;
    let start = target.saturating_sub(context);
    let end = target + context;

    let lines: Vec<String> = reader
        .lines()
        .enumerate()
        .skip(start)
        .take(end - start + 1)
        .filter_map(|(i, line)| line.ok().map(|l| format!("{}: {}", i + 1, l)))
        .collect();

    if lines.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Lines not found",
        ));
    }

    Ok(lines.join("\n"))
}
