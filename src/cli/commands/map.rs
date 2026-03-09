use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use futures::future::join_all;
use serde::Serialize;

use crate::app::App;
use crate::cli::response::{Section, SymbolOutput};
use crate::cli::utils::extract_signature;
use crate::infra::file_filter::FileFilter;
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};

#[derive(Args, Debug)]
#[command(
    after_long_help = "Typical workflow:\n  1. `symora map summary` for entrypoints and major areas\n  2. `symora search symbols <query>` for rough workspace discovery\n  3. `symora map file <path>` for one file overview\n  4. `symora map related <path>` when you need adjacent files\n  5. `symora symbols <file>` or `symora refs <loc>` for precise follow-up\n"
)]
pub struct MapArgs {
    #[command(subcommand)]
    pub command: MapCommand,
}

#[derive(Subcommand, Debug)]
pub enum MapCommand {
    /// High-level project map
    Summary {
        /// Maximum directories to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    /// Show map for a single file
    File {
        /// File path relative to project root
        path: String,

        /// Symbol depth to include
        #[arg(long, default_value = "1")]
        depth: u32,

        /// Maximum related files to show
        #[arg(long, default_value = "8")]
        related_limit: usize,
    },
    /// Show map for a directory
    Dir {
        /// Directory path relative to project root
        #[arg(default_value = ".")]
        path: String,

        /// Maximum child directories/files to show
        #[arg(long, default_value = "12")]
        limit: usize,
    },
    /// Suggest related files for a file
    Related {
        /// File path relative to project root
        path: String,

        /// Maximum related files to show
        #[arg(long, default_value = "10")]
        limit: usize,
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
    total_files: usize,
    code_files: usize,
    support_files: usize,
    test_files: usize,
    directories: usize,
    languages: Vec<LanguageMapOutput>,
    top_directories: Vec<DirectoryMapOutput>,
    entrypoints: Vec<EntryPointOutput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    next_commands: Vec<String>,
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
    focus_symbols: Vec<FocusSymbolOutput>,
    siblings: Vec<String>,
    counterpart_files: Vec<String>,
    symbols: Section<SymbolOutput>,
    related_files: Vec<RelatedFileOutput>,
}

#[derive(Debug, Serialize)]
struct MapDirOutput {
    path: String,
    file_count: usize,
    test_files: usize,
    languages: Vec<LanguageMapOutput>,
    child_directories: Vec<DirectoryMapOutput>,
    files: Vec<DirFileOutput>,
}

#[derive(Debug, Serialize)]
struct DirFileOutput {
    path: String,
    language: String,
    test_file: bool,
}

#[derive(Debug, Serialize)]
struct MapRelatedOutput {
    target: String,
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

#[derive(Default)]
struct SymbolProfile {
    names: HashSet<String>,
    tokens: HashSet<String>,
    import_tokens: HashSet<String>,
}

pub async fn execute(args: MapArgs, app: &App) -> Result<()> {
    match args.command {
        MapCommand::Summary { limit } => execute_summary(app, limit).await,
        MapCommand::File {
            path,
            depth,
            related_limit,
        } => execute_file(app, &path, depth, related_limit).await,
        MapCommand::Dir { path, limit } => execute_dir(app, &path, limit).await,
        MapCommand::Related { path, limit } => execute_related(app, &path, limit).await,
    }
}

async fn execute_summary(app: &App, limit: usize) -> Result<()> {
    let ctx = &app.output;
    let records = scan_project_files(app);
    let code_records: Vec<_> = records
        .iter()
        .filter(|record| is_code_language(record.language))
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

    let mut top_directories: Vec<_> = by_dir
        .into_iter()
        .map(|(path, (file_count, test_files))| DirectoryMapOutput {
            path,
            file_count,
            test_files,
        })
        .collect();
    top_directories.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.path.cmp(&b.path))
    });
    top_directories.truncate(limit);

    let entrypoints = detect_entrypoints(&code_records, limit);
    let next_commands = map_summary_next_commands(&top_directories, &entrypoints);

