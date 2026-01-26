//! Impact command implementation
//!
//! Analyze the impact of changing a symbol using LSP references.
//! Provides pure fact data for LLM to make contextual judgments.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{AffectedFile, ImpactResponse, RefStats, TargetInfo, TestCoverage};
use crate::cli::utils::find_symbol_at_line;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;

#[derive(Args, Debug)]
pub struct ImpactArgs {
    /// File path with position (file:line:column)
    pub location: String,

    /// Maximum files to show
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

pub async fn execute(args: ImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = ParsedLocation::parse(&args.location)?.to_absolute()?;
    let test_matcher = app.test_matcher();
    let root = ctx.root();

    // Get symbol at position for target info
    let symbols = app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::default())
        .await
        .ok();

    let target_symbol = symbols
        .as_ref()
        .and_then(|s| find_symbol_at_line(s, loc.line));

    match app
        .lsp
        .find_references(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(references) => {
            let project_refs: Vec<_> = references
                .iter()
                .filter(|r| ctx.is_project_path(&r.file))
                .collect();

            let mut files: HashMap<String, (bool, usize)> = HashMap::new();
            let mut modules: HashSet<String> = HashSet::new();
            let mut test_refs = 0;
            let mut prod_refs = 0;
            let mut test_files: Vec<String> = Vec::new();

            for r in &project_refs {
                let file_str = ctx.relative_path(&r.file);
                let is_test = test_matcher.is_test_file(&r.file);
                let module = extract_module(&r.file);

                modules.insert(module);

                if is_test {
                    test_refs += 1;
                    if !test_files.contains(&file_str) {
                        test_files.push(file_str.clone());
                    }
                } else {
                    prod_refs += 1;
                }

                files.entry(file_str).or_insert((is_test, 0)).1 += 1;
            }

            // Detect if symbol is exported
            let is_exported = target_symbol
                .as_ref()
                .map(|s| {
                    let lang = Language::from_path(&loc.file);
                    detect_exported(s.body.as_deref(), lang)
                })
                .unwrap_or(false);

            // Build affected files list
            let mut affected_files: Vec<AffectedFile> = files
                .into_iter()
                .map(|(file, (is_test, refs))| AffectedFile {
                    file,
                    is_test,
                    refs,
                })
                .collect();

            // Sort by refs descending
            affected_files.sort_by(|a, b| b.refs.cmp(&a.refs));

            let total_files = affected_files.len();
            affected_files.truncate(args.limit);

            // Build target info
            let target = match target_symbol {
                Some(sym) => TargetInfo::from_symbol(sym, root),
                None => TargetInfo::new(
                    format!("symbol@{}:{}", loc.line, loc.column),
                    "unknown".to_string(),
                    ctx.relative_path(&loc.file),
                    loc.line,
                ),
            };

            let response = ImpactResponse {
                target,
                refs: RefStats {
                    total: project_refs.len(),
                    test: test_refs,
                    prod: prod_refs,
                    files: total_files,
                    modules: modules.len(),
                    is_exported,
                },
                coverage: TestCoverage {
                    count: test_refs,
                    files: test_files,
                },
                files: affected_files,
            };

            ctx.print_success_flat(response);
        }
        Err(e) => ctx.print_error(&e.to_string()),
    }

    Ok(())
}

/// Detect if symbol is exported (public API) based on language keywords.
/// This is a fact check - just looking for keyword existence.
///
/// Exported means:
/// - Rust: `pub ` without restricted visibility (`pub(crate)`, `pub(super)`, `pub(in ...)`)
/// - Go: First character of name is uppercase
/// - Java/Kotlin: `public ` modifier
/// - TypeScript/JavaScript: `export ` keyword
/// - Python: Name doesn't start with underscore
/// - C#: `public ` modifier only (NOT `internal`)
fn detect_exported(body: Option<&str>, lang: Language) -> bool {
    let body = match body {
        Some(b) => b,
        None => return false,
    };

    let first_line = body.lines().next().unwrap_or("");

    match lang {
        Language::Rust => {
            // `pub ` is exported only if not restricted
            // Restricted: pub(crate), pub(super), pub(self), pub(in path)
            if !first_line.contains("pub ") {
                return false;
            }
            // Check for restricted visibility patterns
            !first_line.contains("pub(crate)")
                && !first_line.contains("pub(super)")
                && !first_line.contains("pub(self)")
                && !first_line.contains("pub(in ")
        }
        Language::Java | Language::Kotlin => first_line.contains("public "),
        Language::TypeScript | Language::JavaScript => first_line.contains("export "),
        Language::Go => {
            // Go: Find the function/type name after "func " or "type " keyword
            // The name starts after the keyword and is exported if uppercase
            let name_start = first_line
                .find("func ")
                .map(|i| i + 5)
                .or_else(|| first_line.find("type ").map(|i| i + 5));

            if let Some(start) = name_start {
                first_line[start..]
                    .chars()
                    .find(|c| c.is_alphabetic())
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
            } else {
                // For variables/constants: first alphabetic char
                first_line
                    .chars()
                    .find(|c| c.is_alphabetic())
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
            }
        }
        Language::Python => {
            // Python: Extract the name after def/class keyword
            let name_start = first_line
                .find("def ")
                .map(|i| i + 4)
                .or_else(|| first_line.find("class ").map(|i| i + 6));

            if let Some(start) = name_start {
                // Name is private if it starts with underscore
                !first_line[start..].trim_start().starts_with('_')
            } else {
                // For module-level variables
                !first_line.trim_start().starts_with('_')
            }
        }
        Language::CSharp => {
            // Only `public` is truly exported, NOT `internal` (assembly-private)
            first_line.contains("public ")
        }
        _ => false,
    }
}

