pub mod ast;
pub mod file_filter;
pub mod file_lock;
pub mod lsp;
pub mod retry;

/// Stable 64-bit content fingerprint (FNV-1a) used to detect file changes.
/// The algorithm is fixed and portable on purpose: this value is persisted
/// as the store's incremental-reindex currency key, so it must stay identical
/// across toolchains and target platforms for the same bytes.
#[inline]
pub fn hash_content(content: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Whether a failure leaves what a path holds unknown.
///
/// One kind settles it wherever it appears: `NotFound` says nothing is there,
/// so an answer computed without the path is still whole — the path was never
/// in the domain. Every other kind hides something that may exist, and that is
/// what makes a count a lower bound.
///
/// This is the line for looking a path up or opening it. A failure to DECODE
/// one settles more; see `hides_text`.
#[inline]
pub fn hides_content(error: &std::io::Error) -> bool {
    error.kind() != std::io::ErrorKind::NotFound
}

/// Whether a failure to READ a file leaves its text unknown.
///
/// Adds the verdict only a decode can reach: `InvalidData` says what is there
/// is not text, so the file was never in a text search's domain either. An
/// open or a lookup cannot reach that verdict — it never saw the bytes — which
/// is why the two lines are drawn apart.
///
/// Every surface that reads files shares this one, or the same file would be
/// outside one search's domain and a hole in another's.
#[inline]
pub fn hides_text(error: &std::io::Error) -> bool {
    hides_content(error) && error.kind() != std::io::ErrorKind::InvalidData
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// The two lines are drawn apart because an open or a lookup never saw the
    /// bytes: `InvalidData` from one of those says nothing about whether the
    /// path holds text, and reading it as "not text" would drop the path from
    /// an answer that is then reported as whole.
    #[test]
    fn only_a_decode_can_settle_that_a_path_holds_no_text() {
        let not_found = Error::from(ErrorKind::NotFound);
        assert!(!hides_content(&not_found), "nothing is there to hide");
        assert!(!hides_text(&not_found));

        let invalid = Error::from(ErrorKind::InvalidData);
        assert!(
            hides_content(&invalid),
            "an open that failed this way still leaves the content unknown"
        );
        assert!(
            !hides_text(&invalid),
            "a decode that failed this way settled it"
        );

        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Interrupted,
            ErrorKind::Other,
        ] {
            let error = Error::from(kind);
            assert!(hides_content(&error), "{kind:?}");
            assert!(hides_text(&error), "{kind:?}");
        }
    }
}
