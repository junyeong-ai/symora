//! Infrastructure layer for Symora
//!
//! Contains low-level implementations and external integrations.

pub mod ast;
pub mod file_filter;
pub mod lsp;
pub mod retry;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Hash content for cache invalidation
#[inline]
pub fn hash_content(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}
