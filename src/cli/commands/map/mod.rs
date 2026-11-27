use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::disclosure::{Cap, as_paths, relative_paths, with_cap};
use crate::cli::response::{Section, SymbolOutput};
use crate::cli::utils::shell_arg;
use crate::constants::defaults;
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};

mod modfile;
mod tests_of;
mod workspace;

use tests_of::TestFileOutput;
use workspace::PackageOutput;

#[derive(Args, Debug)]
#[command(
    after_long_help = "Typical workflow:\n  1. `symora map summary` for entrypoints and major areas\n  2. `symora search symbols <query>` for rough workspace discovery\n  3. `symora map file <path>` for one file, the tests importing it, and its neighbours\n  4. `symora symbols <file>` or `symora refs <loc>` for precise follow-up\n"
)]
pub struct MapArgs {
    #[command(subcommand)]
    pub command: MapCommand,
}

#[derive(Subcommand, Debug)]
pub enum MapCommand {
    /// High-level project map
    Summary {
        /// Maximum top-level directories, packages and entrypoints to show
        #[arg(long, default_value_t = defaults::MAP_SUMMARY_LIMIT)]
        limit: usize,
    },
    /// Show map for a single file
    File {
        /// File path relative to project root
        path: String,

        /// Symbol depth to include
        #[arg(long, default_value_t = defaults::MAP_FILE_DEPTH)]
        depth: u32,

        /// Maximum tests to show
        #[arg(long, default_value_t = defaults::MAP_FILE_TESTS_LIMIT)]
        tests_limit: usize,
    },
}

#[derive(Debug, Clone)]
struct FileRecord {
    abs_path: PathBuf,
    rel_path: String,
    language: Language,
    is_test: bool,
    parent: String,
    top_dir: String,
}

#[derive(Debug, Serialize)]
struct MapSummaryOutput {
    root: String,
    /// Present (true) only when the walk could not read part of the tree, so
    /// every count below — and the language list itself — is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    incomplete: bool,
    /// The paths that flag stands for, so the shortfall can be checked rather
    /// than only noted.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unread_paths: Vec<String>,
    /// What kept a manifest's declarations from being read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hints: Vec<String>,
    total_files: usize,
    code_files: usize,
    support_files: usize,
    test_files: usize,
    directories: usize,
    languages: Vec<LanguageMapOutput>,
    top_directories: Section<DirectoryMapOutput>,
    /// The packages the workspace manifests list (Cargo `[workspace]`,
    /// npm or yarn `workspaces`, `pnpm-workspace.yaml`, uv
    /// `[tool.uv.workspace]`, `go.work`), largest first; absent where none
    /// declares a workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    packages: Option<Section<PackageOutput>>,
    entrypoints: Section<EntryPointOutput>,
}

#[derive(Debug, Serialize)]
struct LanguageMapOutput {
    language: String,
    file_count: usize,
    test_files: usize,
}

#[derive(Debug, Serialize)]
struct DirectoryMapOutput {
    path: String,
    file_count: usize,
    test_files: usize,
}

#[derive(Debug, Serialize)]
struct EntryPointOutput {
    file: String,
    reason: String,
}

#[derive(Debug)]
struct EntryPointCandidate {
    file: String,
    reason: String,
    score: i32,
}

#[derive(Debug, Serialize)]
struct MapFileOutput {
    file: String,
    language: String,
    test_file: bool,
    /// Present (true) only when the walk could not read somewhere those lists
    /// draw from, so `siblings` and `tests` — read as "this file has no
    /// others beside it" and "no test imports this file" — are a lower
    /// bound. They enumerate rather than rank, which is what makes their
    /// emptiness a claim the walk has to have earned. `siblings` asks about
    /// one directory; `tests` reads test files across the whole project, so
    /// a hole anywhere, or a test file that would not read, bounds it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    incomplete: bool,
    /// The paths that flag stands for, so the shortfall can be checked rather
    /// than only noted.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unread_paths: Vec<String>,
    /// What produced `symbols`, omitted when nothing read the file's
    /// declarations — `symbols` then carries the error, where an empty list
    /// would be a file that declares none.
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<crate::cli::SymbolBackend>,
    siblings: Section<String>,
    /// The tests whose evidence names this file (`tests_of`): each says
    /// which, and the section's hint says what its reading leaves out.
    tests: Section<TestFileOutput>,
    /// The file's declarations in source order, once each.
    symbols: Section<SymbolOutput>,
}

/// A list a limit cut, carrying the size of what it cut from and the cap
/// that cut it. Truncating here and nowhere else keeps `count` describing
/// the set the items were drawn from — a list that reports only what
/// survived reads as the whole of a directory, of a project's entrypoints,
/// or of a file's neighbours.
fn limited<T>(mut items: Vec<T>, limit: usize, cap: Cap) -> Section<T> {
    let total = items.len();
    items.truncate(limit);
    with_cap(Section::with_total(items, total), cap)
}

