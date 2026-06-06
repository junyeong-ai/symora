pub mod ast;
pub mod file_filter;
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
