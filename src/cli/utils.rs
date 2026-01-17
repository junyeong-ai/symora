//! Common CLI utilities
//!
//! Shared helper functions for CLI commands.

use std::path::Path;

use crate::models::config::TestConfig;
use crate::models::symbol::Symbol;

/// Test file matcher with support for custom patterns
#[derive(Debug, Clone)]
pub struct TestMatcher {
    custom_file_patterns: Vec<String>,
    custom_dir_patterns: Vec<String>,
}

impl Default for TestMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl TestMatcher {
    pub fn new() -> Self {
        Self {
            custom_file_patterns: Vec::new(),
            custom_dir_patterns: Vec::new(),
        }
    }

    pub fn from_config(config: &TestConfig) -> Self {
        Self {
            custom_file_patterns: config.file_patterns.clone(),
            custom_dir_patterns: config.dir_patterns.clone(),
        }
    }

    pub fn is_test_file(&self, path: &Path) -> bool {
        // Check custom patterns first
        if self.matches_custom_patterns(path) {
            return true;
        }

        // Fall back to default patterns
        is_test_file_default(path)
    }

    fn matches_custom_patterns(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy().to_lowercase();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Check custom directory patterns
        for pattern in &self.custom_dir_patterns {
            let pattern_lower = pattern.to_lowercase();
            if path_str.contains(&pattern_lower) {
                return true;
            }
        }

        // Check custom file patterns
        for pattern in &self.custom_file_patterns {
            if file_name.ends_with(pattern) {
                return true;
            }
            // Support prefix patterns like "test_*"
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                if file_name.starts_with(prefix) {
                    return true;
                }
            }
        }

        false
    }
}

/// Check if a path represents a test file (convenience function using defaults only)
pub fn is_test_file(path: &Path) -> bool {
    is_test_file_default(path)
}

