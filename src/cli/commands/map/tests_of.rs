//! The tests `map file` names for a file, each by the evidence that ties it
//! there: a test file whose import statements name the file, a test file its
//! package's build compiles with it, or a region of the file its language
//! compiles only under test.
//!
//! A file that merely shares the name, the directory or the name's words is
//! not evidence. In a workspace several packages hold an `admission.py` or a
//! `deadline.py`, and a list that ranks one of them beside the module's real
//! tests reads as "related" while it is not — so a test is listed here only
//! where its own syntax, or the language's build rule, names this file.
//!
//! Imports are read through [`ImportExtractor`] and matched against the names
//! this file answers to, as each language spells them. Where the language's
//! rules leave the file a reference names open — a JavaScript specifier that
//! could name two files, a Python module name two files answer to — the
//! reference names neither, so the list errs short, never wide; what each
//! reading leaves out is said in the section's hint.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::FileRecord;
use crate::cli::OutputError;
use crate::infra::ast::test_regions;
use crate::models::symbol::Language;
use crate::services::imports::ImportExtractor;

/// What ties a listed test to the file. Declared in the order the list is
/// sorted in, the file's own regions first and the widest claim last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TestEvidence {
    /// A region of the file itself its language compiles only under test
    /// (Rust's `#[cfg(test)]` and `#[test]`).
    TestRegion,
    /// A test file the package's build compiles with this one: a Go
    /// `_test.go` file in its directory.
    SamePackage,
    /// An import naming this file's module, or a class it declares.
    Imports,
    /// An import naming the package this file belongs to: a Go import path,
    /// a JVM wildcard import.
    ImportsPackage,
}

#[derive(Debug, Serialize)]
pub(super) struct TestFileOutput {
    file: String,
    /// The region's first and last lines, for `test_region` alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<u32>,
    evidence: TestEvidence,
}

/// The tests found, the test files that could not be read, and what the
/// language's reading leaves out, with a manifest it could not read where
/// one would have named the file.
pub(super) struct TestsOf {
    pub(super) items: Vec<TestFileOutput>,
    pub(super) unread: Vec<PathBuf>,
    pub(super) hints: Vec<String>,
}

const CONTEXT_REMEDY: &str =
    "`symora context <file>:<line> --tests` lists the tests referencing one declaration.";

/// How a language's tests name a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    Python,
    /// JavaScript and TypeScript, which import each other's files.
    Script,
    /// Java, Kotlin and Scala, which import each other's classes.
    Jvm,
    Go,
    Dart,
    /// Test regions alone: a `use` from another crate's test names the file
    /// only through the crate's module tree, which this reading does not
    /// build.
    Rust,
}

impl Reading {
    fn of(language: Language) -> Option<Self> {
        match language {
            Language::Python => Some(Self::Python),
            Language::JavaScript | Language::TypeScript => Some(Self::Script),
            Language::Java | Language::Kotlin | Language::Scala => Some(Self::Jvm),
            Language::Go => Some(Self::Go),
            Language::Dart => Some(Self::Dart),
            Language::Rust => Some(Self::Rust),
            _ => None,
        }
    }

    /// What the reading leaves out, which an empty list would otherwise be
    /// read as ruling out.
    fn hint(self) -> String {
        let unread = match self {
            Self::Python => {
                "Read from import statements: a test reaching this module through a package \
                 that re-exports it, a fixture or importlib, or by a name several files answer \
                 to, is not listed."
            }
            Self::Script => {
                "Read from relative import and export specifiers: a test importing this file by \
                 a package name or path alias, through a re-export or a CommonJS require() call, \
                 or by a specifier that could name several files, is not listed."
            }
            Self::Jvm => {
                "Read from import statements naming the classes this file declares: a test in \
                 the same package uses them without an import and is not listed."
            }
            Self::Go => {
                "Read from the _test.go files beside this one and the import paths naming its \
                 package: a test reaching it through another package is not listed."
            }
            Self::Dart => {
                "Read from relative and package: imports: a test reaching this library through \
                 another library's export is not listed."
            }
            Self::Rust => {
                "Read from the regions this file compiles only under test: a test in another \
                 file reaching it through `use` is not listed."
            }
        };
        format!("{unread} {CONTEXT_REMEDY}")
    }

