//! Test-versus-production classification for source code.
//!
//! Two questions get asked of this module, and they are not the same
//! question:
//!
//! - *Is this file test material?* — asked when ranking or summarising
//!   whole files. Answered from the path alone, which is exact for the
//!   layouts a build system defines (Go's `_test.go`, a JVM `src/test`
//!   source set) and conventional elsewhere. Costs no I/O.
//! - *Is the code at this position test material?* — asked when a
//!   reference, caller, or usage is being classified as coverage rather
//!   than production dependency. A path can not answer it: Rust's dominant
//!   idiom puts tests in a `#[cfg(test)] mod tests` inside the file they
//!   exercise, so a path-only answer reports a heavily-tested symbol as
//!   uncovered and inflates every risk signal derived from it.
//!
//! [`TestScope`] answers the first and hands out a [`TestClassifier`] for
//! the second. The classifier memoises per file and lives for exactly one
//! analysis pass, so its view can never go stale against an edit.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::infra::ast::{has_test_regions, test_regions};
use crate::models::config::TestConfig;
use crate::models::symbol::Language;

/// Path-level test rules: the built-in catalog covering every language
/// Symora supports, plus the user's `.symora/config.toml` `[test]`
/// additions.
#[derive(Debug, Clone, Default)]
pub struct TestScope {
    custom_file_patterns: Vec<String>,
    custom_dir_patterns: Vec<String>,
}

impl TestScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_config(config: &TestConfig) -> Self {
        Self {
            custom_file_patterns: config.file_patterns.clone(),
            custom_dir_patterns: config.dir_patterns.clone(),
        }
    }

    /// Whether the file as a whole is test material.
    pub fn is_test_file(&self, path: &Path) -> bool {
        self.matches_custom_patterns(path) || is_test_file_default(path)
    }

    /// A position-level classifier for one analysis pass.
    pub fn classifier(&self) -> TestClassifier<'_> {
        TestClassifier {
            scope: self,
            regions: HashMap::new(),
        }
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
            if let Some(prefix) = pattern.strip_suffix('*')
                && file_name.starts_with(prefix)
            {
                return true;
            }
        }

        false
    }
}

/// Classifies code positions for one analysis pass, memoising each file's
/// test regions on first use.
///
/// A file that is test material as a whole never gets read: the path
/// already settles it. A language that defines no test-only region never
/// gets read either, so the position-level answer costs exactly the
/// file-level one everywhere the language has nothing more to say.
pub struct TestClassifier<'a> {
    scope: &'a TestScope,
    regions: HashMap<PathBuf, Vec<RangeInclusive<u32>>>,
}

impl TestClassifier<'_> {
    /// Whether the code at `line` (1-indexed) in `file` is test material.
    pub fn is_test_code(&mut self, file: &Path, line: u32) -> bool {
        if self.scope.is_test_file(file) {
            return true;
        }
        self.file_regions(file).iter().any(|r| r.contains(&line))
    }

    fn file_regions(&mut self, file: &Path) -> &[RangeInclusive<u32>] {
        self.regions
            .entry(file.to_path_buf())
            .or_insert_with(|| {
                let language = Language::from_path(file);
                if !has_test_regions(language) {
                    return Vec::new();
                }
                std::fs::read_to_string(file)
                    .map(|content| test_regions(&content, language))
                    .unwrap_or_default()
            })
            .as_slice()
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

    use std::io::Write;

    #[test]
    fn default_patterns_match_well_known_test_files() {
        let scope = TestScope::new();
        assert!(scope.is_test_file(Path::new("src/tests/foo.rs")));
        assert!(scope.is_test_file(Path::new("src/foo_test.rs")));
        assert!(scope.is_test_file(Path::new("src/foo.test.ts")));
        assert!(scope.is_test_file(Path::new("src/FooTest.java")));
        assert!(scope.is_test_file(Path::new("src/FooSpec.kt")));

        assert!(!scope.is_test_file(Path::new("src/main.rs")));
        assert!(!scope.is_test_file(Path::new("src/service.kt")));
    }

    #[test]
    fn custom_file_patterns_extend_default_set() {
        let config = TestConfig {
            file_patterns: vec!["_check.rs".to_string(), "Verify.java".to_string()],
            dir_patterns: vec![],
        };
        let scope = TestScope::from_config(&config);

        assert!(scope.is_test_file(Path::new("src/foo_check.rs")));
        assert!(scope.is_test_file(Path::new("src/FooVerify.java")));
        assert!(scope.is_test_file(Path::new("src/foo_test.rs")));
    }

    #[test]
    fn custom_dir_patterns_extend_default_set() {
        let config = TestConfig {
            file_patterns: vec![],
            dir_patterns: vec!["/verification/".to_string()],
        };
        let scope = TestScope::from_config(&config);

        assert!(scope.is_test_file(Path::new("src/verification/foo.rs")));
        assert!(scope.is_test_file(Path::new("tests/foo.rs")));
    }

    #[test]
    fn prefix_patterns_with_trailing_star() {
        let config = TestConfig {
            file_patterns: vec!["check_*".to_string()],
            dir_patterns: vec![],
        };
        let scope = TestScope::from_config(&config);

        assert!(scope.is_test_file(Path::new("src/check_something.rs")));
    }

    fn write_temp(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).expect("create fixture");
        file.write_all(content.as_bytes()).expect("write fixture");
        (dir, path)
    }

    #[test]
    fn inline_rust_tests_are_test_code_in_a_production_file() {
        let (_dir, path) = write_temp(
            "service.rs",
            "fn prod() {}\n\n#[cfg(test)]\nmod tests {\n    fn exercise() { prod(); }\n}\n",
        );
        let scope = TestScope::new();
        assert!(!scope.is_test_file(&path));

        let mut classifier = scope.classifier();
        assert!(!classifier.is_test_code(&path, 1));
        assert!(classifier.is_test_code(&path, 5));
    }

    #[test]
    fn a_test_file_needs_no_content_to_classify() {
        let scope = TestScope::new();
        let mut classifier = scope.classifier();
        assert!(classifier.is_test_code(Path::new("tests/absent.rs"), 1));
    }

    #[test]
    fn languages_without_test_regions_fall_back_to_the_path_answer() {
        let (_dir, path) = write_temp(
            "service.py",
            "def prod():\n    pass\n\nclass Check(unittest.TestCase):\n    pass\n",
        );
        let scope = TestScope::new();
        let mut classifier = scope.classifier();
        assert!(!classifier.is_test_code(&path, 4));
    }

    #[test]
    fn an_unreadable_file_makes_no_test_claim() {
        let scope = TestScope::new();
        let mut classifier = scope.classifier();
        assert!(!classifier.is_test_code(Path::new("src/absent.rs"), 1));
    }
}
