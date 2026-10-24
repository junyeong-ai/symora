use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::response::disclosure::{as_paths, relative_paths};
use crate::cli::response::{Section, SymbolOutput};
use crate::cli::utils::shell_arg;
use crate::constants::defaults;
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Typical workflow:\n  1. `symora map summary` for entrypoints and major areas\n  2. `symora search symbols <query>` for rough workspace discovery\n  3. `symora map file <path>` for one file, its tests, and its neighbours\n  4. `symora symbols <file>` or `symora refs <loc>` for precise follow-up\n"
)]
pub struct MapArgs {
    #[command(subcommand)]
    pub command: MapCommand,
}

#[derive(Subcommand, Debug)]
pub enum MapCommand {
    /// High-level project map
    Summary {
        /// Maximum top-level directories and entrypoints to show
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

        /// Maximum related files to show
        #[arg(long, default_value_t = defaults::MAP_FILE_RELATED_LIMIT)]
        related_limit: usize,
    },
}

#[derive(Debug, Clone)]
struct FileRecord {
    abs_path: PathBuf,
    rel_path: String,
    language: Language,
    is_test: bool,
    stem: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    next_commands: Vec<String>,
    total_files: usize,
    code_files: usize,
    support_files: usize,
    test_files: usize,
    directories: usize,
    languages: Vec<LanguageMapOutput>,
    top_directories: Section<DirectoryMapOutput>,
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
    /// draw from, so `siblings` and `counterpart_files` — read as "this file
    /// has no others beside it" and "this file has no test" — are a lower
    /// bound. They enumerate rather than rank, which is what makes their
    /// emptiness a claim the walk has to have earned. `siblings` asks about
    /// one directory; `counterpart_files` searches the whole project, so a
    /// hole anywhere bounds it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    incomplete: bool,
    /// The paths that flag stands for, so the shortfall can be checked rather
    /// than only noted.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unread_paths: Vec<String>,
    /// Omitted when nothing read the file's declarations — an empty list is
    /// a file that declares none, which is a different fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_symbols: Option<Vec<FocusSymbolOutput>>,
    /// What produced `symbols` and `focus_symbols`, omitted with them.
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<crate::cli::SymbolBackend>,
    siblings: Section<String>,
    counterpart_files: Vec<String>,
    symbols: Section<SymbolOutput>,
    related_files: Vec<RelatedFileOutput>,
}