    /// Whether a file of `language` can import one this reading reads.
    fn imports_from(self, language: Language) -> bool {
        Self::of(language) == Some(self)
    }
}

/// The tests that name `target`, or why none can be read for its language.
pub(super) fn tests_of(
    target: &FileRecord,
    records: &[FileRecord],
    root: &Path,
    max_file_bytes: u64,
) -> Result<TestsOf, OutputError> {
    let Some(reading) = Reading::of(target.language) else {
        return Err(OutputError::unsupported(format!(
            "The tests of a {} file are not read from its imports",
            target.language.lsp_id()
        ))
        .with_hint(CONTEXT_REMEDY));
    };
    let paths: HashSet<&str> = records.iter().map(|r| r.rel_path.as_str()).collect();
    let mut found = TestsOf {
        items: Vec::new(),
        unread: Vec::new(),
        hints: vec![reading.hint()],
    };

    if reading == Reading::Rust {
        if !target.is_test {
            match std::fs::read_to_string(&target.abs_path) {
                Ok(content) => {
                    found
                        .items
                        .extend(
                            outermost(test_regions(&content, target.language)).map(|region| {
                                TestFileOutput {
                                    file: target.rel_path.clone(),
                                    line: Some(*region.start()),
                                    end_line: Some(*region.end()),
                                    evidence: TestEvidence::TestRegion,
                                }
                            }),
                        )
                }
                Err(e) if crate::infra::hides_text(&e) => {
                    found.unread.push(target.abs_path.clone())
                }
                Err(_) => {}
            }
        }
        return Ok(found);
    }

    let Some(names) = Names::of(reading, target, records, &paths, root) else {
        return Ok(found);
    };
    found.hints.extend(names.problem.clone());
    for record in records {
        if record.rel_path == target.rel_path
            || !record.is_test
            || !reading.imports_from(record.language)
        {
            continue;
        }
        if reading == Reading::Go
            && record.parent == target.parent
            && record.rel_path.ends_with("_test.go")
        {
            found.items.push(listed(record, TestEvidence::SamePackage));
            continue;
        }
        if std::fs::metadata(&record.abs_path).is_ok_and(|m| m.len() > max_file_bytes) {
            continue;
        }
        let content = match std::fs::read_to_string(&record.abs_path) {
            Ok(content) => content,
            Err(e) => {
                if crate::infra::hides_text(&e) {
                    found.unread.push(record.abs_path.clone());
                }
                continue;
            }
        };
        // A necessary condition, not a judgement: an import naming the file
        // spells every word of one of these, unless it is relative and
        // written below the file's directory, where `.` and `..` can name it
        // bare. Only the files passing it are parsed.
        if !names
            .needles
            .iter()
            .any(|words| words.iter().all(|word| content.contains(word.as_str())))
            && !(names.relative && record.parent.starts_with(&target.parent))
        {
            continue;
        }
        if let Some(evidence) = names.named_by(record, &content, &paths) {
            found.items.push(listed(record, evidence));
        }
    }
    found.items.sort_by(|a, b| {
        a.evidence
            .cmp(&b.evidence)
            .then_with(|| a.file.cmp(&b.file))
    });
    Ok(found)
}

fn listed(record: &FileRecord, evidence: TestEvidence) -> TestFileOutput {
    TestFileOutput {
        file: record.rel_path.clone(),
        line: None,
        end_line: None,
        evidence,
    }
}

/// The regions no other one encloses: a `#[test]` function inside a
/// `#[cfg(test)]` module is part of it, not a region of its own.
fn outermost(
    mut regions: Vec<std::ops::RangeInclusive<u32>>,
) -> impl Iterator<Item = std::ops::RangeInclusive<u32>> {
    regions.sort_by(|a, b| a.start().cmp(b.start()).then(b.end().cmp(a.end())));
    let mut kept: Vec<std::ops::RangeInclusive<u32>> = Vec::new();
    for region in regions {
        if kept
            .last()
            .is_some_and(|outer| outer.contains(region.start()) && outer.contains(region.end()))
        {
            continue;
        }
        kept.push(region);
    }
    kept.into_iter()
}