pub async fn execute(args: MapArgs, app: &App) -> Result<()> {
    match args.command {
        MapCommand::Summary { limit } => execute_summary(app, limit).await,
        MapCommand::File {
            path,
            depth,
            tests_limit,
        } => execute_file(app, &path, depth, tests_limit).await,
    }
}

async fn execute_summary(app: &App, limit: usize) -> Result<()> {
    let ctx = &app.output;
    let mut scan = scan_project_files(app);
    let declared = workspace::read(&scan.records, app.root());
    scan.unread_paths.extend(declared.unread);
    let unread_paths = scan.unread_at(ctx, app.root());
    let records = scan.records;
    let code_records: Vec<_> = records
        .iter()
        .filter(|record| record.language.is_code())
        .collect();

    let mut by_language: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_dir: HashMap<String, (usize, usize)> = HashMap::new();
    let mut dirs = HashSet::new();

    for record in &code_records {
        let entry = by_language
            .entry(record.language.lsp_id().to_string())
            .or_insert((0, 0));
        entry.0 += 1;
        if record.is_test {
            entry.1 += 1;
        }

        let dir_entry = by_dir.entry(record.top_dir.clone()).or_insert((0, 0));
        dir_entry.0 += 1;
        if record.is_test {
            dir_entry.1 += 1;
        }

        if !record.parent.is_empty() {
            dirs.insert(record.parent.clone());
        }
    }

    let languages = by_language
        .into_iter()
        .map(|(language, (file_count, test_files))| LanguageMapOutput {
            language,
            file_count,
            test_files,
        })
        .collect();

    let mut directories: Vec<_> = by_dir
        .into_iter()
        .map(|(path, (file_count, test_files))| DirectoryMapOutput {
            path,
            file_count,
            test_files,
        })
        .collect();
    directories.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.path.cmp(&b.path))
    });

    let top_directories = limited(directories, limit, Cap::Option("--limit"));
    let entrypoints = limited(
        detect_entrypoints(&code_records),
        limit,
        Cap::Option("--limit"),
    );

    let response = MapSummaryOutput {
        root: ctx.root().display().to_string(),
        incomplete: !unread_paths.is_empty(),
        unread_paths,
        hints: declared.hints,
        total_files: records.len(),
        code_files: code_records.len(),
        support_files: records.len().saturating_sub(code_records.len()),
        test_files: code_records.iter().filter(|r| r.is_test).count(),
        directories: dirs.len(),
        languages,
        top_directories,
        packages: declared
            .packages
            .map(|packages| limited(packages, limit, Cap::Option("--limit"))),
        entrypoints,
    };

    ctx.print_success(response);
    Ok(())
}

async fn execute_file(app: &App, path: &str, depth: u32, tests_limit: usize) -> Result<()> {
    let ctx = &app.output;
    let mut scan = scan_project_files(app);
    let Some(target) = find_record(&scan.records, path, app.root()) else {
        ctx.print_error(scan.missing_target(app.root(), path));
        return Ok(());
    };
    let tests = match tests_of::tests_of(
        &target,
        &scan.records,
        app.root(),
        app.config().search.max_file_size_bytes(),
    ) {
        Ok(found) => {
            scan.unread_paths.extend(found.unread);
            // What the reading leaves out leads; the cap, where it cut the
            // list, follows.
            let mut tests = limited(found.items, tests_limit, Cap::Option("--tests-limit"));
            tests.hints.splice(0..0, found.hints);
            tests
        }
        Err(e) => Section::error(e),
    };
    let unread_paths = scan.unread_at(ctx, app.root());
    let records = &scan.records;

    let (backend, symbols) = match crate::cli::declared_in(
        app,
        &target.abs_path,
        FindSymbolsOptions::default().with_depth(depth).with_body(),
    )
    .await
    {
        Ok(answer) => {
            let symbols = answer.symbols;
            // `count` must describe the same domain as `items`: the
            // declarations an overview lists, not every symbol in the file.
            let total = symbols
                .iter()
                .filter(|symbol| is_overview_declaration(symbol))
                .count();
            let items = symbols
                .iter()
                .filter(|symbol| is_overview_declaration(symbol))
                .take(12)
                .map(|symbol| {
                    let mut out = SymbolOutput::from_symbol(symbol, ctx.root()).without_body();
                    out.signature = symbol.signature.clone();
                    out.without_children()
                })
                .collect();
            let every = Cap::Fixed(format!(
                "every declaration: symora symbols -- {}",
                shell_arg(&target.rel_path)
            ));
            (
                Some(answer.backend),
                with_cap(Section::with_total(items, total), every),
            )
        }
        Err(e) => (None, Section::error(e)),
    };

    let siblings = records
        .iter()
        .filter(|r| r.parent == target.parent && r.rel_path != target.rel_path)
        .map(|r| r.rel_path.clone())
        .collect();

    ctx.print_success(MapFileOutput {
        file: target.rel_path,
        language: target.language.lsp_id().to_string(),
        test_file: target.is_test,
        incomplete: !unread_paths.is_empty(),
        unread_paths,
        backend,
        siblings: limited(
            siblings,
            8,
            Cap::Fixed(match target.parent.as_str() {
                "" => "the rest are the other files at the project root".to_string(),
                parent => format!("the rest are the other files in {parent}/"),
            }),
        ),
        tests,
        symbols,
    });
    Ok(())
}

