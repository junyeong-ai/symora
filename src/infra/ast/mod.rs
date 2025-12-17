//! AST parsing infrastructure for Symora
//!
//! Tree-sitter based parsing for AST pattern search.
//! Supports 13 languages with verified node types.

pub mod node_types;

pub use node_types::{
    NodeType, format_query_error, get_node_types, is_supported, supported_languages,
};
