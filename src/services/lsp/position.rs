//! Position-encoding negotiation and wire<->native coordinate conversion.
//!
//! LSP positions are `(line, character)` where `character` is counted in the
//! negotiated [`PositionEncoding`] (LSP 3.17). Symora's own coordinates are
//! different: JSON/CLI columns are 1-indexed Unicode SCALARS, and source
//! slicing uses byte offsets. This module is the single boundary that decodes
//! the wire `character` into a symora-native unit, so nothing downstream of
//! `services/lsp` ever sees an encoded offset.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::infra::lsp::protocol::PositionEncoding;

/// Byte offset into `line` for a wire `character` offset in `encoding`.
/// For utf-8 the wire offset already IS a byte offset (floored to a char
/// boundary defensively); for utf-16 the line's chars are walked once,
/// summing UTF-16 code units until the offset is reached.
pub fn encoded_offset_to_byte(encoding: PositionEncoding, line: &str, wire_offset: u32) -> usize {
    let wire = wire_offset as usize;
    match encoding {
        PositionEncoding::Utf8 => floor_char_boundary(line, wire),
        PositionEncoding::Utf16 => {
            let mut units = 0usize;
            for (byte, ch) in line.char_indices() {
                if units >= wire {
                    return byte;
                }
                units += ch.len_utf16();
            }
            line.len()
        }
    }
}

/// 0-indexed Unicode-scalar column for a wire `character` offset in `encoding`
/// — the value the public JSON `column` is built from (after +1).
pub fn encoded_offset_to_scalar(encoding: PositionEncoding, line: &str, wire_offset: u32) -> u32 {
    let wire = wire_offset as usize;
    match encoding {
        PositionEncoding::Utf8 => {
            let byte = floor_char_boundary(line, wire);
            line[..byte].chars().count() as u32
        }
        PositionEncoding::Utf16 => {
            let mut units = 0usize;
            for (scalar, ch) in line.chars().enumerate() {
                if units >= wire {
                    return scalar as u32;
                }
                units += ch.len_utf16();
            }
            line.chars().count() as u32
        }
    }
}

/// Wire `character` offset for a 0-indexed Unicode-scalar column — the
/// symmetric outbound conversion, so a position symora sends after a non-BMP
/// char on the line lands on the column the server expects.
pub fn scalar_to_wire(encoding: PositionEncoding, line: &str, scalar: u32) -> u32 {
    let scalar = scalar as usize;
    match encoding {
        PositionEncoding::Utf8 => line
            .char_indices()
            .nth(scalar)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len()) as u32,
        PositionEncoding::Utf16 => line
            .chars()
            .take(scalar)
            .map(|ch| ch.len_utf16() as u32)
            .sum(),
    }
}