fn scan_project_files(app: &App) -> ProjectScan {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = FileFilter::new(app.root());
    let discovery = filter.discover_files(&extensions);
    let mut files = discovery.files;
    files.sort();

    ProjectScan {
        records: files
            .into_iter()
            .filter_map(|path| build_file_record(&path, app))
            .collect(),
        unread_paths: discovery.unreadable,
    }
}

/// The project as one walk saw it, and WHERE it could not read.
///
/// `map`'s counts are read as facts about the project — a language absent from
/// `languages` is taken to mean the project has none — so a walk that could
/// not enter part of the tree has to say so. Paths rather than a tally because
/// `map file` answers about one file: a count alone would mark it incomplete
/// for a failure that happened somewhere else entirely.
struct ProjectScan {
    records: Vec<FileRecord>,
    unread_paths: Vec<PathBuf>,
}

impl ProjectScan {
    /// The walk's failures that touch `target` — in either direction. A path
    /// it could not read may sit UNDER what was asked about, or may be the
    /// ancestor that hides it; both mean the answer about `target` was
    /// assembled from an incomplete view of it, and checking only one
    /// direction leaves the other reading as a fact about the tree.
    ///
    /// Named rather than counted: an answer that says only "part of the tree
    /// could not be read" leaves a reader with nowhere to check, and `map` has
    /// no rebuild to prescribe — the paths are the whole remedy. The flag
    /// beside them is read off this same list, so the two cannot disagree.
    fn unread_at(&self, ctx: &crate::cli::OutputContext, target: &Path) -> Vec<String> {
        relative_paths(
            ctx,
            &as_paths(
                &self
                    .unread_paths
                    .iter()
                    .filter(|unread| unread.starts_with(target) || target.starts_with(*unread))
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
        )
    }

    fn missed_anything_at(&self, target: &Path) -> bool {
        self.unread_paths
            .iter()
            .any(|unread| unread.starts_with(target) || target.starts_with(unread))
    }

    /// The error for a path the records do not hold. "Not in this project" is
    /// a claim about the tree, and two other facts explain the absence first:
    /// a walk turned away from the subtree the path lives in never learned
    /// it, and a file whose extension names no language was never in the
    /// domain to begin with — the same verdict `symbols` and `refs` give it.
    fn missing_target(&self, root: &Path, path: &str) -> OutputError {
        let full = root.join(path);
        if self.missed_anything_at(&full) {
            return OutputError::new(
                crate::cli::errors::ErrorCode::Io,
                format!("Part of the tree holding {path} could not be read"),
            )
            .with_hint("Check the permissions on that path and retry.");
        }
        if full.is_file() && Language::from_path(&full) == Language::Unknown {
            return OutputError::from(crate::error::LspError::UnsupportedLanguage(
                full.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or(path)
                    .to_string(),
            ));
        }
        OutputError::not_found(format!("File not found in project: {path}"))
    }
}

fn build_file_record(path: &Path, app: &App) -> Option<FileRecord> {
    let language = Language::from_path(path);
    if language == Language::Unknown {
        return None;
    }

    let rel = path.strip_prefix(app.root()).ok()?.display().to_string();
    let parent = Path::new(&rel)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let top_dir = rel
        .split_once('/')
        .map_or_else(|| ".".to_string(), |(directory, _)| directory.to_string());

    Some(FileRecord {
        abs_path: path.to_path_buf(),
        rel_path: rel,
        language,
        is_test: app.test_scope().is_test_file(path),
        parent,
        top_dir,
    })
}

fn find_record(records: &[FileRecord], input: &str, root: &Path) -> Option<FileRecord> {
    let candidate = normalize_path_arg(input, root);
    records
        .iter()
        .find(|record| {
            record.abs_path == candidate || Path::new(&record.rel_path) == Path::new(input)
        })
        .cloned()
}

fn normalize_path_arg(input: &str, root: &Path) -> PathBuf {
    let path = Path::new(input);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn detect_entrypoints(records: &[&FileRecord]) -> Vec<EntryPointOutput> {
    let mut candidates = Vec::new();
    for record in records {
        let file_name = Path::new(&record.rel_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        if let Some((reason, score)) = classify_entrypoint(record, file_name) {
            candidates.push(EntryPointCandidate {
                file: record.rel_path.clone(),
                reason: reason.to_string(),
                score,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| path_depth(&a.file).cmp(&path_depth(&b.file)))
            .then_with(|| a.file.cmp(&b.file))
    });

    let mut top_dir_counts: HashMap<String, usize> = HashMap::new();
    let mut filtered = Vec::new();
    for candidate in candidates {
        let top_dir = candidate
            .file
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let is_low_confidence = candidate.score <= 35;
        let count = top_dir_counts.entry(top_dir).or_insert(0);
        if is_low_confidence && *count >= 1 {
            continue;
        }
        *count += 1;
        filtered.push(EntryPointOutput {
            file: candidate.file,
            reason: candidate.reason,
        });
    }

    filtered
}

fn classify_entrypoint(record: &FileRecord, file_name: &str) -> Option<(&'static str, i32)> {
    let depth_penalty = path_depth(&record.rel_path).min(6) as i32;

    let (reason, base_score) = match file_name {
        "main.rs" | "main.go" | "main.py" | "main.ts" | "main.tsx" | "main.js" | "main.jsx" => {
            ("main entry file", 100)
        }
        "lib.rs" | "lib.go" => ("library root", 92),
        "manage.py" => ("application entry file", 90),
        "app.rs" | "app.py" | "app.ts" | "app.tsx" | "app.js" | "server.rs" | "server.py"
        | "server.ts" | "server.js" | "cli.rs" | "cli.py" | "cli.ts" | "cli.js" => {
            ("application bootstrap candidate", 82)
        }
        "index.ts" | "index.tsx" | "index.js" | "index.jsx" => ("index/export hub", 34),
        _ if record.rel_path.starts_with("src/main.") => ("source entry candidate", 95),
        _ if record.rel_path.starts_with("bin/") => ("binary entry candidate", 88),
        _ => return None,
    };

    Some((reason, base_score - depth_penalty))
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
}

/// Whether an overview lists the declaration: a callable or a type, a
/// module, or a value — a low-level one (a field, a property) only where it
/// holds callables.
fn is_overview_declaration(symbol: &Symbol) -> bool {
    let has_callable_children = symbol.children.iter().any(|child| {
        matches!(
            child.kind,
            crate::models::symbol::SymbolKind::Function
                | crate::models::symbol::SymbolKind::Method
                | crate::models::symbol::SymbolKind::Constructor
        )
    });

    if symbol.kind.is_low_level() && !has_callable_children {
        return false;
    }

    matches!(
        symbol.kind,
        crate::models::symbol::SymbolKind::Function
            | crate::models::symbol::SymbolKind::Method
            | crate::models::symbol::SymbolKind::Constructor
            | crate::models::symbol::SymbolKind::Class
            | crate::models::symbol::SymbolKind::Struct
            | crate::models::symbol::SymbolKind::Enum
            | crate::models::symbol::SymbolKind::Interface
            | crate::models::symbol::SymbolKind::Module
            | crate::models::symbol::SymbolKind::Namespace
            | crate::models::symbol::SymbolKind::Package
            | crate::models::symbol::SymbolKind::Constant
            | crate::models::symbol::SymbolKind::Variable
            | crate::models::symbol::SymbolKind::Object
            | crate::models::symbol::SymbolKind::Impl
    ) || has_callable_children
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three ways a path can be absent from the records carry different
    /// remedies, so they carry different verdicts: fix the path, fix the
    /// permissions, or reach for a command that reads the file as text.
    #[test]
    fn a_missing_target_says_which_absence_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("notes.txt"), "text").unwrap();

        let scan = ProjectScan {
            records: Vec::new(),
            unread_paths: Vec::new(),
        };
        assert_eq!(
            scan.missing_target(root, "notes.txt").code,
            crate::cli::errors::ErrorCode::LanguageNotConfigured
        );
        assert_eq!(
            scan.missing_target(root, "nope.rs").code,
            crate::cli::errors::ErrorCode::NotFound
        );

        let blocked = ProjectScan {
            records: Vec::new(),
            unread_paths: vec![root.join("notes.txt")],
        };
        assert_eq!(
            blocked.missing_target(root, "notes.txt").code,
            crate::cli::errors::ErrorCode::Io
        );
    }
}