    let response = MapSummaryOutput {
        root: ctx.root().display().to_string(),
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
    let records = scan_project_files(app);
    let Some(target) = find_record(&records, path, app.root()) else {
        ctx.print_error(&format!("File not found in project: {}", path));
        return Ok(());
    };

    let language_status = app.lsp.server_status(target.language).await;
    if matches!(
        language_status,
        crate::models::lsp::ServerStatus::NotInstalled { .. }
    ) {
        let file = target.rel_path.clone();
        ctx.print_success(MapFileOutput {
            file,
            language: target.language.lsp_id().to_string(),
            test_file: target.is_test,
            focus_symbols: Vec::new(),
            siblings: records
                .iter()
                .filter(|r| r.parent == target.parent && r.rel_path != target.rel_path)
                .map(|r| r.rel_path.clone())
                .take(8)
                .collect(),
            counterpart_files: detect_counterparts(&target, &records)
                .into_iter()
                .map(|r| r.rel_path.clone())
                .collect(),
            symbols: Section::error(format!(
                "Language server is not installed for {}. Run `symora doctor {}` for installation help.",
                target.language.lsp_id(),
                target.language.lsp_id()
            )),
            related_files: collect_related_files(app, &target, &records, related_limit).await,
        });
        return Ok(());
    }

    let (focus_symbols, symbols) = match app
        .lsp
        .find_symbols(
            &target.abs_path,
            FindSymbolsOptions::default().with_depth(depth).with_body(),
        )
        .await
    {
        Ok(mut symbols) => {
            Symbol::compute_paths_for_all(&mut symbols);
            let focus_symbols = build_focus_symbols(&symbols, ctx.root());
            let total = symbols.len();
            let items = symbols
                .iter()
                .filter(|symbol| is_focus_symbol_candidate(symbol))
                .take(12)
                .map(|symbol| {
                    let mut out = SymbolOutput::from_symbol(symbol, ctx.root()).without_body();
                    out.signature = extract_signature(symbol.body.as_deref());
                    out.without_children()
                })
                .collect();
            (focus_symbols, Section::with_limit(items, total))
        }
        Err(e) => (Vec::new(), Section::error(e.to_string())),
    };

    let related_files = collect_related_files(app, &target, &records, related_limit).await;
    let siblings = records
        .iter()
        .filter(|r| r.parent == target.parent && r.rel_path != target.rel_path)
        .map(|r| r.rel_path.clone())
        .take(8)
        .collect();
    let counterpart_files = detect_counterparts(&target, &records)
        .into_iter()
        .map(|r| r.rel_path.clone())
        .collect();

    ctx.print_success(MapFileOutput {
        file: target.rel_path,
        language: target.language.lsp_id().to_string(),
        test_file: target.is_test,
        focus_symbols,
        siblings,
        counterpart_files,
        symbols,
        related_files,
    });
    Ok(())
}

async fn execute_dir(app: &App, path: &str, limit: usize) -> Result<()> {
    let ctx = &app.output;
    let records = scan_project_files(app);
    let dir = normalize_dir_arg(path);

    let records_in_dir: Vec<_> = records
        .iter()
        .filter(|r| is_under_dir(&r.rel_path, &dir))
        .cloned()
        .collect();

    if records_in_dir.is_empty() {
        ctx.print_error(&format!("Directory has no indexed source files: {}", path));
        return Ok(());
    }

    let mut by_language: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut child_dirs: HashMap<String, (usize, usize)> = HashMap::new();

    for record in &records_in_dir {
        let entry = by_language
            .entry(record.language.lsp_id().to_string())
            .or_insert((0, 0));
        entry.0 += 1;
        if record.is_test {
            entry.1 += 1;
        }

        if let Some(child) = immediate_child_dir(&dir, &record.rel_path) {
            let child_entry = child_dirs.entry(child).or_insert((0, 0));
            child_entry.0 += 1;
            if record.is_test {
                child_entry.1 += 1;
            }
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

    let mut child_directories: Vec<_> = child_dirs
        .into_iter()
        .map(|(path, (file_count, test_files))| DirectoryMapOutput {
            path,
            file_count,
            test_files,
        })
        .collect();
    child_directories.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.path.cmp(&b.path))
    });
    child_directories.truncate(limit);