/// What an import has to write to name the target, as its language spells
/// it.
struct Names {
    reading: Reading,
    target: String,
    /// Sets of words an import naming the target writes every one of, for
    /// one set at least.
    needles: Vec<Vec<String>>,
    /// Whether a relative import can name the target without writing one.
    relative: bool,
    /// Python: the module names the target answers to and no other file
    /// does, and its path's components without the extension, which a
    /// relative import names.
    modules: Vec<String>,
    module_path: Vec<String>,
    /// JVM: the qualified names of the classes, functions and properties
    /// the file declares, and its package.
    classes: Vec<String>,
    package: Option<String>,
    /// Go: the package's import path. Dart: the library's `package:` URI.
    import_path: Option<String>,
    /// The manifest that would have named the target and could not be read:
    /// imports of that name are then not looked for, which is said.
    problem: Option<String>,
}

impl Names {
    fn of(
        reading: Reading,
        target: &FileRecord,
        records: &[FileRecord],
        paths: &HashSet<&str>,
        root: &Path,
    ) -> Option<Self> {
        let mut names = Self {
            reading,
            target: target.rel_path.clone(),
            needles: Vec::new(),
            relative: false,
            modules: Vec::new(),
            module_path: Vec::new(),
            classes: Vec::new(),
            package: None,
            import_path: None,
            problem: None,
        };
        match reading {
            Reading::Python => {
                names.module_path = python_module_path(&target.rel_path);
                let taken: HashSet<String> = records
                    .iter()
                    .filter(|other| {
                        other.rel_path != target.rel_path
                            && other.language == Language::Python
                            && python_module_path(&other.rel_path).last()
                                == names.module_path.last()
                    })
                    .flat_map(|other| python_module_names(&other.rel_path, paths))
                    .collect();
                names.modules = python_module_names(&target.rel_path, paths)
                    .into_iter()
                    .filter(|name| !taken.contains(name))
                    .collect();
                // `pkg.mod` is written whole or as `from pkg import mod`, and
                // a relative import writes its dots and the module's name.
                let last = names.module_path.last().cloned().unwrap_or_default();
                names.needles = names
                    .modules
                    .iter()
                    .map(|module| {
                        let top = module.split('.').next().unwrap_or_default();
                        vec![top.to_string(), last.clone()]
                    })
                    .chain([vec!["from .".to_string(), last.clone()]])
                    .collect();
                names.relative = true;
            }
            Reading::Script => {
                names.needles = vec![vec![script_needle(&target.rel_path)]];
                names.relative = true;
            }
            Reading::Jvm => {
                let content = std::fs::read_to_string(&target.abs_path).ok()?;
                let declared = ImportExtractor::shared().extract(&content, target.language);
                let qualify = |name: &String| match &declared.package {
                    Some(package) => format!("{package}.{name}"),
                    None => name.clone(),
                };
                names.classes = declared
                    .declares
                    .iter()
                    .chain(&declared.members)
                    .map(qualify)
                    .collect();
                names.needles = declared
                    .declares
                    .iter()
                    .chain(&declared.members)
                    .cloned()
                    .chain(declared.package.clone())
                    .map(|word| vec![word])
                    .collect();
                names.package = declared.package;
            }
            Reading::Go => {
                match go_import_path(target, root) {
                    Ok(path) => names.import_path = path,
                    Err(problem) => names.problem = Some(problem),
                }
                names.needles = names
                    .import_path
                    .iter()
                    .map(|path| vec![path.clone()])
                    .collect();
            }
            Reading::Dart => {
                match dart_package_uri(&target.rel_path, root) {
                    Ok(uri) => names.import_path = uri,
                    Err(problem) => names.problem = Some(problem),
                }
                names.needles = Path::new(&target.rel_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| vec![name.to_string()])
                    .into_iter()
                    .collect();
            }
            Reading::Rust => return None,
        }
        Some(names)
    }