#[derive(Debug, Serialize)]
struct RelatedFileOutput {
    file: String,
    language: String,
    score: i32,
    reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct FocusSymbolOutput {
    name: String,
    kind: String,
    file: String,
    line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    child_count: usize,
}

/// A list a limit cut, carrying the size of what it cut from. Truncating
/// here and nowhere else keeps `count` describing the set the items were
/// drawn from — a list that reports only what survived reads as the whole
/// of a directory, of a project's entrypoints, or of a file's neighbours.
fn limited<T>(mut items: Vec<T>, limit: usize) -> Section<T> {
    let total = items.len();
    items.truncate(limit);
    Section::with_total(items, total)
}

pub async fn execute(args: MapArgs, app: &App) -> Result<()> {
    match args.command {
        MapCommand::Summary { limit } => execute_summary(app, limit).await,
        MapCommand::File {
            path,
            depth,
            related_limit,
        } => execute_file(app, &path, depth, related_limit).await,
    }
}

async fn execute_summary(app: &App, limit: usize) -> Result<()> {
    let ctx = &app.output;
    let scan = scan_project_files(app);
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

    let top_directories = limited(directories, limit);
    let entrypoints = limited(detect_entrypoints(&code_records), limit);
    let next_commands = map_summary_next_commands(&top_directories.items, &entrypoints.items);

    let response = MapSummaryOutput {
        root: ctx.root().display().to_string(),
        incomplete: !unread_paths.is_empty(),
        unread_paths,
        total_files: records.len(),
        code_files: code_records.len(),
        support_files: records.len().saturating_sub(code_records.len()),
        test_files: code_records.iter().filter(|r| r.is_test).count(),
        directories: dirs.len(),
        languages,
        top_directories,
        entrypoints,
        next_commands,
    };

    ctx.print_success(response);
    Ok(())
}

async fn execute_file(app: &App, path: &str, depth: u32, related_limit: usize) -> Result<()> {
    let ctx = &app.output;
    let scan = scan_project_files(app);
    let unread_paths = scan.unread_at(ctx, app.root());
    let records = &scan.records;
    let Some(target) = find_record(records, path, app.root()) else {
        ctx.print_error(scan.missing_target(app.root(), path));
        return Ok(());
    };

    let (backend, focus_symbols, symbols) = match crate::cli::declared_in(
        app,
        &target.abs_path,
        FindSymbolsOptions::default().with_depth(depth).with_body(),
    )
    .await
    {
        Ok(answer) => {
            let symbols = answer.symbols;
            let focus_symbols = build_focus_symbols(&symbols, ctx.root());
            // `count` must describe the same domain as `items`: focus
            // candidates, not every symbol in the file.
            let total = symbols
                .iter()
                .filter(|symbol| is_focus_symbol_candidate(symbol))
                .count();
            let items = symbols
                .iter()
                .filter(|symbol| is_focus_symbol_candidate(symbol))
                .take(12)
                .map(|symbol| {
                    let mut out = SymbolOutput::from_symbol(symbol, ctx.root()).without_body();
                    out.signature = symbol.signature.clone();
                    out.without_children()
                })
                .collect();
            (
                Some(answer.backend),
                Some(focus_symbols),
                Section::with_total(items, total),
            )
        }
        Err(e) => (None, None, Section::error(e)),
    };

    let related_files = collect_related_files(&target, records, related_limit);
    let siblings = records
        .iter()
        .filter(|r| r.parent == target.parent && r.rel_path != target.rel_path)
        .map(|r| r.rel_path.clone())
        .collect();
    let counterpart_files = detect_counterparts(&target, records)
        .into_iter()
        .map(|r| r.rel_path.clone())
        .collect();

    ctx.print_success(MapFileOutput {
        file: target.rel_path,
        language: target.language.lsp_id().to_string(),
        test_file: target.is_test,
        incomplete: !unread_paths.is_empty(),
        unread_paths,
        focus_symbols,
        backend,
        siblings: limited(siblings, 8),
        counterpart_files,
        symbols,
        related_files,
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
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    Some(FileRecord {
        abs_path: path.to_path_buf(),
        rel_path: rel,
        language,
        is_test: app.test_scope().is_test_file(path),
        stem,
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

fn detect_counterparts<'a>(target: &FileRecord, records: &'a [FileRecord]) -> Vec<&'a FileRecord> {
    records
        .iter()
        .filter(|candidate| candidate.rel_path != target.rel_path)
        .filter(|candidate| {
            let candidate_stem = normalize_stem(&candidate.stem);
            let target_stem = normalize_stem(&target.stem);
            candidate_stem == target_stem && candidate.language == target.language
        })
        .collect()
}

fn collect_related_files(
    target: &FileRecord,
    records: &[FileRecord],
    limit: usize,
) -> Vec<RelatedFileOutput> {
    if limit == 0 {
        return Vec::new();
    }
    let mut results = collect_related_file_candidates(target, records);
    results.truncate(limit);
    results
}

fn collect_related_file_candidates(
    target: &FileRecord,
    records: &[FileRecord],
) -> Vec<RelatedFileOutput> {
    let target_norm_stem = normalize_stem(&target.stem);
    let mut results = Vec::new();

    for candidate in records {
        if candidate.rel_path == target.rel_path {
            continue;
        }

        let mut score = 0;
        let mut reasons = Vec::new();
        let candidate_norm_stem = normalize_stem(&candidate.stem);

        if candidate_norm_stem == target_norm_stem {
            score += 14;
            reasons.push("same stem".to_string());
        }
        if candidate.parent == target.parent {
            score += 8;
            reasons.push("same directory".to_string());
        }
        if candidate.top_dir == target.top_dir && candidate.parent != target.parent {
            score += 4;
            reasons.push("same top-level area".to_string());
        }
        if candidate.is_test != target.is_test && candidate_norm_stem == target_norm_stem {
            score += 8;
            reasons.push("matching test/source names".to_string());
        }

        let shared_tokens = shared_stem_tokens(&target_norm_stem, &candidate_norm_stem);
        if shared_tokens > 0 {
            score += shared_tokens * 2;
            reasons.push("shared name tokens".to_string());
        }

        if score > 0 {
            results.push(RelatedFileOutput {
                file: candidate.rel_path.clone(),
                language: candidate.language.lsp_id().to_string(),
                score,
                reasons,
            });
        }
    }

    results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.file.cmp(&b.file)));
    results
}

fn normalize_stem(stem: &str) -> String {
    let stem = stem.strip_prefix("test_").unwrap_or(stem);
    ["_test", ".test", ".spec", "-test", "-spec"]
        .iter()
        .find_map(|suffix| stem.strip_suffix(suffix))
        .unwrap_or(stem)
        .to_string()
}

fn shared_stem_tokens(a: &str, b: &str) -> i32 {
    let a_tokens: HashSet<_> = split_tokens(a).into_iter().collect();
    let b_tokens: HashSet<_> = split_tokens(b).into_iter().collect();
    a_tokens.intersection(&b_tokens).count() as i32
}

fn split_tokens(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn build_focus_symbols(symbols: &[Symbol], root: &Path) -> Vec<FocusSymbolOutput> {
    let mut candidates: Vec<_> = symbols
        .iter()
        .filter(|symbol| is_focus_symbol_candidate(symbol))
        .collect();

    candidates.sort_by(|a, b| {
        focus_symbol_score(b)
            .cmp(&focus_symbol_score(a))
            .then_with(|| a.location.line.cmp(&b.location.line))
            .then_with(|| a.name.cmp(&b.name))
    });

    candidates
        .into_iter()
        .take(5)
        .map(|symbol| FocusSymbolOutput {
            name: symbol.name.clone(),
            kind: symbol.kind.to_string(),
            file: symbol
                .location
                .file
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| symbol.location.file.display().to_string()),
            line: symbol.location.line,
            name_path: symbol.name_path.clone(),
            signature: symbol.signature.clone(),
            child_count: symbol.children.len(),
        })
        .collect()
}

fn focus_symbol_score(symbol: &Symbol) -> i32 {
    let base = match symbol.kind {
        crate::models::symbol::SymbolKind::Function
        | crate::models::symbol::SymbolKind::Method
        | crate::models::symbol::SymbolKind::Constructor => 14,
        crate::models::symbol::SymbolKind::Class
        | crate::models::symbol::SymbolKind::Struct
        | crate::models::symbol::SymbolKind::Enum
        | crate::models::symbol::SymbolKind::Interface => 12,
        crate::models::symbol::SymbolKind::Module
        | crate::models::symbol::SymbolKind::Namespace
        | crate::models::symbol::SymbolKind::Package => 10,
        crate::models::symbol::SymbolKind::Constant
        | crate::models::symbol::SymbolKind::Variable
        | crate::models::symbol::SymbolKind::Object => 8,
        crate::models::symbol::SymbolKind::Property
        | crate::models::symbol::SymbolKind::Field
        | crate::models::symbol::SymbolKind::EnumMember => 4,
        _ => 6,
    };

    let name_bonus = match symbol.name.as_str() {
        "main" | "run" | "execute" | "handle" | "process" | "serve" => 4,
        _ => 0,
    };

    let callable_child_bonus = symbol
        .children
        .iter()
        .filter(|child| {
            matches!(
                child.kind,
                crate::models::symbol::SymbolKind::Function
                    | crate::models::symbol::SymbolKind::Method
                    | crate::models::symbol::SymbolKind::Constructor
            )
        })
        .count()
        .min(6) as i32;

    base + (symbol.children.len().min(4) as i32) + callable_child_bonus + name_bonus
}

fn is_focus_symbol_candidate(symbol: &Symbol) -> bool {
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
    ) || has_callable_children
}

fn map_summary_next_commands(
    top_directories: &[DirectoryMapOutput],
    entrypoints: &[EntryPointOutput],
) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(entrypoint) = entrypoints.first() {
        commands.push(format!(
            "symora map file --related-limit 5 -- {}",
            shell_arg(&entrypoint.file)
        ));
    }
    if let Some(dir) = top_directories.first() {
        commands.push(format!(
            "symora pack --tokens 4000 --focus={}",
            shell_arg(&dir.path)
        ));
    }
    if let Some(entrypoint) = entrypoints.first() {
        commands.push(format!(
            "symora symbols --depth 1 -- {}",
            shell_arg(&entrypoint.file)
        ));
    }
    commands.truncate(3);
    commands
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

    #[test]
    fn normalizes_test_suffixes() {
        assert_eq!(normalize_stem("foo_test"), "foo");
        assert_eq!(normalize_stem("test_foo"), "foo");
        assert_eq!(normalize_stem("foo_test_runner"), "foo_test_runner");
        assert_eq!(normalize_stem("foo.spec"), "foo");
        assert_eq!(normalize_stem("foo-test"), "foo");
    }
}
