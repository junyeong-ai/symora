//! Reference classification — splits a symbol's usages into production
//! versus test buckets and aggregates per-file / per-module counts.
//!
//! The input is already the usage set (`LocationAnalysis` owns what that
//! means); this type only decides what each usage counts as. Test material
//! is decided per POSITION rather than per file, because a usage inside a
//! `#[cfg(test)]` region of a production file is test coverage, and calling
//! it a production dependency deflates coverage and inflates risk on the
//! very languages whose tests live beside the code they exercise.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::models::symbol::Location;
use crate::services::test_scope::TestScope;

pub struct RefsClassification<'a> {
    pub total: usize,
    pub test: usize,
    pub prod: usize,
    pub unique_files: usize,
    pub unique_modules: usize,
    pub test_refs: Vec<&'a Location>,
    pub file_counts: HashMap<PathBuf, (bool, usize)>,
}

impl<'a> RefsClassification<'a> {
    pub fn of(usages: &'a [Location], test_scope: &TestScope) -> Self {
        let mut classifier = test_scope.classifier();
        let mut test_refs = Vec::new();
        let mut prod = 0usize;
        let mut unique_files = HashSet::new();
        let mut unique_modules = HashSet::new();
        let mut file_counts: HashMap<PathBuf, (bool, usize)> = HashMap::new();

        for usage in usages {
            unique_files.insert(&usage.file);
            unique_modules.insert(extract_module(&usage.file));

            if classifier.is_test_code(&usage.file, usage.line) {
                test_refs.push(usage);
            } else {
                prod += 1;
            }

            // A file row is a file-level fact: it reports whether the FILE is
            // test material, which stays true even when only some of its
            // usages sit in a test region.
            file_counts
                .entry(usage.file.clone())
                .or_insert((test_scope.is_test_file(&usage.file), 0))
                .1 += 1;
        }

        Self {
            total: test_refs.len() + prod,
            test: test_refs.len(),
            prod,
            unique_files: unique_files.len(),
            unique_modules: unique_modules.len(),
            test_refs,
            file_counts,
        }
    }
}

/// Derive a module-path-style identifier from a file path so refs can be
/// counted per logical module rather than per file (folder names like
/// `src`, `lib`, `test` are stripped to keep the prefix meaningful).
pub fn extract_module(path: &Path) -> String {
    let components: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    let start = components
        .iter()
        .position(|&c| c == "src" || c == "lib" || c == "main" || c == "test" || c == "tests")
        .map(|i| i + 1)
        .unwrap_or(0);

    let end = components.len().saturating_sub(1);

    if start < end {
        components[start..end].join("/")
    } else {
        "root".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    fn at(file: &Path, line: u32) -> Location {
        Location::point(file.to_path_buf(), line, 1)
    }

    #[test]
    fn extract_module_strips_well_known_root_dirs() {
        assert_eq!(
            extract_module(&PathBuf::from("src/services/lsp.rs")),
            "services"
        );
        assert_eq!(
            extract_module(&PathBuf::from("src/cli/commands/impact.rs")),
            "cli/commands"
        );
        assert_eq!(extract_module(&PathBuf::from("src/main.rs")), "root");
        assert_eq!(
            extract_module(&PathBuf::from("lib/utils/helpers.py")),
            "utils"
        );
    }

    #[test]
    fn test_files_split_from_production_files() {
        let usages = vec![
            at(Path::new("src/service.rs"), 10),
            at(Path::new("tests/service_test.rs"), 4),
            at(Path::new("tests/service_test.rs"), 9),
        ];
        let classified = RefsClassification::of(&usages, &TestScope::new());

        assert_eq!(classified.total, 3);
        assert_eq!(classified.test, 2);
        assert_eq!(classified.prod, 1);
        assert_eq!(classified.unique_files, 2);
    }

    /// The defect a path-only answer cannot see: a production file whose
    /// tests live inside it. The usages in the `#[cfg(test)]` module are
    /// coverage, while the file itself stays a production file.
    #[test]
    fn usages_inside_an_inline_test_module_count_as_coverage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("service.rs");
        let mut file = std::fs::File::create(&path).expect("create fixture");
        file.write_all(
            b"fn prod() {}\n\nfn caller() { prod(); }\n\n#[cfg(test)]\nmod tests {\n    fn a() { prod(); }\n    fn b() { prod(); }\n}\n",
        )
        .expect("write fixture");

        let usages = vec![at(&path, 3), at(&path, 7), at(&path, 8)];
        let classified = RefsClassification::of(&usages, &TestScope::new());

        assert_eq!(classified.total, 3);
        assert_eq!(classified.test, 2);
        assert_eq!(classified.prod, 1);

        let (is_test_file, count) = classified.file_counts[&path];
        assert!(!is_test_file, "the file itself is production source");
        assert_eq!(count, 3);
    }
}