    let mut files: Vec<_> = records_in_dir
        .iter()
        .filter(|r| immediate_child_dir(&dir, &r.rel_path).is_none())
        .map(|r| DirFileOutput {
            path: r.rel_path.clone(),
            language: r.language.lsp_id().to_string(),
            test_file: r.is_test,
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.truncate(limit);

    ctx.print_success(MapDirOutput {
        path: if dir.is_empty() { ".".to_string() } else { dir },
        file_count: records_in_dir.len(),
        test_files: records_in_dir.iter().filter(|r| r.is_test).count(),
        languages,
        child_directories,
        files,
    });
    Ok(())
}

async fn execute_related(app: &App, path: &str, limit: usize) -> Result<()> {
    let ctx = &app.output;
    let records = scan_project_files(app);
    let Some(target) = find_record(&records, path, app.root()) else {
        ctx.print_error(&format!("File not found in project: {}", path));
        return Ok(());
    };

    ctx.print_success(MapRelatedOutput {
        target: target.rel_path.clone(),
        related_files: collect_related_files(app, &target, &records, limit).await,
    });
    Ok(())
}

fn scan_project_files(app: &App) -> Vec<FileRecord> {
    let extensions: Vec<&str> = Language::all()
        .into_iter()
        .flat_map(|lang| lang.extensions().iter().copied())
        .collect();
    let filter = FileFilter::with_gitignore(app.root());
    let mut files = filter.discover_files(&extensions);
    files.sort();

    files
        .into_iter()
        .filter_map(|path| build_file_record(&path, app))
        .collect()
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
        .split('/')
        .next()
        .map(|s| s.to_string())
        .unwrap_or_else(|| rel.clone());
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    Some(FileRecord {
        abs_path: path.to_path_buf(),
        rel_path: rel,
        language,
        is_test: app.test_matcher().is_test_file(path),
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

fn normalize_dir_arg(input: &str) -> String {
    let trimmed = input.trim_matches('/');
    if trimmed == "." || trimmed.is_empty() {
        String::new()
    } else {
        trimmed.to_string()
    }
}

fn is_under_dir(rel_path: &str, dir: &str) -> bool {
    dir.is_empty() || rel_path == dir || rel_path.starts_with(&format!("{dir}/"))
}

fn immediate_child_dir(dir: &str, rel_path: &str) -> Option<String> {
    let rest = if dir.is_empty() {
        rel_path
    } else {
        rel_path.strip_prefix(&format!("{dir}/"))?
    };
    let (first, _) = rest.split_once('/')?;
    Some(if dir.is_empty() {
        first.to_string()
    } else {
        format!("{dir}/{first}")
    })
}

fn detect_entrypoints(records: &[&FileRecord], limit: usize) -> Vec<EntryPointOutput> {
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
        if filtered.len() >= limit {
            break;
        }
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
        _ if record.rel_path.ends_with("/main.py")
            || record.rel_path.ends_with("/main.ts")
            || record.rel_path.ends_with("/main.js") =>
        {
            ("module entry candidate", 78)
        }
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

async fn collect_related_files(
    app: &App,
    target: &FileRecord,
    records: &[FileRecord],
    limit: usize,
) -> Vec<RelatedFileOutput> {
    let mut results = collect_related_files_heuristic(target, records);
    if results.is_empty() {
        return results;
    }

    let probe_limit = usize::min(usize::max(limit.saturating_mul(3), 12), 24);
    let target_profile = load_symbol_profile(app, &target.abs_path).await;
    if target_profile.names.is_empty()
        && target_profile.tokens.is_empty()
        && target_profile.import_tokens.is_empty()
    {
        results.truncate(limit);
        return results;
    }

    let top_files: Vec<PathBuf> = results
        .iter()
        .take(probe_limit)
        .map(|item| app.root().join(&item.file))
        .collect();
    let profiles = join_all(top_files.iter().map(|path| load_symbol_profile(app, path))).await;

    for (item, profile) in results
        .iter_mut()
        .take(probe_limit)
        .zip(profiles.into_iter())
    {
        let exact_shared = target_profile.names.intersection(&profile.names).count() as i32;
        if exact_shared > 0 {
            item.score += exact_shared.min(4) * 3;
            item.reasons.push("shared symbols".to_string());
        }

        let token_shared = target_profile.tokens.intersection(&profile.tokens).count() as i32;
        if token_shared > 0 {
            item.score += token_shared.min(6);
            item.reasons.push("shared symbol tokens".to_string());
        }

        let shared_imports = target_profile
            .import_tokens
            .intersection(&profile.import_tokens)
            .count() as i32;
        if shared_imports > 0 {
            item.score += shared_imports.min(4) * 2;
            item.reasons.push("shared imports".to_string());
        }

        let imports_target_symbols = profile
            .import_tokens
            .intersection(&target_profile.names)
            .count() as i32;
        if imports_target_symbols > 0 {
            item.score += imports_target_symbols.min(3) * 4;
            item.reasons.push("imports target symbols".to_string());
        }

        let target_imports_candidate = target_profile
            .import_tokens
            .intersection(&profile.names)
            .count() as i32;
        if target_imports_candidate > 0 {
            item.score += target_imports_candidate.min(2) * 3;
            item.reasons.push("target imports symbols".to_string());
        }
    }

    for item in &mut results {
        dedupe_reasons(&mut item.reasons);
    }
    results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.file.cmp(&b.file)));
    results.truncate(limit);
    results
}

fn collect_related_files_heuristic(
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
            reasons.push("test/source counterpart".to_string());
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
    stem.replace("_test", "")
        .replace(".test", "")
        .replace(".spec", "")
        .replace("-test", "")
        .replace("-spec", "")
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

async fn load_symbol_profile(app: &App, file: &Path) -> SymbolProfile {
    let Ok(mut symbols) = app
        .lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(1))
        .await
    else {
        return SymbolProfile::default();
    };

    Symbol::compute_paths_for_all(&mut symbols);
    let filtered = Symbol::filter_advanced(&symbols, None, false, None, None, true);
    let mut profile = SymbolProfile::default();
    for symbol in filtered {
        profile.names.insert(symbol.name.to_ascii_lowercase());
        for token in split_symbol_tokens(&symbol.name) {
            profile.tokens.insert(token);
        }
        if let Some(container) = symbol.container {
            for token in split_symbol_tokens(&container) {
                profile.tokens.insert(token);
            }
        }
    }

    if let Ok(content) = tokio::fs::read_to_string(file).await {
        for token in extract_import_tokens(&content) {
            profile.import_tokens.insert(token);
        }
    }

    profile
}

fn extract_import_tokens(content: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    for line in content.lines().take(300) {
        let trimmed = line.trim();
        if !looks_like_import_line(trimmed) {
            continue;
        }

        for token in split_symbol_tokens(trimmed) {
            if token.len() >= 3 {
                tokens.insert(token);
            }
        }
    }
    tokens
}

fn looks_like_import_line(line: &str) -> bool {
    let prefixes = [
        "use ",
        "pub use ",
        "import ",
        "from ",
        "#include ",
        "include ",
        "require(",
        "require ",
        "export * from ",
        "export {",
    ];
    prefixes.iter().any(|prefix| line.starts_with(prefix))
}

fn split_symbol_tokens(value: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(value.len() * 2);
    let mut prev_is_lower = false;
    for ch in value.chars() {
        if ch.is_ascii_uppercase() && prev_is_lower {
            normalized.push('_');
        }
        prev_is_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        normalized.push(ch.to_ascii_lowercase());
    }
    split_tokens(&normalized)
}

fn dedupe_reasons(reasons: &mut Vec<String>) {
    let mut seen = HashSet::new();
    reasons.retain(|reason| seen.insert(reason.clone()));
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
            signature: extract_signature(symbol.body.as_deref()),
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
            "symora map file {} --related-limit 5",
            entrypoint.file
        ));
    }
    if let Some(dir) = top_directories.first() {
        commands.push(format!("symora map dir {} --limit 10", dir.path));
    }
    if let Some(entrypoint) = entrypoints.first() {
        commands.push(format!("symora symbols {} --depth 1", entrypoint.file));
    }
    commands.truncate(3);
    commands
}

fn is_code_language(language: Language) -> bool {
    !matches!(
        language,
        Language::Markdown | Language::Toml | Language::Yaml
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dir_arg() {
        assert_eq!(normalize_dir_arg("."), "");
        assert_eq!(normalize_dir_arg("src/"), "src");
    }

    #[test]
    fn finds_immediate_child_dir() {
        assert_eq!(
            immediate_child_dir("", "src/cli/mod.rs"),
            Some("src".to_string())
        );
        assert_eq!(
            immediate_child_dir("src", "src/cli/mod.rs"),
            Some("src/cli".to_string())
        );
        assert_eq!(immediate_child_dir("src", "src/main.rs"), None);
    }

    #[test]
    fn normalizes_test_suffixes() {
        assert_eq!(normalize_stem("foo_test"), "foo");
        assert_eq!(normalize_stem("foo.spec"), "foo");
        assert_eq!(normalize_stem("foo-test"), "foo");
    }
}
