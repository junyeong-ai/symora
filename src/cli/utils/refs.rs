//! Reference classification — splits LSP `find_references` results into
//! production vs test buckets and aggregates per-file / per-module counts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::test_matcher::TestMatcher;
use crate::models::symbol::Location;

pub struct RefsClassification<'a> {
    pub total: usize,
    pub test: usize,
    pub prod: usize,
    pub unique_files: usize,
    pub unique_modules: usize,
    pub test_refs: Vec<&'a Location>,
    pub file_counts: HashMap<PathBuf, (bool, usize)>,
}

pub fn classify_refs<'a>(
    refs: &'a [Location],
    root: &Path,
    self_file: Option<&Path>,
    self_line: Option<u32>,
    test_matcher: &TestMatcher,
) -> RefsClassification<'a> {
    let mut test_count = 0usize;
    let mut prod_count = 0usize;
    let mut test_refs = Vec::new();
    let mut unique_files = HashSet::new();
    let mut unique_modules = HashSet::new();
    let mut file_counts: HashMap<PathBuf, (bool, usize)> = HashMap::new();

    for r in refs {
        if self_file.is_some_and(|f| r.file == f) && self_line.is_some_and(|l| r.line == l) {
            continue;
        }
        if !r.file.starts_with(root) {
            continue;
        }

        unique_files.insert(&r.file);
        let module = extract_module(&r.file);
        unique_modules.insert(module);
        let is_test = test_matcher.is_test_file(&r.file);

        if is_test {
            test_count += 1;
            test_refs.push(r);
        } else {
            prod_count += 1;
        }

        file_counts.entry(r.file.clone()).or_insert((is_test, 0)).1 += 1;
    }

    RefsClassification {
        total: test_count + prod_count,
        test: test_count,
        prod: prod_count,
        unique_files: unique_files.len(),
        unique_modules: unique_modules.len(),
        test_refs,
        file_counts,
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
}
