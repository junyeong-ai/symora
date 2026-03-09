use anyhow::Result;
use clap::Args;

use crate::app::App;
use crate::cli::LocationArg;
use crate::cli::response::{
    AffectedFileOutput, ImpactOutput, RefOutput, TargetOutput, TestCoverageOutput,
};
use crate::cli::utils::{classify_refs, find_symbol_at_position};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::Language;

#[derive(Args, Debug)]
pub struct ImpactArgs {
    #[command(flatten)]
    pub loc: LocationArg,

    /// Maximum files to show
    #[arg(long)]
    pub limit: Option<usize>,
}

pub async fn execute(args: ImpactArgs, app: &App) -> Result<()> {
    let ctx = &app.output;
    let loc = args.loc.parse()?.to_absolute()?;
    let test_matcher = app.test_matcher();
    let root = ctx.root();
    let limit = args.limit.unwrap_or(50);

    // Get symbol at position for target info
    let symbols = app
        .lsp
        .find_symbols(&loc.file, FindSymbolsOptions::default().with_body())
        .await
        .ok();

    let target_symbol = symbols
        .as_ref()
        .and_then(|s| find_symbol_at_position(s, loc.line, Some(loc.column)));

    match app
        .lsp
        .find_references(&loc.file, loc.line, loc.column)
        .await
    {
        Ok(references) => {
            let classified = classify_refs(&references, root, None, None, test_matcher);

            let is_exported = target_symbol.as_ref().and_then(|s| {
                s.body.as_deref().map(|body| {
                    let lang = Language::from_path(&loc.file);
                    detect_exported(Some(body), lang)
                })
            });

            let mut affected_files: Vec<AffectedFileOutput> = classified
                .file_counts
                .into_iter()
                .map(|(path, (is_test, refs))| AffectedFileOutput {
                    file: ctx.relative_path(&path),
                    is_test,
                    refs,
                })
                .collect();
            affected_files.sort_by(|a, b| b.refs.cmp(&a.refs));
            let total_files = affected_files.len();
            affected_files.truncate(limit);

            let test_files: Vec<String> = classified
                .test_refs
                .iter()
                .map(|r| ctx.relative_path(&r.file))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();

            let target = TargetOutput::from_symbol_or_fallback(
                target_symbol,
                &loc.file,
                loc.line,
                loc.column,
                root,
            );

            let response = ImpactOutput {
                target,
                refs: RefOutput {
                    total: classified.total,
                    test: classified.test,
                    prod: classified.prod,
                    files: Some(total_files),
                    modules: Some(classified.unique_modules),
                    is_exported,
                },
                coverage: TestCoverageOutput {
                    count: classified.test,
                    files: test_files,
                },
                files: affected_files,
            };

            ctx.print_success(response);
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
            // Go: exported if the function/type name starts with uppercase
            // Must handle method receivers: "func (h *Handler) Process()" → check "Process"
            if let Some(func_pos) = first_line.find("func ") {
                let after_func = &first_line[func_pos + 5..];
                // Skip method receiver: "(receiver) Name"
                let name_part = if after_func.starts_with('(') {
                    after_func
                        .find(") ")
                        .map(|i| &after_func[i + 2..])
                        .unwrap_or(after_func)
                } else {
                    after_func
                };
                name_part
                    .chars()
                    .find(|c| c.is_alphabetic())
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
            } else if let Some(type_pos) = first_line.find("type ") {
                first_line[type_pos + 5..]
                    .chars()
                    .find(|c| c.is_alphabetic())
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
            } else {
                // Variables/constants: first alphabetic char
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Method receivers
        assert!(detect_exported(
            Some("func (h *Handler) Process()"),
            Language::Go
        ));
        assert!(!detect_exported(
            Some("func (h *Handler) process()"),
            Language::Go
        ));
        assert!(detect_exported(
            Some("func (s Service) Export()"),
            Language::Go
        ));
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