    /// The evidence an importer's `content` holds that it names the target.
    fn named_by(
        &self,
        importer: &FileRecord,
        content: &str,
        paths: &HashSet<&str>,
    ) -> Option<TestEvidence> {
        let extractor = ImportExtractor::shared();
        let importer_dir = &importer.parent;
        if self.reading == Reading::Python {
            return extractor
                .python_module_names(content)
                .iter()
                .any(|reference| match reference.strip_prefix('.') {
                    Some(_) => {
                        python_relative(importer_dir, reference).as_ref() == Some(&self.module_path)
                    }
                    None => self.modules.iter().any(|module| module == reference),
                })
                .then_some(TestEvidence::Imports);
        }
        let imports = extractor.extract(content, importer.language);
        match self.reading {
            Reading::Python | Reading::Rust => None,
            Reading::Script => imports
                .references
                .iter()
                .chain(&imports.requires)
                .any(|specifier| {
                    script_lookup(importer_dir, specifier, paths) == Some(self.target.as_str())
                })
                .then_some(TestEvidence::Imports),
            Reading::Jvm => {
                let wildcard = self.package.as_ref().map(|package| format!("{package}.*"));
                let mut evidence = None;
                for reference in &imports.references {
                    if self.classes.iter().any(|class| {
                        reference == class
                            || reference
                                .strip_prefix(class.as_str())
                                .is_some_and(|rest| rest.starts_with('.'))
                    }) {
                        return Some(TestEvidence::Imports);
                    }
                    if wildcard.as_ref() == Some(reference) {
                        evidence = Some(TestEvidence::ImportsPackage);
                    }
                }
                evidence
            }
            Reading::Go => {
                let path = self.import_path.as_ref()?;
                imports
                    .references
                    .iter()
                    .any(|reference| reference == path)
                    .then_some(TestEvidence::ImportsPackage)
            }
            Reading::Dart => imports
                .references
                .iter()
                .any(|reference| {
                    if reference.starts_with("package:") {
                        self.import_path.as_ref() == Some(reference)
                    } else {
                        !reference.contains(':')
                            && joined(importer_dir, reference).as_deref()
                                == Some(self.target.as_str())
                    }
                })
                .then_some(TestEvidence::Imports),
        }
    }
}

/// A Python file's path components without its extension, with a package's
/// `__init__` folded onto its directory: what a relative import names it by.
fn python_module_path(rel_path: &str) -> Vec<String> {
    let mut components: Vec<String> = rel_path.split('/').map(str::to_string).collect();
    if let Some(last) = components.last_mut() {
        let stem = last
            .strip_suffix(".pyi")
            .or_else(|| last.strip_suffix(".py"))
            .unwrap_or(last)
            .to_string();
        *last = stem;
    }
    if components.last().is_some_and(|stem| stem == "__init__") {
        components.pop();
    }
    components
}