/// Extract module path from file path.
/// Returns the directory path between src/lib and the filename.
fn extract_module(path: &Path) -> String {
    let components: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Find start point (after src/ or lib/)
    let start = components
        .iter()
        .position(|&c| c == "src" || c == "lib" || c == "main" || c == "test" || c == "tests")
        .map(|i| i + 1)
        .unwrap_or(0);

    // End before the filename
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
    use std::path::PathBuf;

    #[test]
    fn test_extract_module() {
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
    fn test_detect_exported_rust() {
        // Exported
        assert!(detect_exported(Some("pub fn process()"), Language::Rust));
        assert!(detect_exported(Some("pub struct Foo {}"), Language::Rust));

        // Not exported - restricted visibility
        assert!(!detect_exported(
            Some("pub(crate) fn process()"),
            Language::Rust
        ));
        assert!(!detect_exported(
            Some("pub(super) fn process()"),
            Language::Rust
        ));
        assert!(!detect_exported(
            Some("pub(self) fn process()"),
            Language::Rust
        ));
        assert!(!detect_exported(
            Some("pub(in crate::foo) fn process()"),
            Language::Rust
        ));

        // Not exported - private
        assert!(!detect_exported(Some("fn process()"), Language::Rust));
    }

    #[test]
    fn test_detect_exported_go() {
        assert!(detect_exported(Some("func Process()"), Language::Go));
        assert!(detect_exported(Some("type Handler struct"), Language::Go));
        assert!(!detect_exported(Some("func process()"), Language::Go));
        assert!(!detect_exported(Some("type handler struct"), Language::Go));
    }

    #[test]
    fn test_detect_exported_typescript() {
        assert!(detect_exported(
            Some("export function process()"),
            Language::TypeScript
        ));
        assert!(detect_exported(
            Some("export const foo = 1"),
            Language::TypeScript
        ));
        assert!(!detect_exported(
            Some("function process()"),
            Language::TypeScript
        ));
        assert!(!detect_exported(
            Some("const foo = 1"),
            Language::TypeScript
        ));
    }

    #[test]
    fn test_detect_exported_python() {
        assert!(detect_exported(Some("def process():"), Language::Python));
        assert!(detect_exported(Some("class Handler:"), Language::Python));
        assert!(!detect_exported(Some("def _process():"), Language::Python));
        assert!(!detect_exported(Some("class _Handler:"), Language::Python));
    }

    #[test]
    fn test_detect_exported_java() {
        assert!(detect_exported(
            Some("public void process()"),
            Language::Java
        ));
        assert!(detect_exported(
            Some("public class Handler"),
            Language::Java
        ));
        assert!(!detect_exported(
            Some("private void process()"),
            Language::Java
        ));
        assert!(!detect_exported(Some("void process()"), Language::Java));
    }

    #[test]
    fn test_detect_exported_csharp() {
        // Only public is exported
        assert!(detect_exported(
            Some("public void Process()"),
            Language::CSharp
        ));
        assert!(detect_exported(
            Some("public class Handler"),
            Language::CSharp
        ));

        // internal is NOT exported (assembly-private)
        assert!(!detect_exported(
            Some("internal void Process()"),
            Language::CSharp
        ));
        assert!(!detect_exported(
            Some("private void Process()"),
            Language::CSharp
        ));
    }
}
