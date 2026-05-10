//! Identifies test files. Combines a built-in pattern catalog (covering
//! every language Symora supports) with the user's `.symora/config.toml`
//! `[test]` overrides.

use std::path::Path;

use crate::models::config::TestConfig;

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
        if self.matches_custom_patterns(path) {
            return true;
        }
        is_test_file_default(path)
    }

    fn matches_custom_patterns(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy().to_lowercase();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        for pattern in &self.custom_dir_patterns {
            if path_str.contains(&pattern.to_lowercase()) {
                return true;
            }
        }

        for pattern in &self.custom_file_patterns {
            if file_name.ends_with(pattern) {
                return true;
            }
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

fn is_test_file_default(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    if path_str.contains("/tests/")
        || path_str.contains("/test/")
        || path_str.contains("/__tests__/")
        || path_str.contains("/spec/")
        || path_str.starts_with("tests/")
        || path_str.starts_with("test/")
    {
        return true;
    }

    if file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.cpp")
        || file_name.ends_with("_test.cc")
        || file_name.ends_with("Test.cpp")
        || file_name.ends_with("_test.c")
        || file_name.ends_with("_test.zig")
    {
        return true;
    }

    if file_name.ends_with("Test.java")
        || file_name.ends_with("Tests.java")
        || file_name.ends_with("IT.java")
        || file_name.ends_with("Test.kt")
        || file_name.ends_with("Tests.kt")
        || file_name.ends_with("Spec.kt")
        || file_name.ends_with("Spek.kt")
        || file_name.ends_with("Spec.scala")
        || file_name.ends_with("Test.scala")
        || file_name.ends_with("Suite.scala")
        || file_name.ends_with("_test.clj")
        || file_name.ends_with("Spec.groovy")
    {
        return true;
    }

    if file_name.ends_with("Test.cs")
        || file_name.ends_with("Tests.cs")
        || file_name.ends_with("Test.fs")
        || file_name.ends_with("Tests.fs")
    {
        return true;
    }

    if file_name.ends_with(".test.ts")
        || file_name.ends_with(".test.tsx")
        || file_name.ends_with(".test.js")
        || file_name.ends_with(".test.jsx")
        || file_name.ends_with(".spec.ts")
        || file_name.ends_with(".spec.tsx")
        || file_name.ends_with(".spec.js")
        || file_name.ends_with(".spec.jsx")
        || file_name.ends_with(".cy.ts")
        || file_name.ends_with(".cy.js")
        || file_name.ends_with(".test.vue")
        || file_name.ends_with(".spec.vue")
    {
        return true;
    }

    if file_name.ends_with("_test.py")
        || file_name.ends_with("_spec.rb")
        || file_name.ends_with("_test.rb")
        || file_name.ends_with("Test.php")
        || file_name.ends_with("Cest.php")
        || file_name.ends_with(".t")
        || file_name.ends_with("_spec.lua")
        || file_name.ends_with(".bats")
        || file_name.ends_with(".Tests.ps1")
    {
        return true;
    }

    if file_name.ends_with("Spec.hs")
        || file_name.ends_with("Test.hs")
        || file_name.ends_with("_test.exs")
        || file_name.ends_with("_SUITE.erl")
        || file_name.ends_with("_tests.erl")
        || file_name.ends_with("Test.elm")
        || file_name.ends_with("_test.ml")
    {
        return true;
    }

    if file_name.ends_with("_test.go")
        || file_name.ends_with("Tests.swift")
        || file_name.ends_with("Spec.swift")
        || file_name.ends_with("_test.dart")
    {
        return true;
    }

    if file_name.ends_with("_test.jl") || file_name.ends_with("_test.R") {
        return true;
    }

    if (file_name.starts_with("test_") && file_name.ends_with(".py"))
        || (file_name.starts_with("test_") && file_name.ends_with(".lua"))
        || (file_name.starts_with("test_") && file_name.ends_with(".sh"))
        || (file_name.starts_with("test_") && file_name.ends_with(".ml"))
        || (file_name.starts_with("test-") && file_name.ends_with(".R"))
        || (file_name.starts_with("test_") && file_name.ends_with(".jl"))
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::config::TestConfig;

    #[test]
    fn default_patterns_match_well_known_test_files() {
        let matcher = TestMatcher::new();
        assert!(matcher.is_test_file(Path::new("src/tests/foo.rs")));
        assert!(matcher.is_test_file(Path::new("src/foo_test.rs")));
        assert!(matcher.is_test_file(Path::new("src/foo.test.ts")));
        assert!(matcher.is_test_file(Path::new("src/FooTest.java")));
        assert!(matcher.is_test_file(Path::new("src/FooSpec.kt")));

        assert!(!matcher.is_test_file(Path::new("src/main.rs")));
        assert!(!matcher.is_test_file(Path::new("src/service.kt")));
    }

    #[test]
    fn custom_file_patterns_extend_default_set() {
        let config = TestConfig {
            file_patterns: vec!["_check.rs".to_string(), "Verify.java".to_string()],
            dir_patterns: vec![],
            markers: vec![],
        };
        let matcher = TestMatcher::from_config(&config);

        assert!(matcher.is_test_file(Path::new("src/foo_check.rs")));
        assert!(matcher.is_test_file(Path::new("src/FooVerify.java")));
        // Defaults still match.
        assert!(matcher.is_test_file(Path::new("src/foo_test.rs")));
    }

    #[test]
    fn custom_dir_patterns_extend_default_set() {
        let config = TestConfig {
            file_patterns: vec![],
            dir_patterns: vec!["/verification/".to_string()],
            markers: vec![],
        };
        let matcher = TestMatcher::from_config(&config);

        assert!(matcher.is_test_file(Path::new("src/verification/foo.rs")));
        // Defaults still match.
        assert!(matcher.is_test_file(Path::new("tests/foo.rs")));
    }

    #[test]
    fn prefix_patterns_with_trailing_star() {
        let config = TestConfig {
            file_patterns: vec!["check_*".to_string()],
            dir_patterns: vec![],
            markers: vec![],
        };
        let matcher = TestMatcher::from_config(&config);

        assert!(matcher.is_test_file(Path::new("src/check_something.rs")));
    }
}