/// The module names a Python file answers to: the one its packages spell,
/// counting up through each directory holding an `__init__.py`, and the one
/// its path spells from the project root, which a test run from the root
/// imports a namespace package by. A name a component cannot spell (a
/// hyphen, a leading digit) is no module name.
pub(super) fn python_module_names(rel_path: &str, paths: &HashSet<&str>) -> Vec<String> {
    let module_path = python_module_path(rel_path);
    let directories: Vec<&str> = rel_path.split('/').collect();
    let directories = &directories[..directories.len().saturating_sub(1)];
    let mut top = directories.len();
    while top > 0
        && ["__init__.py", "__init__.pyi"]
            .iter()
            .any(|init| paths.contains(format!("{}/{init}", directories[..top].join("/")).as_str()))
    {
        top -= 1;
    }
    let mut names = Vec::new();
    for components in [&module_path[top.min(module_path.len())..], &module_path[..]] {
        if components.is_empty() || !components.iter().all(|c| is_python_identifier(c)) {
            continue;
        }
        let name = components.join(".");
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn is_python_identifier(component: &str) -> bool {
    let mut chars = component.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

/// The module path a relative Python import names from a file in
/// `importer_dir`: one leading dot is the importer's package, each further
/// one a level up.
fn python_relative(importer_dir: &str, reference: &str) -> Option<Vec<String>> {
    let rest = reference.trim_start_matches('.');
    let up = reference.len() - rest.len() - 1;
    let mut path: Vec<String> = importer_dir
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    path.truncate(path.len().checked_sub(up)?);
    path.extend(
        rest.split('.')
            .filter(|c| !c.is_empty())
            .map(str::to_string),
    );
    (!path.is_empty()).then_some(path)
}

/// The word a relative specifier naming a script file writes: its stem, or
/// its directory's name for an `index` file named through the directory.
fn script_needle(rel_path: &str) -> String {
    let path = Path::new(rel_path);
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split('.').next())
        .unwrap_or_default();
    if stem == "index" {
        return path
            .parent()
            .and_then(|dir| dir.file_name())
            .and_then(|dir| dir.to_str())
            .unwrap_or(stem)
            .to_string();
    }
    stem.to_string()
}

const SCRIPT_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts",
];

/// The one project file a relative specifier written in `importer_dir`
/// names: the path it spells, with a TypeScript extension in place of a
/// JavaScript one it writes, with a script extension added, or the `index`
/// of the directory it names. A specifier that is not relative, or that
/// could name several of those files, names none here.
fn script_lookup<'p>(
    importer_dir: &str,
    specifier: &str,
    paths: &HashSet<&'p str>,
) -> Option<&'p str> {
    let relative = specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../");
    if !relative {
        return None;
    }
    let directory_only = specifier.ends_with('/')
        || specifier.ends_with("/.")
        || specifier.ends_with("/..")
        || specifier == "."
        || specifier == "..";
    let base = joined(importer_dir, specifier)?;
    let mut candidates = Vec::new();
    if !directory_only {
        for (written, substitutes) in [
            (".js", &[".ts", ".tsx", ".d.ts"][..]),
            (".jsx", &[".tsx", ".d.ts"][..]),
            (".mjs", &[".mts", ".d.mts"][..]),
            (".cjs", &[".cts", ".d.cts"][..]),
        ] {
            if let Some(stem) = base.strip_suffix(written) {
                candidates.extend(substitutes.iter().map(|ext| format!("{stem}{ext}")));
            }
        }
        candidates.push(base.clone());
        candidates.extend(SCRIPT_EXTENSIONS.iter().map(|ext| format!("{base}{ext}")));
    }
    let index = if base.is_empty() {
        "index".to_string()
    } else {
        format!("{base}/index")
    };
    candidates.extend(SCRIPT_EXTENSIONS.iter().map(|ext| format!("{index}{ext}")));
    let mut named = candidates
        .iter()
        .filter_map(|candidate| paths.get(candidate.as_str()));
    let first = named.next()?;
    named.all(|other| other == first).then_some(*first)
}

/// `reference` read from `directory`, both project-relative, with `.` and
/// `..` folded; `None` where it climbs above the project root.
pub(super) fn joined(directory: &str, reference: &str) -> Option<String> {
    let mut components: Vec<&str> = directory.split('/').filter(|c| !c.is_empty()).collect();
    for component in reference.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            name => components.push(name),
        }
    }
    Some(components.join("/"))
}

/// The import path of the Go package holding `target`: the module path the
/// nearest `go.mod` above it declares, and the directories from there.
/// `Err` names a `go.mod` that would not read, since the packages below it
/// are then imported by a path this cannot spell.
fn go_import_path(target: &FileRecord, root: &Path) -> Result<Option<String>, String> {
    let directories: Vec<&str> = target.parent.split('/').filter(|c| !c.is_empty()).collect();
    for depth in (0..=directories.len()).rev() {
        let relative = [&directories[..depth], &["go.mod"]].concat().join("/");
        let content = match std::fs::read_to_string(root.join(&relative)) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(unread_manifest(&relative, &e.to_string(), "import paths")),
        };
        let module = super::modfile::module_path(&content)
            .map_err(|e| unread_manifest(&relative, &e, "import paths"))?;
        let Some(module) = module else {
            return Ok(None);
        };
        let below = &directories[depth..];
        return Ok(Some(if below.is_empty() {
            module
        } else {
            format!("{module}/{}", below.join("/"))
        }));
    }
    Ok(None)
}