/// Check if a path represents a test file based on built-in patterns
fn is_test_file_default(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Directory patterns (language-agnostic)
    if path_str.contains("/tests/")
        || path_str.contains("/test/")
        || path_str.contains("/__tests__/")
        || path_str.contains("/spec/")
        || path_str.starts_with("tests/")
        || path_str.starts_with("test/")
    {
        return true;
    }

    // Systems languages
    if file_name.ends_with("_test.rs")                      // Rust
        || file_name.ends_with("_test.cpp")                 // C++
        || file_name.ends_with("_test.cc")                  // C++
        || file_name.ends_with("Test.cpp")                  // C++
        || file_name.ends_with("_test.c")                   // C
        || file_name.ends_with("_test.zig")
    // Zig
    {
        return true;
    }

    // JVM languages
    if file_name.ends_with("Test.java")                     // Java JUnit
        || file_name.ends_with("Tests.java")                // Java JUnit
        || file_name.ends_with("IT.java")                   // Java Integration Test
        || file_name.ends_with("Test.kt")                   // Kotlin JUnit
        || file_name.ends_with("Tests.kt")                  // Kotlin JUnit
        || file_name.ends_with("Spec.kt")                   // Kotlin Kotest
        || file_name.ends_with("Spek.kt")                   // Kotlin Spek
        || file_name.ends_with("Spec.scala")                // Scala ScalaTest
        || file_name.ends_with("Test.scala")                // Scala
        || file_name.ends_with("Suite.scala")               // Scala
        || file_name.ends_with("_test.clj")                 // Clojure
        || file_name.ends_with("Spec.groovy")
    // Groovy Spock
    {
        return true;
    }

    // .NET languages
    if file_name.ends_with("Test.cs")                       // C#
        || file_name.ends_with("Tests.cs")                  // C#
        || file_name.ends_with("Test.fs")                   // F#
        || file_name.ends_with("Tests.fs")
    // F#
    {
        return true;
    }

    // Web languages
    if file_name.ends_with(".test.ts")                      // TypeScript Jest/Vitest
        || file_name.ends_with(".test.tsx")                 // TypeScript React
        || file_name.ends_with(".test.js")                  // JavaScript Jest
        || file_name.ends_with(".test.jsx")                 // JavaScript React
        || file_name.ends_with(".spec.ts")                  // TypeScript Mocha/Playwright
        || file_name.ends_with(".spec.tsx")                 // TypeScript React
        || file_name.ends_with(".spec.js")                  // JavaScript Mocha
        || file_name.ends_with(".spec.jsx")                 // JavaScript React
        || file_name.ends_with(".cy.ts")                    // Cypress
        || file_name.ends_with(".cy.js")                    // Cypress
        || file_name.ends_with(".test.vue")                 // Vue
        || file_name.ends_with(".spec.vue")
    // Vue
    {
        return true;
    }

    // Scripting languages
    if file_name.ends_with("_test.py")                      // Python pytest
        || file_name.ends_with("_spec.rb")                  // Ruby RSpec
        || file_name.ends_with("_test.rb")                  // Ruby Minitest
        || file_name.ends_with("Test.php")                  // PHP PHPUnit
        || file_name.ends_with("Cest.php")                  // PHP Codeception
        || file_name.ends_with(".t")                        // Perl
        || file_name.ends_with("_spec.lua")                 // Lua busted
        || file_name.ends_with(".bats")                     // Bash bats
        || file_name.ends_with(".Tests.ps1")
    // PowerShell Pester
    {
        return true;
    }

    // Functional languages
    if file_name.ends_with("Spec.hs")                       // Haskell
        || file_name.ends_with("Test.hs")                   // Haskell
        || file_name.ends_with("_test.exs")                 // Elixir
        || file_name.ends_with("_SUITE.erl")                // Erlang
        || file_name.ends_with("_tests.erl")                // Erlang
        || file_name.ends_with("Test.elm")                  // Elm
        || file_name.ends_with("_test.ml")
    // OCaml
    {
        return true;
    }

    // Mobile/Application languages
    if file_name.ends_with("_test.go")                      // Go
        || file_name.ends_with("Tests.swift")               // Swift XCTest
        || file_name.ends_with("Spec.swift")                // Swift Quick
        || file_name.ends_with("_test.dart")
    // Dart test
    {
        return true;
    }

    // Scientific languages
    if file_name.ends_with("_test.jl")                      // Julia
        || file_name.ends_with("_test.R")
    // R
    {
        return true;
    }

    // Prefix patterns
    if (file_name.starts_with("test_") && file_name.ends_with(".py"))       // Python
        || (file_name.starts_with("test_") && file_name.ends_with(".lua"))  // Lua
        || (file_name.starts_with("test_") && file_name.ends_with(".sh"))   // Bash
        || (file_name.starts_with("test_") && file_name.ends_with(".ml"))   // OCaml
        || (file_name.starts_with("test-") && file_name.ends_with(".R"))    // R
        || (file_name.starts_with("test_") && file_name.ends_with(".jl"))
    // Julia
    {
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

/// Read a single line from a file (for snippet extraction)
pub fn read_line_at(path: &Path, line: u32) -> std::io::Result<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    reader
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .transpose()?
        .map(|s| s.trim().to_string())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Line not found"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::config::TestConfig;

    #[test]
    fn test_default_patterns() {
        let matcher = TestMatcher::new();

        // Default patterns should match
        assert!(matcher.is_test_file(Path::new("src/tests/foo.rs")));
        assert!(matcher.is_test_file(Path::new("src/foo_test.rs")));
        assert!(matcher.is_test_file(Path::new("src/foo.test.ts")));
        assert!(matcher.is_test_file(Path::new("src/FooTest.java")));
        assert!(matcher.is_test_file(Path::new("src/FooSpec.kt")));

        // Non-test files should not match
        assert!(!matcher.is_test_file(Path::new("src/main.rs")));
        assert!(!matcher.is_test_file(Path::new("src/service.kt")));
    }

    #[test]
    fn test_custom_file_patterns() {
        let config = TestConfig {
            file_patterns: vec!["_check.rs".to_string(), "Verify.java".to_string()],
            dir_patterns: vec![],
            markers: vec![],
        };
        let matcher = TestMatcher::from_config(&config);

        // Custom patterns should match
        assert!(matcher.is_test_file(Path::new("src/foo_check.rs")));
        assert!(matcher.is_test_file(Path::new("src/FooVerify.java")));

        // Default patterns should still work
        assert!(matcher.is_test_file(Path::new("src/foo_test.rs")));
    }

    #[test]
    fn test_custom_dir_patterns() {
        let config = TestConfig {
            file_patterns: vec![],
            dir_patterns: vec!["/verification/".to_string()],
            markers: vec![],
        };
        let matcher = TestMatcher::from_config(&config);

        // Custom directory pattern should match
        assert!(matcher.is_test_file(Path::new("src/verification/foo.rs")));

        // Default directory patterns should still work
        assert!(matcher.is_test_file(Path::new("tests/foo.rs")));
    }

    #[test]
    fn test_prefix_patterns() {
        let config = TestConfig {
            file_patterns: vec!["check_*".to_string()],
            dir_patterns: vec![],
            markers: vec![],
        };
        let matcher = TestMatcher::from_config(&config);

        // Prefix pattern should match
        assert!(matcher.is_test_file(Path::new("src/check_something.rs")));
    }
}
