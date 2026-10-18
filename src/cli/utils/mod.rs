//! Cross-cutting CLI helpers, grouped by concern:
//!   - `refs`: aggregate LSP `find_references` output into per-test /
//!     per-module statistics.
//!   - `symbol_nav`: position-driven walks over a symbol tree.
//!   - `io`: read line snippets from disk for `--snippet` output.
//!   - `signature`: heuristic `fn xxx(...)` extraction from a body.

mod io;
mod refs;
mod signature;
mod symbol_nav;
pub mod ui;

pub use io::{read_line_at, read_lines_around};
pub use refs::{RefsClassification, extract_module};
pub use signature::extract_signature;
pub use symbol_nav::{
    AnchorResolution, SymbolResolution, ambiguity_hint, column_addressed_symbol, declared_with,
    enclosing_callable, find_named_at_position, find_symbol_at_position, holder_of,
    line_addressed_symbol, members_at_deletion, members_at_line, symbols_declared_on_line,
};

/// Encode one argument for a POSIX shell without expansion or word splitting.
pub fn shell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::shell_arg;

    #[test]
    fn shell_arguments_round_trip_without_expansion() {
        let values = [
            "",
            "plain",
            "a path with spaces",
            "O'Reilly",
            "$(printf expanded);`printf expanded`",
            "first\nsecond",
            "--help",
        ];
        let command = format!(
            "printf '%s\\0' {}",
            values
                .iter()
                .map(|value| shell_arg(value))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let output = std::process::Command::new("sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert!(output.status.success());
        let expected: Vec<u8> = values
            .iter()
            .flat_map(|value| value.bytes().chain(std::iter::once(0)))
            .collect();
        assert_eq!(output.stdout, expected);
    }
}
