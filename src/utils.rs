/// Convert a character index to byte index for safe UTF-8 string slicing
pub fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Cheap token estimate using the well-known ~4 chars per token heuristic.
/// Lives at the crate root so any layer (services, cli, mcp) can call it
/// without crossing layer boundaries.
///
/// Uses `chars().count()`, not `len()`, because a byte-based count would
/// overestimate by 3-4× on Korean/Japanese/Chinese source — and source
/// files routinely contain multi-byte identifiers, comments, or strings.
/// 4 chars/token still over-counts CJK relative to a real BPE tokenizer
/// (BPE often groups 1-2 CJK chars per token), but it's a stable
/// upper-bound budget signal across languages.
pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_ascii_is_chars_div_4_ceil() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("12345678"), 2);
    }

    #[test]
    fn token_estimate_counts_chars_not_bytes_for_cjk() {
        // "안녕" is 6 bytes UTF-8 but 2 chars — should give 1 token, not 2.
        assert_eq!(estimate_tokens("안녕"), 1);
        // "한국어 코드" is 6 chars (incl. space) → 2 tokens.
        assert_eq!(estimate_tokens("한국어 코드"), 2);
    }

    #[test]
    fn token_estimate_handles_emoji_as_single_char() {
        // "🦀" (4 bytes) is one user-perceived character.
        assert_eq!(estimate_tokens("🦀"), 1);
        assert_eq!(estimate_tokens("🦀🦀🦀🦀"), 1);
    }
}