/// The `package:` URI of a Dart library below a package's `lib`: the name
/// the `pubspec.yaml` beside that `lib` declares, and the path below it.
/// `Err` names a `pubspec.yaml` that would not read.
fn dart_package_uri(rel_path: &str, root: &Path) -> Result<Option<String>, String> {
    let components: Vec<&str> = rel_path.split('/').collect();
    let Some(lib) = components.iter().rposition(|c| *c == "lib") else {
        return Ok(None);
    };
    let relative = [&components[..lib], &["pubspec.yaml"]].concat().join("/");
    let content = match std::fs::read_to_string(root.join(&relative)) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(unread_manifest(
                &relative,
                &e.to_string(),
                "package: imports",
            ));
        }
    };
    let documents = yaml_rust2::YamlLoader::load_from_str(&content)
        .map_err(|e| unread_manifest(&relative, &e.to_string(), "package: imports"))?;
    let Some(name) = documents
        .first()
        .and_then(|pubspec| pubspec["name"].as_str())
    else {
        return Ok(None);
    };
    Ok(Some(format!(
        "package:{name}/{}",
        components[lib + 1..].join("/")
    )))
}

fn unread_manifest(path: &str, error: &str, what: &str) -> String {
    format!("{path} could not be read ({error}), so {what} naming this file are not looked for.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    /// A module's name is spelled by the packages enclosing it, counting up
    /// through each `__init__.py`, and by its path from the root; a
    /// component no identifier spells names no module.
    #[test]
    fn a_python_module_answers_to_its_package_names() {
        let files = paths(&[
            "packages/aix-core/src/aix_core/__init__.py",
            "packages/aix-core/src/aix_core/llm/__init__.py",
            "packages/aix-core/src/aix_core/llm/deadline.py",
            "scripts/deploy/build.py",
        ]);
        assert_eq!(
            python_module_names("packages/aix-core/src/aix_core/llm/deadline.py", &files),
            vec!["aix_core.llm.deadline"]
        );
        assert_eq!(
            python_module_names("packages/aix-core/src/aix_core/llm/__init__.py", &files),
            vec!["aix_core.llm"]
        );
        assert_eq!(
            python_module_names("scripts/deploy/build.py", &files),
            vec!["build", "scripts.deploy.build"]
        );
    }

    #[test]
    fn a_relative_python_import_counts_its_dots_up_from_the_importer() {
        let up = |reference| python_relative("tests/unit/core", reference);
        assert_eq!(
            up(".helper"),
            Some(vec![
                "tests".into(),
                "unit".into(),
                "core".into(),
                "helper".into()
            ])
        );
        assert_eq!(
            up("..x.y"),
            Some(vec!["tests".into(), "unit".into(), "x".into(), "y".into()])
        );
        assert_eq!(
            up("."),
            Some(vec!["tests".into(), "unit".into(), "core".into()])
        );
        assert_eq!(up("...."), None);
        assert_eq!(up("....."), None);
    }

    /// A relative specifier names the one file the lookup can take; one that
    /// could name two names neither, and a bare package name none at all.
    #[test]
    fn a_relative_specifier_names_one_file_or_none() {
        let files = paths(&[
            "src/a.ts",
            "src/b.js",
            "src/b.ts",
            "src/lib/index.ts",
            "src/lib.css",
        ]);
        assert_eq!(script_lookup("test", "../src/a", &files), Some("src/a.ts"));
        assert_eq!(
            script_lookup("test", "../src/a.js", &files),
            Some("src/a.ts")
        );
        assert_eq!(
            script_lookup("src", "./lib", &files),
            Some("src/lib/index.ts")
        );
        assert_eq!(
            script_lookup("src/lib", ".", &files),
            Some("src/lib/index.ts")
        );
        assert_eq!(
            script_lookup("src", "./lib.css", &files),
            Some("src/lib.css")
        );
        assert_eq!(script_lookup("src", "./b", &files), None);
        assert_eq!(script_lookup("src", "a", &files), None);
        assert_eq!(script_lookup("", "../a", &files), None);
    }

    #[test]
    fn nested_test_regions_are_one_region() {
        let regions: Vec<_> = outermost(vec![10..=20, 12..=14, 16..=18, 30..=31]).collect();
        assert_eq!(regions, vec![10..=20, 30..=31]);
    }
}
