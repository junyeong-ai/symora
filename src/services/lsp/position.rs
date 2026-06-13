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

    /// 0-indexed Unicode-scalar offset for a wire `(line0, character)` on
    /// `file` — for an edit range, whose column edit.rs later slices by char.
    /// Identity-degrades to `character` if the line is unreadable.
    pub fn scalar_offset(&mut self, file: &Path, line0: u32, wire_char: u32) -> u32 {
        let encoding = self.encoding;
        match self.line(file, line0) {
            Some(line) => encoded_offset_to_scalar(encoding, line, wire_char),
            None => wire_char,
        }
    }

    /// 1-indexed Unicode-scalar column for a wire `(line0, character)` on
    /// `file` — for a public JSON `column`. Identity-degrades to
    /// `character + 1` if the line is unreadable.
    pub fn scalar_column(&mut self, file: &Path, line0: u32, wire_char: u32) -> u32 {
        self.scalar_offset(file, line0, wire_char) + 1
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
    fn converter_degrades_to_identity_on_unreadable_file() {
        let mut conv = PositionConverter::new(PositionEncoding::Utf16);
        let col = conv.scalar_column(Path::new("/nonexistent/file.rs"), 0, 4);
        assert_eq!(col, 5, "unreadable target falls back to wire_char + 1");
    }

    #[test]
    fn converter_uses_seeded_content_for_scalar_column() {
        let path = Path::new("seeded.rs");
        let mut conv = PositionConverter::new(PositionEncoding::Utf16).with_content(path, LINE);
        // wire utf-16 char 11 on line 0 -> 1-indexed scalar column 11.
        assert_eq!(conv.scalar_column(path, 0, 11), 11);
    }
}
