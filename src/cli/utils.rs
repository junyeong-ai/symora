//! Common CLI utilities
//!
//! Shared helper functions for CLI commands.

use std::path::Path;

use crate::models::symbol::Symbol;

/// Check if a path represents a test file based on common patterns
pub fn is_test_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Directory patterns
    if path_str.contains("/tests/")
        || path_str.contains("/test/")
        || path_str.contains("/__tests__/")
        || path_str.contains("/spec/")
        || path_str.starts_with("tests/")
        || path_str.starts_with("test/")
    {
        return true;
    }

    // File suffix patterns
    if file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.go")
        || file_name.ends_with("_test.py")
        || file_name.ends_with("Test.java")
        || file_name.ends_with("Test.kt")
        || file_name.ends_with("Tests.java")
        || file_name.ends_with("Tests.kt")
        || file_name.ends_with(".test.ts")
        || file_name.ends_with(".test.tsx")
        || file_name.ends_with(".test.js")
        || file_name.ends_with(".test.jsx")
        || file_name.ends_with(".spec.ts")
        || file_name.ends_with(".spec.tsx")
        || file_name.ends_with(".spec.js")
        || file_name.ends_with(".spec.jsx")
        || file_name.ends_with("_spec.rb")
    {
        return true;
    }

    // File prefix patterns (Python)
    if file_name.starts_with("test_") && file_name.ends_with(".py") {
        return true;
    }

    false
}

/// Extract function/method signature from body source code
pub fn extract_signature(body: Option<&str>) -> Option<String> {
    let body = body?;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Common signature patterns
        if trimmed.contains("fn ")
            || trimmed.contains("func ")
            || trimmed.contains("def ")
            || trimmed.contains("function ")
            || trimmed.contains("async ")
            || trimmed.contains("pub ")
            || trimmed.contains("class ")
            || trimmed.contains("struct ")
            || trimmed.contains("enum ")
            || trimmed.contains("interface ")
            || trimmed.contains("trait ")
            || trimmed.contains("impl ")
            || trimmed.contains("type ")
            || trimmed.contains("const ")
        {
            let sig = if let Some(brace_pos) = trimmed.find('{') {
                trimmed[..brace_pos].trim()
            } else if let Some(arrow_pos) = trimmed.find("=>") {
                trimmed[..arrow_pos].trim()
            } else if let Some(stripped) = trimmed.strip_suffix(':') {
                stripped
            } else {
                trimmed
            };

            return Some(sig.to_string());
        }
    }

    body.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

/// Find symbol at a specific line number (recursive search)
pub fn find_symbol_at_line(symbols: &[Symbol], line: u32) -> Option<&Symbol> {
    fn search(symbols: &[Symbol], line: u32) -> Option<&Symbol> {
        for symbol in symbols {
            let start = symbol.location.line;
            let end = symbol.location.end_line.unwrap_or(start);

            if line >= start && line <= end {
                if let Some(child) = search(&symbol.children, line) {
                    return Some(child);
                }
                return Some(symbol);
            }
        }
        None
    }
    search(symbols, line)
}