fn floor_char_boundary(s: &str, byte: usize) -> usize {
    if byte >= s.len() {
        return s.len();
    }
    let mut b = byte;
    while !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// Decodes wire positions to symora-native 1-indexed scalar columns, reading
/// and caching each target file's lines so a many-position result does
/// O(distinct files) reads. A file already in hand (the request's own) is
/// seeded via [`PositionConverter::with_content`]; an unreadable or
/// out-of-range target degrades to identity (the wire offset as the column)
/// rather than failing the whole result.
pub struct PositionConverter {
    encoding: PositionEncoding,
    files: HashMap<PathBuf, Option<Vec<String>>>,
}

impl PositionConverter {
    pub fn new(encoding: PositionEncoding) -> Self {
        Self {
            encoding,
            files: HashMap::new(),
        }
    }

    /// Seed the converter with content already read for `file`, avoiding a
    /// re-read for the request's own file.
    pub fn with_content(mut self, file: &Path, content: &str) -> Self {
        self.files
            .insert(file.to_path_buf(), Some(split_lines(content)));
        self
    }

    fn line(&mut self, file: &Path, line0: u32) -> Option<&str> {
        self.files
            .entry(file.to_path_buf())
            .or_insert_with(|| std::fs::read_to_string(file).ok().map(|c| split_lines(&c)))
            .as_ref()
            .and_then(|lines| lines.get(line0 as usize))
            .map(String::as_str)
    }

    /// 0-indexed Unicode-scalar offset for an inbound wire `(line0, character)`
    /// on `file` — decodes a RESULT position (hover/def/refs/folding/selection/
    /// code-action/inlay) for display.
    ///
    /// When the line is unreadable it degrades to `wire_char`. This is exact for
    /// a single-byte (ASCII) line and the request's own file is always seeded, so
    /// a degrade can only affect a CROSS-FILE result whose file became unreadable
    /// mid-request — and even then the result's file and line stay correct, only
    /// the column may be off on a multibyte line. The degrade is deliberately
    /// best-effort: a display column is a recoverable hint, so dropping an
    /// otherwise-valid navigation result would be the worse failure. The
    /// edit-APPLY path makes the opposite, equally deliberate choice via
    /// [`scalar_offset_checked`](Self::scalar_offset_checked) — it FAILS CLOSED,
    /// because a guessed offset there would corrupt the file unrecoverably.
    pub fn scalar_offset(&mut self, file: &Path, line0: u32, wire_char: u32) -> u32 {
        let encoding = self.encoding;
        match self.line(file, line0) {
            Some(line) => encoded_offset_to_scalar(encoding, line, wire_char),
            None => wire_char,
        }
    }

    /// 1-indexed scalar column plus a flag disclosing whether it was DEGRADED —
    /// i.e. the target line was unreadable so the wire offset was used verbatim
    /// instead of transcoded. `true` can only arise for a cross-file result on a
    /// file that became unreadable mid-request (the request's own file is always
    /// seeded), and even then only its column may be off on a multibyte line.
    /// Callers thread the flag onto the emitted location so an agent can tell a
    /// transcoded column from a wire-offset guess, rather than the read path
    /// silently presenting a guess as truth (CLAUDE.md invariant 4) — the
    /// disclosure analogue of the edit path's fail-closed `scalar_offset_checked`.
    pub fn scalar_column_disclosed(
        &mut self,
        file: &Path,
        line0: u32,
        wire_char: u32,
    ) -> (u32, bool) {
        let encoding = self.encoding;
        match self.line(file, line0) {
            Some(line) => (
                encoded_offset_to_scalar(encoding, line, wire_char) + 1,
                false,
            ),
            None => (wire_char + 1, true),
        }
    }

    /// Like `scalar_offset` but FAILS CLOSED instead of degrading: returns
    /// `None` when the target line cannot be read and `wire_char > 0`. The
    /// edit-apply path uses this so an edit is never sliced at a guessed byte
    /// offset on a multibyte line. A `wire_char` of 0 needs no line (it maps to
    /// scalar 0 in every encoding), so a line-start or end-of-file insertion
    /// still succeeds.
    pub fn scalar_offset_checked(
        &mut self,
        file: &Path,
        line0: u32,
        wire_char: u32,
    ) -> Option<u32> {
        if wire_char == 0 {
            return Some(0);
        }
        let encoding = self.encoding;
        self.line(file, line0)
            .map(|line| encoded_offset_to_scalar(encoding, line, wire_char))
    }
}

fn split_lines(content: &str) -> Vec<String> {
    content.lines().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `let x = "😀";` — 😀 (U+1F600) is 4 UTF-8 bytes, 2 UTF-16 code units, 1
    // scalar. The `"` after it sits at byte 13, utf-16 unit 11, scalar 10.
    const LINE: &str = "let x = \"😀\";";

    #[test]
    fn utf16_offset_converts_to_byte_and_scalar() {
        // wire utf-16 char 11 == the closing quote after the emoji.
        assert_eq!(
            encoded_offset_to_byte(PositionEncoding::Utf16, LINE, 11),
            13
        );
        assert_eq!(
            encoded_offset_to_scalar(PositionEncoding::Utf16, LINE, 11),
            10
        );
    }

    #[test]
    fn utf8_offset_converts_to_byte_and_scalar() {
        // wire utf-8 char 13 (a byte offset) == the same closing quote.
        assert_eq!(encoded_offset_to_byte(PositionEncoding::Utf8, LINE, 13), 13);
        assert_eq!(
            encoded_offset_to_scalar(PositionEncoding::Utf8, LINE, 13),
            10
        );
    }

    #[test]
    fn cross_encoding_equivalence_on_the_same_position() {
        // The same logical position yields the same scalar column regardless of
        // which encoding the server negotiated.
        let s16 = encoded_offset_to_scalar(PositionEncoding::Utf16, LINE, 11);
        let s8 = encoded_offset_to_scalar(PositionEncoding::Utf8, LINE, 13);
        assert_eq!(s16, s8);
    }

    #[test]
    fn outbound_is_the_inverse_of_inbound() {
        // scalar 10 (the quote) round-trips back to each encoding's wire offset.
        assert_eq!(scalar_to_wire(PositionEncoding::Utf16, LINE, 10), 11);
        assert_eq!(scalar_to_wire(PositionEncoding::Utf8, LINE, 10), 13);
    }

    #[test]
    fn ascii_lines_are_unchanged_in_both_encodings() {
        let ascii = "fn main() {}";
        for enc in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            assert_eq!(encoded_offset_to_byte(enc, ascii, 3), 3);
            assert_eq!(encoded_offset_to_scalar(enc, ascii, 3), 3);
            assert_eq!(scalar_to_wire(enc, ascii, 3), 3);
        }
    }

    #[test]
    fn disclosed_column_flags_a_degraded_unreadable_target() {
        // A cross-file result whose file cannot be read: the column is the raw
        // wire offset (+1) and the degrade is DISCLOSED, so an agent can tell it
        // from a transcoded value rather than trusting a guess.
        let mut conv = PositionConverter::new(PositionEncoding::Utf16);
        let (col, degraded) = conv.scalar_column_disclosed(Path::new("/nonexistent/file.rs"), 0, 4);
        assert_eq!(col, 5);
        assert!(degraded);
    }

    #[test]
    fn disclosed_column_is_not_degraded_for_a_readable_multibyte_line() {
        // A readable non-BMP line transcodes correctly and is NOT flagged: utf-16
        // unit 11 (the quote after 😀) is scalar column 11, trustworthy.
        let path = Path::new("seeded.rs");
        let mut conv = PositionConverter::new(PositionEncoding::Utf16).with_content(path, LINE);
        let (col, degraded) = conv.scalar_column_disclosed(path, 0, 11);
        assert_eq!(col, 11);
        assert!(!degraded);
    }
}
