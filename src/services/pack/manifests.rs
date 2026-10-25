//! The build manifests and configuration files that declare the names a
//! project's imports are written against, read by each file's own format.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::Node;
use super::graph::{is_javascript, split_components};
use crate::models::symbol::Language;

/// What a project's build manifests declare about the names its own imports
/// are written against, where those names are not paths on disk. Reading them
/// is how the languages themselves resolve such imports; inferring the names
/// from the tree would be a guess about which components are real.
#[derive(Default)]
pub(super) struct Manifests {
    /// The project directory the manifests were read in, where a file a
    /// configuration names is looked for on disk; none where they were not
    /// read from one.
    pub(super) root: Option<PathBuf>,
    /// The module path each `go.mod` declares, by the directory it roots. Go
    /// has no relative import form in a module, so every import of a project
    /// package spells its module's path in full.
    pub(super) go_modules: HashMap<Vec<String>, Vec<String>>,
    /// The name each `pubspec.yaml` gives the Dart package rooted at its
    /// directory: a `package:name/x.dart` URI is that directory's
    /// `lib/x.dart`.
    pub(super) dart_packages: HashMap<Vec<String>, String>,
    /// The Cargo package each `Cargo.toml` with a `[package]` roots at its
    /// directory.
    pub(super) rust_packages: HashMap<Vec<String>, RustPackage>,
    /// What each Python project puts on `sys.path`, by the directory of the
    /// build configuration that makes it one.
    pub(super) python_projects: HashMap<Vec<String>, PythonProject>,
    /// How pytest runs from each directory whose files configure it.
    pub(super) pytest_configurations: HashMap<Vec<String>, Pytest>,
    /// The TypeScript projects the tree's configurations describe.
    pub(super) ts_projects: TsProjects,
    /// Each package a JavaScript workspace lists, by its directory.
    pub(super) js_packages: HashMap<Vec<String>, JsPackage>,
    /// Whether each `package.json` above a JavaScript or TypeScript file
    /// declares `"type": "module"`, by its directory: the nearest decides
    /// whether Node reads a `.js` or `.ts` file there as an ES module.
    pub(super) module_types: HashMap<Vec<String>, bool>,
}

/// A package of a JavaScript workspace: the name its `package.json` gives
/// it, the workspace root whose `workspaces` (or `pnpm-workspace.yaml`) makes
/// it a member, and the fields that say which file the name and its
/// subpaths stand for.
#[derive(Debug, Clone)]
pub(super) struct JsPackage {
    pub(super) name: String,
    pub(super) workspace: Vec<String>,
    pub(super) exports: Option<serde_json::Value>,
    pub(super) types: Option<String>,
    pub(super) main: Option<String>,
}

/// Every TypeScript project the tree's configurations describe, each read
/// once however many `references` name it.
#[derive(Debug, Default)]
pub(super) struct TsProjects {
    /// Each configuration file read, by its project path.
    projects: Vec<TsProject>,
    /// The project each directory's configuration describes: its
    /// `tsconfig.json`, or a `jsconfig.json` where it holds none.
    by_directory: HashMap<Vec<String>, usize>,
}

/// A TypeScript configuration as the project `tsc -b` builds from it: what
/// it says about its files' specifiers, the files it compiles itself, and
/// the projects its `references` name, by their indices in `TsProjects`.
#[derive(Debug, Clone, Default)]
struct TsProject {
    config: TsConfig,
    sources: TsSources,
    references: Vec<usize>,
}

/// The files a TypeScript project compiles itself (`ts_sources`).
#[derive(Debug, Clone, Default)]
struct TsSources {
    files: HashSet<PathBuf>,
    include: globset::GlobSet,
    exclude: globset::GlobSet,
}

impl TsSources {
    fn covers(&self, file: &Path) -> bool {
        self.files.contains(file) || (self.include.is_match(file) && !self.exclude.is_match(file))
    }
}

/// The configuration a JavaScript or TypeScript file is compiled under.
pub(super) enum TsOwner<'a> {
    /// No configuration is above it.
    None,
    Config(&'a TsConfig),
    /// The configuration above it has `references`, and no project they
    /// reach compiles it, so `tsc -b` builds it in none: the configurations
    /// of the nearest project and of those it reaches are what an import's
    /// writer could have meant.
    Uncompiled(Vec<&'a TsConfig>),
    /// The projects the `references` above it reach that compile it say
    /// different things about specifiers.
    Undetermined,
}

impl TsProjects {
    /// The configurations of the project at `project` and of every project
    /// its references reach, each once.
    fn reached(&self, project: usize) -> Vec<&TsConfig> {
        let mut reached = Vec::new();
        let mut visited = HashSet::new();
        let mut pending = vec![project];
        while let Some(at) = pending.pop() {
            if visited.insert(at) {
                reached.push(&self.projects[at].config);
                pending.extend(self.projects[at].references.iter().rev());
            }
        }
        reached
    }

    /// The configurations `file` is compiled under in the project at
    /// `project`: its own where it compiles the file itself — or has no
    /// `references`, a configuration's files being whatever is below it —
    /// else those of the projects its references reach, each once, that
    /// compile it, as `tsc -b` builds them; a project compiling the file
    /// stands for those it references.
    fn owners(&self, project: usize, file: &Path) -> Vec<&TsConfig> {
        let own = &self.projects[project];
        if own.references.is_empty() || own.sources.covers(file) {
            return vec![&own.config];
        }
        let mut owners = Vec::new();
        let mut visited = HashSet::from([project]);
        let mut pending: Vec<usize> = own.references.iter().rev().copied().collect();
        while let Some(at) = pending.pop() {
            if !visited.insert(at) {
                continue;
            }
            let referenced = &self.projects[at];
            if referenced.sources.covers(file) {
                owners.push(&referenced.config);
            } else {
                pending.extend(referenced.references.iter().rev());
            }
        }
        owners
    }
}

/// What a TypeScript configuration, with every configuration it extends,
/// says about the specifiers its files import: the directory `baseUrl`
/// names, each `paths` pattern with its substitutions, which resolve
/// against `baseUrl` or, without one, the directory of the configuration
/// declaring them, and the conditions a package's `exports` is read under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TsConfig {
    pub(super) base_url: Option<Vec<String>>,
    pub(super) paths: Vec<(String, Vec<String>)>,
    pub(super) paths_base: Vec<String>,
    /// The module resolution its `moduleResolution`, or the `module` that
    /// implies one, selects, where that is one deciding how TypeScript reads
    /// a package's `exports`; `None` where the versions of TypeScript read
    /// the configuration differently.
    pub(super) module_resolution: Option<ModuleResolution>,
    /// The conditions `customConditions` adds.
    pub(super) custom_conditions: Vec<String>,
}

/// A module resolution strategy that says how a package's `exports` is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModuleResolution {
    /// `bundler`: an import matches `import`, never `require` or `node`.
    Bundler,
    /// `node16` or `nodenext`: an import matches `node`, and `import` or
    /// `require` by the module format of the file writing it.
    Node16,
    /// `node10`, or `node` as it was first named: `exports` is not read, and
    /// a package's name stands for its `types`, `main` or `index`.
    Node10,
    /// `classic`: no package's `package.json` is read, so a package's name
    /// names none of its files.
    Classic,
}

/// What the `paths` pattern a specifier matches maps it to.
pub(super) struct Mapping {
    /// The project paths its substitutions name, in the order written.
    pub(super) paths: Vec<Vec<String>>,
    /// Whether the pattern is `*`, which every specifier matches: one it maps
    /// to no file is looked up as a package, as any unmapped one is.
    pub(super) catch_all: bool,
}

impl TsConfig {
    /// What the `paths` pattern a specifier matches maps it to, or `None`
    /// where it matches none. An exact pattern wins, then the one whose
    /// prefix before `*` is longest, as TypeScript picks.
    pub(super) fn mapped(&self, specifier: &str) -> Option<Mapping> {
        let (pattern, substitutions, star) = self
            .paths
            .iter()
            .find(|(pattern, _)| pattern == specifier)
            .map(|(pattern, substitutions)| (pattern, substitutions, ""))
            .or_else(|| {
                self.paths
                    .iter()
                    .filter_map(|(pattern, substitutions)| {
                        let (prefix, suffix) = pattern.split_once('*')?;
                        let star = specifier.strip_prefix(prefix)?.strip_suffix(suffix)?;
                        Some((prefix.len(), pattern, substitutions, star))
                    })
                    .max_by_key(|(prefix, ..)| *prefix)
                    .map(|(_, pattern, substitutions, star)| (pattern, substitutions, star))
            })?;
        Some(Mapping {
            paths: substitutions
                .iter()
                .filter_map(|substitution| {
                    project_path(&self.paths_base, &substitution.replacen('*', star, 1))
                })
                .collect(),
            catch_all: pattern == "*",
        })
    }
}

/// What Cargo builds of a package: its library, where it has one, and the
/// root file of each crate it builds, where that crate's module tree starts.
#[derive(Debug, Clone, Default)]
pub(super) struct RustPackage {
    pub(super) library: Option<RustLibrary>,
    pub(super) crates: Vec<Vec<String>>,
}

/// A Cargo package's library: the name other crates import it by, and its
/// root file.
#[derive(Debug, Clone)]
pub(super) struct RustLibrary {
    pub(super) name: String,
    pub(super) root: Vec<String>,
}

impl Manifests {
    pub(super) fn read(root: &Path, nodes: &[Node]) -> Self {
        let go_modules =
            manifests_above(root, nodes, |language| language == Language::Go, "go.mod")
                .into_iter()
                .filter_map(|(directory, go_mod)| Some((directory, go_module_path(&go_mod)?)))
                .collect();
        let dart_packages = manifests_above(
            root,
            nodes,
            |language| language == Language::Dart,
            "pubspec.yaml",
        )
        .into_iter()
        .filter_map(|(directory, pubspec)| Some((directory, dart_package_name(&pubspec)?)))
        .collect();
        let cargo_manifests: Vec<(Vec<String>, toml::Table)> = manifests_above(
            root,
            nodes,
            |language| language == Language::Rust,
            "Cargo.toml",
        )
        .into_iter()
        .filter_map(|(directory, cargo)| Some((directory, toml::from_str(&cargo).ok()?)))
        .filter(|(_, manifest): &(_, toml::Table)| manifest.contains_key("package"))
        .collect();
        // Each Rust file is its nearest package's.
        let mut package_files: HashMap<&[String], Vec<Vec<String>>> = cargo_manifests
            .iter()
            .map(|(directory, _)| (directory.as_slice(), Vec::new()))
            .collect();
        for node in nodes.iter().filter(|node| node.language == Language::Rust) {
            let file = split_components(&node.rel_path);
            let package = (0..=node.directory.len())
                .rev()
                .map(|depth| &node.directory[..depth])
                .find(|directory| package_files.contains_key(directory));
            if let Some(files) = package.and_then(|package| package_files.get_mut(package)) {
                files.push(file);
            }
        }
        let rust_packages = cargo_manifests
            .iter()
            .filter_map(|(directory, manifest)| {
                let files = package_files.get(directory.as_slice())?;
                let package = cargo_package(directory, manifest, files)?;
                Some((directory.clone(), package))
            })
            .collect();
        let package_jsons: Vec<(Vec<String>, serde_json::Value)> =
            manifests_above(root, nodes, is_javascript, "package.json")
                .into_iter()
                .filter_map(|(directory, text)| {
                    Some((directory, serde_json::from_str(&text).ok()?))
                })
                .collect();
        let module_types = package_jsons
            .iter()
            .map(|(directory, manifest)| {
                let module = manifest.get("type").and_then(|kind| kind.as_str()) == Some("module");
                (directory.clone(), module)
            })
            .collect();
        let (python_projects, pytest_configurations) = python_configurations(root, nodes);
        Self {
            root: Some(root.to_path_buf()),
            go_modules,
            dart_packages,
            rust_packages,
            python_projects,
            pytest_configurations,
            ts_projects: ts_projects(root, nodes),
            js_packages: js_packages(root, nodes, &package_jsons),
            module_types,
        }
    }

    /// The root file of the project library a Rust path's first segment
    /// names, by the rule every manifest name follows (`own_or_only`).
    pub(super) fn rust_library(&self, name: &str, from: &Node) -> Option<&[String]> {
        own_or_only(&self.rust_packages, from, |package| {
            package
                .library
                .as_ref()
                .is_some_and(|library| library.name == name)
        })
        .and_then(|(_, package)| package.library.as_ref())
        .map(|library| library.root.as_slice())
    }

    /// The directory a Go import path names in the project, as the go command
    /// finds it. The module providing a package is the one with the longest
    /// path the import path begins with — where two `go.mod` files declare
    /// that path, the one every manifest name means (`own_or_only`) — and the
    /// package is the directory as far below that module's root, unless a
    /// `go.mod` of its own makes that directory another module, which the
    /// first does not contain. An import path under no module the project
    /// declares names a package the importer's own module vendors, or nothing
    /// in the project.
    pub(super) fn go_package(&self, import: &[String], from: &Node) -> Option<Vec<String>> {
        let longest = self
            .go_modules
            .values()
            .filter(|module| import.starts_with(module))
            .map(Vec::len)
            .max();
        let Some(longest) = longest else {
            let (own, _) = nearest(&self.go_modules, &from.directory)?;
            let mut vendored = own.clone();
            vendored.push("vendor".to_string());
            vendored.extend_from_slice(import);
            return Some(vendored);
        };
        let (root, _) = own_or_only(&self.go_modules, from, |module| {
            module.len() == longest && import.starts_with(module)
        })?;
        let mut directory = root.clone();
        directory.extend_from_slice(&import[longest..]);
        let nested = self
            .go_modules
            .keys()
            .any(|other| other.len() > root.len() && directory.starts_with(other));
        (!nested).then_some(directory)
    }

    /// The configuration a JavaScript or TypeScript file is compiled under:
    /// the nearest above it, or, where that one has `references` and does
    /// not compile the file itself, the one the referenced projects compiling
    /// it agree on (`TsProjects::owners`). Projects that say different things
    /// about specifiers leave it undetermined; where none compiles it, no
    /// configuration applies to it.
    pub(super) fn ts_owner(&self, from: &Node) -> TsOwner<'_> {
        let projects = &self.ts_projects;
        let Some((_, project)) = nearest(&projects.by_directory, &from.directory) else {
            return TsOwner::None;
        };
        let owners = projects.owners(*project, Path::new(&from.rel_path));
        match owners.split_first() {
            None => TsOwner::Uncompiled(projects.reached(*project)),
            Some((first, rest)) if rest.iter().all(|other| other == first) => {
                TsOwner::Config(first)
            }
            Some(_) => TsOwner::Undetermined,
        }
    }

    /// Whether a file a JavaScript or TypeScript path names is on disk, as
    /// TypeScript looks for one: the file as written, with a script
    /// extension it leaves out or in place of the JavaScript one it writes,
    /// or a directory's `index`.
    pub(super) fn is_script_on_disk(&self, written: &[String]) -> bool {
        const EXTENSIONS: [&str; 9] =
            ["ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs", "cjs"];
        let Some(root) = &self.root else {
            return false;
        };
        let file = project_file(root, written);
        let with_extension = |base: &Path| {
            EXTENSIONS.iter().any(|extension| {
                let mut name = base.as_os_str().to_owned();
                name.push(".");
                name.push(extension);
                Path::new(&name).is_file()
            })
        };
        let javascript = file
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "js" | "jsx" | "mjs" | "cjs"));
        file.is_file()
            || with_extension(&file)
            || (javascript && with_extension(&file.with_extension("")))
            || (file.is_dir() && with_extension(&file.join("index")))
    }

    /// The directory of the package a `package:name/…` URI written in `from`
    /// names, by the rule every manifest name follows (`own_or_only`).
    pub(super) fn dart_package(&self, name: &str, from: &Node) -> Option<&[String]> {
        own_or_only(&self.dart_packages, from, |declared| declared == name)
            .map(|(directory, _)| directory.as_slice())
    }
}

/// The manifest nearest above `directory`, with the directory holding it.
pub(super) fn nearest<'m, T>(
    manifests: &'m HashMap<Vec<String>, T>,
    directory: &[String],
) -> Option<(&'m Vec<String>, &'m T)> {
    (0..=directory.len())
        .rev()
        .find_map(|depth| manifests.get_key_value(&directory[..depth]))
}

/// The one of the manifests declaring a name that an importer means: its own
/// — the nearest above it — where that one `declares` the name, otherwise the
/// only one in the project that does. Where several do and none is the
/// importer's own, which of them it depends on is configuration pack does not
/// read, so the name means none of them.
pub(super) fn own_or_only<'m, T>(
    manifests: &'m HashMap<Vec<String>, T>,
    from: &Node,
    declares: impl Fn(&T) -> bool,
) -> Option<(&'m Vec<String>, &'m T)> {
    if let Some(own) = nearest(manifests, &from.directory)
        && declares(own.1)
    {
        return Some(own);
    }
    let mut declaring = manifests.iter().filter(|(_, manifest)| declares(manifest));
    match (declaring.next(), declaring.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Each directory holding a `manifest` file on the way from a file of a
/// `language` it takes up to the project root, with the manifest's contents — every manifest
/// that governs some file of that language, wherever it sits.
fn manifests_above(
    root: &Path,
    nodes: &[Node],
    language: impl Fn(Language) -> bool,
    manifest: &str,
) -> Vec<(Vec<String>, String)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for node in nodes.iter().filter(|node| language(node.language)) {
        for depth in (0..=node.directory.len()).rev() {
            let directory = &node.directory[..depth];
            // Every directory above one already visited was visited with it.
            if !seen.insert(directory) {
                break;
            }
            let path = directory
                .iter()
                .fold(root.to_path_buf(), |path, part| path.join(part))
                .join(manifest);
            if let Ok(contents) = std::fs::read_to_string(path) {
                found.push((directory.to_vec(), contents));
            }
        }
    }
    found
}

/// The package name a `pubspec.yaml` declares in its top-level `name` key.
fn dart_package_name(pubspec: &str) -> Option<String> {
    pubspec
        .lines()
        .find_map(|line| line.strip_prefix("name:"))
        .map(|name| {
            name.split(" #")
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches(['"', '\''])
                .to_string()
        })
        .filter(|name| !name.is_empty())
}

/// What a `Cargo.toml` declares, where it declares a package: its library —
/// named by `[lib] name` or the package's name with `-` read as `_`, its root
/// file where `[lib] path` (by default `src/lib.rs`) puts it — and the root
/// file of every crate it builds: the library's; each binary, test, example
/// and bench a target table's `path` names or Cargo discovers (`src/main.rs`,
/// `src/bin/*.rs` and `src/bin/*/main.rs`; `tests/`, `examples/` and
/// `benches/` alike), unless its `autobins`-style key turns discovery off;
/// and the build script, `package.build` or `build.rs`. A crate counts only
/// where its root file is one of `files`, the package's own Rust files; a
/// workspace's own manifest declares no package.
fn cargo_package(
    directory: &[String],
    manifest: &toml::Table,
    files: &[Vec<String>],
) -> Option<RustPackage> {
    let package = manifest.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?;
    let file_of = |path: &str| {
        let mut file = directory.to_vec();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                // A root file outside the package's own tree is not followed.
                ".." => return None,
                part => file.push(part.to_string()),
            }
        }
        files.contains(&file).then_some(file)
    };
    let lib = manifest.get("lib").and_then(toml::Value::as_table);
    let setting = |key| {
        lib.and_then(|lib| lib.get(key))
            .and_then(toml::Value::as_str)
    };
    let library = file_of(setting("path").unwrap_or("src/lib.rs")).map(|root| RustLibrary {
        name: setting("name").unwrap_or(name).replace('-', "_"),
        root,
    });
    let mut crates: Vec<Vec<String>> = library.iter().map(|library| library.root.clone()).collect();
    for (table, discovery, location) in [
        ("bin", "autobins", &["src", "bin"][..]),
        ("test", "autotests", &["tests"]),
        ("example", "autoexamples", &["examples"]),
        ("bench", "autobenches", &["benches"]),
    ] {
        let targets = manifest.get(table).and_then(toml::Value::as_array);
        crates.extend(
            targets
                .into_iter()
                .flatten()
                .filter_map(|target| file_of(target.get("path")?.as_str()?)),
        );
        if package.get(discovery).and_then(toml::Value::as_bool) == Some(false) {
            continue;
        }
        if table == "bin" {
            crates.extend(file_of("src/main.rs"));
        }
        let discovered = files.iter().filter(|file| {
            let below = &file[directory.len()..];
            let inside = below.len() >= location.len()
                && below
                    .iter()
                    .zip(location)
                    .all(|(part, wanted)| part == wanted);
            let rest = inside.then(|| &below[location.len()..]);
            match rest {
                Some([file]) => file.ends_with(".rs"),
                Some([_, file]) => file == "main.rs",
                _ => false,
            }
        });
        crates.extend(discovered.cloned());
    }
    match package.get("build") {
        Some(toml::Value::String(build)) => crates.extend(file_of(build)),
        Some(toml::Value::Boolean(false)) => {}
        _ => crates.extend(file_of("build.rs")),
    }
    crates.sort();
    crates.dedup();
    Some(RustPackage { library, crates })
}

/// Every TypeScript configuration above a JavaScript or TypeScript file —
/// the `tsconfig.json` of its directory, or else its `jsconfig.json` — with
/// every project their `references` reach.
fn ts_projects(root: &Path, nodes: &[Node]) -> TsProjects {
    let mut reader = TsReader {
        root,
        parsed: HashMap::new(),
        indices: HashMap::new(),
        projects: TsProjects::default(),
    };
    for file in ["tsconfig.json", "jsconfig.json"] {
        for (directory, contents) in manifests_above(root, nodes, is_javascript, file) {
            if reader.projects.by_directory.contains_key(&directory) {
                continue;
            }
            let path = [directory.as_slice(), &[file.to_string()]].concat();
            reader.parsed.entry(path.clone()).or_insert_with(|| {
                count_parse();
                jsonc(&contents)
            });
            if let Some(project) = reader.project(&path) {
                reader.projects.by_directory.insert(directory, project);
            }
        }
    }
    reader.projects
}

/// The TypeScript configurations being read, each file parsed once and each
/// project built once, by its project path.
struct TsReader<'r> {
    root: &'r Path,
    parsed: HashMap<Vec<String>, Option<serde_json::Value>>,
    indices: HashMap<Vec<String>, Option<usize>>,
    projects: TsProjects,
}

impl TsReader<'_> {
    /// The configuration in the file at `path`, parsed the first time it is
    /// asked for; `None` where it is missing or no JSON with comments.
    fn config(&mut self, path: &[String]) -> Option<&serde_json::Value> {
        let root = self.root;
        self.parsed
            .entry(path.to_vec())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(project_file(root, path)).ok()?;
                count_parse();
                jsonc(&text)
            })
            .as_ref()
    }

    /// The index of the project the configuration in the file at `path`
    /// describes, with each project its `references` name — a
    /// configuration file, or a directory's `tsconfig.json` — read the same
    /// way, each once: a reference back to a project being read names it
    /// without reading it again. `references` is not inherited.
    fn project(&mut self, path: &[String]) -> Option<usize> {
        if let Some(known) = self.indices.get(path) {
            return *known;
        }
        let mut chain = Vec::new();
        self.config_chain(path, &mut chain);
        let Some((directory, head)) = chain.first() else {
            self.indices.insert(path.to_vec(), None);
            return None;
        };
        let referenced: Vec<Vec<String>> = head
            .get("references")
            .and_then(|references| references.as_array())
            .into_iter()
            .flatten()
            .filter_map(|reference| project_path(directory, reference.get("path")?.as_str()?))
            .map(|mut path| {
                if !project_file(self.root, &path).is_file() {
                    path.push("tsconfig.json".to_string());
                }
                path
            })
            .collect();
        let index = self.projects.projects.len();
        self.projects.projects.push(TsProject {
            config: ts_config(&chain),
            sources: ts_sources(&chain),
            references: Vec::new(),
        });
        self.indices.insert(path.to_vec(), Some(index));
        let references = referenced
            .iter()
            .filter_map(|reference| self.project(reference))
            .collect();
        self.projects.projects[index].references = references;
        Some(index)
    }

    /// The configuration in the file at `path`, then each it extends,
    /// nearest first: what an extending configuration sets overrides what
    /// it extends, and of several it extends at once the later wins. Only an
    /// `extends` written as a relative path names a file in the project; a
    /// package's configuration is not read.
    fn config_chain(&mut self, path: &[String], chain: &mut Vec<(Vec<String>, serde_json::Value)>) {
        // A configuration extending itself through others would never end.
        const DEPTH: usize = 16;
        if chain.len() >= DEPTH {
            return;
        }
        let Some(config) = self.config(path).cloned() else {
            return;
        };
        let directory = path[..path.len().saturating_sub(1)].to_vec();
        let extends: Vec<String> = match config.get("extends") {
            Some(serde_json::Value::String(extended)) => vec![extended.clone()],
            Some(serde_json::Value::Array(extended)) => extended
                .iter()
                .filter_map(|extended| extended.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        chain.push((directory.clone(), config));
        for extended in extends.iter().rev() {
            if !(extended.starts_with("./") || extended.starts_with("../")) {
                continue;
            }
            let Some(mut path) = project_path(&directory, extended) else {
                continue;
            };
            // TypeScript takes the name as written, then with `.json` added.
            if !extended.ends_with(".json")
                && !project_file(self.root, &path).is_file()
                && let Some(last) = path.last_mut()
            {
                last.push_str(".json");
            }
            self.config_chain(&path, chain);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How many TypeScript configuration files this thread has parsed, which
    /// a test of what reading `references` costs counts.
    static TS_CONFIG_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one parse of a TypeScript configuration file, in a test.
fn count_parse() {
    #[cfg(test)]
    TS_CONFIG_PARSES.with(|parses| parses.set(parses.get() + 1));
}

/// What a configuration compiles itself, from the first configuration of
/// its `extends` chain declaring each of `files`, `include` and `exclude`,
/// relative to that one's directory. Without `files` or `include` it includes
/// everything below its directory; without `exclude` it excludes
/// `node_modules`, `bower_components` and `jspm_packages`. An `include`
/// pattern whose last segment holds no `.`, `*` or `?` names a directory,
/// and everything below it. Which extensions a compiler then takes from
/// what a pattern matches (`allowJs`, a `.vue` compiler's) is not read: a
/// pattern covers every file it matches.
fn ts_sources(chain: &[(Vec<String>, serde_json::Value)]) -> TsSources {
    let head = chain.first().map(|(directory, _)| directory.as_slice());
    let head = head.unwrap_or_default();
    let declared = |key: &str| {
        chain.iter().find_map(|(directory, config)| {
            let paths = config.get(key)?.as_array()?;
            let paths: Vec<Vec<String>> = paths
                .iter()
                .filter_map(|path| project_path(directory, path.as_str()?))
                .collect();
            Some(paths)
        })
    };
    let files = declared("files");
    let include = declared("include").unwrap_or_else(|| match files {
        Some(_) => Vec::new(),
        None => vec![[head, &["**".to_string(), "*".to_string()]].concat()],
    });
    let exclude = declared("exclude").unwrap_or_else(|| {
        ["node_modules", "bower_components", "jspm_packages"]
            .map(|excluded| [head, &[excluded.to_string()]].concat())
            .to_vec()
    });
    let globs = |patterns: &mut dyn Iterator<Item = String>| {
        let mut set = globset::GlobSetBuilder::new();
        for pattern in patterns {
            if let Ok(glob) = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
            {
                set.add(glob);
            }
        }
        set.build().unwrap_or_default()
    };
    let include = globs(&mut include.iter().map(|pattern| {
        let directory = pattern
            .last()
            .is_none_or(|last| !last.contains(['.', '*', '?']));
        let mut pattern = pattern.clone();
        if directory {
            pattern.extend(["**".to_string(), "*".to_string()]);
        }
        pattern.join("/")
    }));
    let exclude = globs(&mut exclude.iter().flat_map(|pattern| {
        let pattern = pattern.join("/");
        let below = if pattern.is_empty() {
            "**".to_string()
        } else {
            format!("{pattern}/**")
        };
        [pattern, below]
    }));
    TsSources {
        files: files
            .into_iter()
            .flatten()
            .map(|file| file.iter().collect())
            .collect(),
        include,
        exclude,
    }
}

/// Every package a JavaScript workspace in the project lists: each
/// `package.json` of `manifests` with a name below a workspace root — a
/// `package.json` with `workspaces`, or a `pnpm-workspace.yaml` — whose
/// directory one of the root's globs matches and none of its `!` globs does.
fn js_packages(
    root: &Path,
    nodes: &[Node],
    manifests: &[(Vec<String>, serde_json::Value)],
) -> HashMap<Vec<String>, JsPackage> {
    let mut workspaces: Vec<(Vec<String>, Vec<String>)> = manifests
        .iter()
        .filter_map(|(directory, manifest)| {
            let globs = match manifest.get("workspaces")? {
                serde_json::Value::Object(workspaces) => workspaces.get("packages")?,
                globs => globs,
            };
            let globs = globs
                .as_array()?
                .iter()
                .filter_map(|glob| glob.as_str().map(str::to_string))
                .collect();
            Some((directory.clone(), globs))
        })
        .collect();
    workspaces.extend(
        manifests_above(root, nodes, is_javascript, "pnpm-workspace.yaml")
            .into_iter()
            .map(|(directory, yaml)| (directory, pnpm_workspace_globs(&yaml))),
    );
    let mut packages = HashMap::new();
    for (workspace, globs) in workspaces {
        let matcher = |globs: &mut dyn Iterator<Item = &str>| {
            let mut set = globset::GlobSetBuilder::new();
            for glob in globs {
                let glob = glob.trim_start_matches("./").trim_end_matches('/');
                if let Ok(glob) = globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                {
                    set.add(glob);
                }
            }
            set.build().ok()
        };
        let Some(included) = matcher(
            &mut globs
                .iter()
                .filter(|glob| !glob.starts_with('!'))
                .map(String::as_str),
        ) else {
            continue;
        };
        let excluded = matcher(&mut globs.iter().filter_map(|glob| glob.strip_prefix('!')));
        for (directory, manifest) in manifests {
            let Some(member) = directory.strip_prefix(workspace.as_slice()) else {
                continue;
            };
            let member = member.join("/");
            if member.is_empty()
                || !included.is_match(&member)
                || excluded
                    .as_ref()
                    .is_some_and(|excluded| excluded.is_match(&member))
            {
                continue;
            }
            let field = |key: &str| {
                manifest
                    .get(key)
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            };
            let Some(name) = field("name") else {
                continue;
            };
            packages.insert(
                directory.clone(),
                JsPackage {
                    name,
                    workspace: workspace.clone(),
                    exports: manifest.get("exports").cloned(),
                    types: field("types").or_else(|| field("typings")),
                    main: field("main"),
                },
            );
        }
    }
    packages
}

/// The package globs a `pnpm-workspace.yaml` lists under `packages:`, as a
/// block sequence (`- 'packages/*'`) or a flow one (`['packages/*']`).
fn pnpm_workspace_globs(yaml: &str) -> Vec<String> {
    let unquote = |item: &str| {
        let item = item.trim();
        let item = match item.find(" #") {
            Some(comment) => item[..comment].trim_end(),
            None => item,
        };
        item.trim_matches(['\'', '"']).to_string()
    };
    let mut lines = yaml.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("packages:") else {
            continue;
        };
        if let Some(flow) = rest
            .trim()
            .strip_prefix('[')
            .and_then(|flow| flow.split(']').next())
        {
            return flow
                .split(',')
                .map(unquote)
                .filter(|glob| !glob.is_empty())
                .collect();
        }
        return lines
            .take_while(|line| line.trim().is_empty() || line.starts_with([' ', '\t', '-', '#']))
            .filter_map(|line| line.trim().strip_prefix('-').map(unquote))
            .filter(|glob| !glob.is_empty())
            .collect();
    }
    Vec::new()
}

/// What the TypeScript configuration at the head of an `extends` chain
/// says, with what it extends.
fn ts_config(chain: &[(Vec<String>, serde_json::Value)]) -> TsConfig {
    let directory = chain
        .first()
        .map(|(directory, _)| directory.as_slice())
        .unwrap_or_default();
    let option = |key: &str| {
        chain.iter().find_map(|(directory, config)| {
            let value = config.get("compilerOptions")?.get(key)?;
            Some((directory, value))
        })
    };
    let base_url =
        option("baseUrl").and_then(|(directory, value)| project_path(directory, value.as_str()?));
    let declared = option("paths");
    let paths = declared
        .and_then(|(_, paths)| paths.as_object())
        .into_iter()
        .flatten()
        .map(|(pattern, substitutions)| {
            let substitutions = substitutions
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|substitution| substitution.as_str().map(str::to_string))
                .collect();
            (pattern.clone(), substitutions)
        })
        .collect();
    let paths_base = base_url
        .clone()
        .or_else(|| declared.map(|(directory, _)| directory.clone()))
        .unwrap_or_else(|| directory.to_vec());
    let setting = |key: &str| {
        option(key)
            .and_then(|(_, value)| value.as_str())
            .map(str::to_ascii_lowercase)
    };
    // An explicit `moduleResolution` anywhere in the chain overrides the one
    // a `module` implies. With neither, TypeScript 6 and 7 resolve as
    // `bundler` does (`--traceResolution`: "Module resolution kind is not
    // specified, using 'Bundler'"); a `module` that TypeScript 5 read as
    // implying `node10` or `classic` and 6 as implying `bundler` — `commonjs`,
    // the `es` versions — leaves it open.
    let module_resolution = match setting("moduleResolution").as_deref() {
        Some("bundler") => Some(ModuleResolution::Bundler),
        Some("node16" | "nodenext") => Some(ModuleResolution::Node16),
        Some("node" | "node10") => Some(ModuleResolution::Node10),
        Some("classic") => Some(ModuleResolution::Classic),
        Some(_) => None,
        None => match setting("module").as_deref() {
            None | Some("preserve") => Some(ModuleResolution::Bundler),
            Some("node16" | "node18" | "node20" | "nodenext") => Some(ModuleResolution::Node16),
            _ => None,
        },
    };
    let custom_conditions = option("customConditions")
        .and_then(|(_, conditions)| conditions.as_array())
        .into_iter()
        .flatten()
        .filter_map(|condition| condition.as_str().map(str::to_string))
        .collect();
    TsConfig {
        base_url,
        paths,
        paths_base,
        module_resolution,
        custom_conditions,
    }
}

/// The file on disk a project path names.
fn project_file(root: &Path, path: &[String]) -> PathBuf {
    path.iter()
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// A JSON-with-comments document, as `tsconfig.json` is, read as JSON: its
/// `//` and `/* */` comments, and the commas it allows before a closing
/// bracket, removed wherever they stand outside a string.
fn jsonc(text: &str) -> Option<serde_json::Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            plain.push(c);
            match c {
                '\\' => plain.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                plain.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        plain.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                plain.push(' ');
            }
            _ => plain.push(c),
        }
    }
    let mut json = String::with_capacity(plain.len());
    let mut in_string = false;
    let mut escaped = false;
    for (at, c) in plain.char_indices() {
        if in_string {
            json.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        }
        let trailing = c == ',' && plain[at + 1..].trim_start().starts_with([']', '}']);
        if !trailing {
            json.push(c);
        }
    }
    serde_json::from_str(&json).ok()
}

/// The files a Python project's build configuration, or pytest's own, is
/// written in.
const PYTHON_CONFIGURATION_FILES: [&str; 5] = [
    "pyproject.toml",
    "setup.cfg",
    "setup.py",
    "pytest.ini",
    "tox.ini",
];

/// What a Python project puts on `sys.path`.
#[derive(Debug, Clone, Default)]
pub(super) struct PythonProject {
    /// The directories its build configuration puts there.
    pub(super) roots: Vec<Vec<String>>,
    /// The projects in the tree its dependencies install (`python_dependencies`),
    /// by their directories.
    pub(super) dependencies: Vec<Vec<String>>,
}

/// What a pytest configuration puts on `sys.path`: the directories its
/// `pythonpath` names, and the basedir of each test module and
/// `conftest.py` it imports, where its import mode puts that — test modules
/// being the files its `python_files` patterns match (`test_*.py` and
/// `*_test.py` by default), by name, or by path for a pattern holding `/`.
#[derive(Debug, Clone)]
pub(super) struct Pytest {
    pub(super) pythonpath: Vec<Vec<String>>,
    test_names: globset::GlobSet,
    test_paths: globset::GlobSet,
    pub(super) basedir: Basedir,
}

/// Where pytest's `--import-mode` puts a test module's basedir on
/// `sys.path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Basedir {
    /// `prepend`, the default: at `sys.path[0]`, so a module there is found
    /// before any other directory's.
    First,
    /// `append`: at the end, after every other directory.
    Last,
    /// `importlib`: nowhere.
    Nowhere,
}

impl Pytest {
    fn new(pythonpath: Vec<Vec<String>>, python_files: &[&str], basedir: Basedir) -> Self {
        let mut names = globset::GlobSetBuilder::new();
        let mut paths = globset::GlobSetBuilder::new();
        for pattern in python_files {
            // pytest matches a pattern holding a separator against the whole
            // path, which a relative pattern can match the end of.
            let (set, pattern) = if pattern.contains('/') {
                (&mut paths, format!("*/{pattern}"))
            } else {
                (&mut names, pattern.to_string())
            };
            if let Ok(glob) = globset::Glob::new(&pattern) {
                set.add(glob);
            }
        }
        Self {
            pythonpath,
            test_names: names.build().unwrap_or_default(),
            test_paths: paths.build().unwrap_or_default(),
            basedir,
        }
    }

    /// Whether pytest imports the project file at `path` as a test module
    /// or a `conftest.py`.
    pub(super) fn imports_as_test(&self, path: &Path) -> bool {
        let name = path.file_name().unwrap_or_default();
        name == "conftest.py"
            || self.test_names.is_match(name)
            || self.test_paths.is_match(Path::new("/").join(path))
    }
}

impl Default for Pytest {
    fn default() -> Self {
        Self::new(Vec::new(), &["test_*.py", "*_test.py"], Basedir::First)
    }
}

/// The directories of a tree's Python packages: those holding an
/// `__init__.py`.
pub(super) fn python_packages(nodes: &[Node]) -> HashSet<&[String]> {
    nodes
        .iter()
        .filter(|node| {
            node.language == Language::Python
                && Path::new(&node.rel_path)
                    .file_stem()
                    .is_some_and(|stem| stem == "__init__")
        })
        .map(|node| node.directory.as_slice())
        .collect()
}

/// The Python configuration files one directory holds, read.
pub(super) struct PythonConfiguration<'f> {
    pyproject: Option<toml::Table>,
    /// Whether it holds a `pyproject.toml`, read or not.
    has_pyproject: bool,
    setup_cfg: Option<HashMap<String, HashMap<String, String>>>,
    has_setup_py: bool,
    pytest_ini: Option<&'f str>,
    tox_ini: Option<&'f str>,
}

impl<'f> PythonConfiguration<'f> {
    fn read(files: &'f [(&str, String)]) -> Self {
        let file = |name: &str| {
            files
                .iter()
                .find(|(file, _)| *file == name)
                .map(|(_, contents)| contents.as_str())
        };
        Self {
            pyproject: file("pyproject.toml").and_then(|contents| toml::from_str(contents).ok()),
            has_pyproject: file("pyproject.toml").is_some(),
            setup_cfg: file("setup.cfg").map(ini_sections),
            has_setup_py: file("setup.py").is_some(),
            pytest_ini: file("pytest.ini"),
            tox_ini: file("tox.ini"),
        }
    }

    /// Whether the directory's files configure a build, which is what makes
    /// it a project's: a `setup.py`, or a `pyproject.toml` or `setup.cfg`
    /// holding more than pytest's own table. `pytest.ini` and `tox.ini`
    /// configure tools, never a build.
    fn configures_a_build(&self) -> bool {
        let pytest_only_toml = self.pyproject.as_ref().is_some_and(|pyproject| {
            pyproject.keys().all(|key| key == "tool")
                && toml_at(pyproject, &["tool"])
                    .is_some_and(|tool| !tool.is_empty() && tool.keys().all(|key| key == "pytest"))
        });
        let pytest_only_cfg = self.setup_cfg.as_ref().is_some_and(|sections| {
            !sections.is_empty() && sections.keys().all(|section| section == "tool:pytest")
        });
        self.has_setup_py
            || (self.has_pyproject && !pytest_only_toml)
            || (self.setup_cfg.is_some() && !pytest_only_cfg)
    }
}

/// What the Python files of a project tree are configured by: every
/// project above a Python file, with what it puts on `sys.path`, and every
/// pytest configuration there, each by its directory. A directory is a
/// project's where its files configure a build
/// (`PythonConfiguration::configures_a_build`) and it is no package — one
/// holding `__init__.py` is part of the project above it, its `setup.py` a
/// module like any other.
fn python_configurations(
    root: &Path,
    nodes: &[Node],
) -> (
    HashMap<Vec<String>, PythonProject>,
    HashMap<Vec<String>, Pytest>,
) {
    let packages = python_packages(nodes);
    let mut configurations: BTreeMap<Vec<String>, Vec<(&str, String)>> = BTreeMap::new();
    for file in PYTHON_CONFIGURATION_FILES {
        for (directory, contents) in
            manifests_above(root, nodes, |language| language == Language::Python, file)
        {
            configurations
                .entry(directory)
                .or_default()
                .push((file, contents));
        }
    }
    let mut pytest = HashMap::new();
    let mut pyprojects: HashMap<&[String], toml::Table> = HashMap::new();
    let mut projects = Vec::new();
    for (directory, files) in &configurations {
        let mut configuration = PythonConfiguration::read(files);
        if let Some(options) = configuration.pytest(directory) {
            pytest.insert(directory.clone(), options);
        }
        if packages.contains(directory.as_slice()) || !configuration.configures_a_build() {
            continue;
        }
        projects.push((directory, python_project(directory, Some(&configuration))));
        if let Some(pyproject) = configuration.pyproject.take() {
            pyprojects.insert(directory.as_slice(), pyproject);
        }
    }
    let projects = projects
        .into_iter()
        .map(|(directory, mut project)| {
            project.dependencies = python_dependencies(directory, &pyprojects);
            (directory.clone(), project)
        })
        .collect();
    (projects, pytest)
}

/// The projects in the tree that the project at `directory` declares a
/// dependency on its tooling installs from there, by their directories: a
/// `[project]` dependency whose `[tool.uv.sources]` entry — the project's
/// own, else its uv workspace root's — is `{ workspace = true }`, naming the
/// workspace member of that name, or `{ path = "…" }`; and a Poetry
/// dependency written `{ path = "…" }`. Names compare as normalized
/// distribution names. Installing a project installs these, so their
/// packages are on its `sys.path` as well — those of a directory whichever
/// build configuration makes a project (`python_configurations`), and none
/// of one that is no project's.
fn python_dependencies(
    directory: &[String],
    pyprojects: &HashMap<&[String], toml::Table>,
) -> Vec<Vec<String>> {
    let Some(pyproject) = pyprojects.get(directory) else {
        return Vec::new();
    };
    let workspace = (0..=directory.len())
        .rev()
        .map(|depth| &directory[..depth])
        .find_map(|root| {
            let workspace = toml_at(pyprojects.get(root)?, &["tool", "uv", "workspace"])?;
            uv_member(root, workspace, directory).then_some(root)
        });
    let sources = |at: &[String]| {
        pyprojects
            .get(at)
            .and_then(|pyproject| toml_at(pyproject, &["tool", "uv", "sources"]))
            .into_iter()
            .flatten()
            .map(|(name, source)| (distribution_name(name), (at.to_vec(), source)))
            .collect::<HashMap<_, _>>()
    };
    let mut declared = workspace.map(sources).unwrap_or_default();
    declared.extend(sources(directory));
    let named = toml_value(pyproject, &["project", "dependencies"])
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(|requirement| {
            let end = requirement
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
                .unwrap_or(requirement.len());
            distribution_name(&requirement[..end])
        });
    let mut dependencies: Vec<Vec<String>> = named
        .filter_map(|name| {
            let (at, source) = declared.get(&name)?;
            if let Some(path) = source.get("path").and_then(toml::Value::as_str) {
                return project_path(at, path);
            }
            if source.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
                return None;
            }
            let root = workspace?;
            let workspace = toml_at(pyprojects.get(root)?, &["tool", "uv", "workspace"])?;
            let mut members = pyprojects.iter().filter(|(member, pyproject)| {
                uv_member(root, workspace, member)
                    && toml_value(pyproject, &["project", "name"])
                        .and_then(toml::Value::as_str)
                        .is_some_and(|declared| distribution_name(declared) == name)
            });
            match (members.next(), members.next()) {
                (Some((member, _)), None) => Some(member.to_vec()),
                _ => None,
            }
        })
        .collect();
    let poetry = toml_at(pyproject, &["tool", "poetry", "dependencies"]);
    dependencies.extend(
        poetry.into_iter().flatten().filter_map(|(_, dependency)| {
            project_path(directory, dependency.get("path")?.as_str()?)
        }),
    );
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

/// Whether the directory `member` is a member of the uv workspace rooted at
/// `root`: the root itself, or a directory below it one of its `members`
/// globs matches and none of its `exclude` globs does.
fn uv_member(root: &[String], workspace: &toml::Table, member: &[String]) -> bool {
    let Some(below) = member.strip_prefix(root) else {
        return false;
    };
    if below.is_empty() {
        return true;
    }
    let below = below.join("/");
    let matches = |key: &str| {
        workspace
            .get(key)
            .map(toml_strings)
            .unwrap_or_default()
            .iter()
            .filter_map(|glob| {
                globset::GlobBuilder::new(glob.trim_start_matches("./").trim_end_matches('/'))
                    .literal_separator(true)
                    .build()
                    .ok()
            })
            .any(|glob| glob.compile_matcher().is_match(&below))
    };
    matches("members") && !matches("exclude")
}

/// A distribution name as Python packaging compares them: in lower case,
/// each run of `-`, `_` and `.` read as one `-`.
fn distribution_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.push(c.to_ascii_lowercase());
        }
    }
    normalized
}

/// What a Python project at `directory` puts on `sys.path`, from its
/// `configuration`: the package locations its build configuration declares
/// — setuptools' `package-dir` and `packages.find.where` (in
/// `pyproject.toml` or `setup.cfg`), Poetry's `packages`, Hatch's `packages`
/// and `sources`, PDM's `package-dir` — or, where it declares none,
/// setuptools' automatic discovery: the directory itself (a flat layout) and
/// its `src/` (a src layout). A project without a configuration (the
/// project root where none is there) is read with none.
pub(super) fn python_project(
    directory: &[String],
    configuration: Option<&PythonConfiguration>,
) -> PythonProject {
    let pyproject = configuration.and_then(|configuration| configuration.pyproject.as_ref());
    let setup_cfg = configuration.and_then(|configuration| configuration.setup_cfg.as_ref());
    let mut declared: Vec<String> = Vec::new();
    if let Some(pyproject) = pyproject {
        declared.extend(pyproject_package_roots(pyproject));
    }
    if let Some(package_dir) = setup_cfg
        .and_then(|cfg| cfg.get("options"))
        .and_then(|options| options.get("package_dir"))
    {
        declared.extend(package_dir.lines().filter_map(|line| {
            let (package, location) = line.split_once('=')?;
            root_of_package(package.trim(), location.trim())
        }));
    }
    if let Some(find) = setup_cfg
        .and_then(|cfg| cfg.get("options.packages.find"))
        .and_then(|find| find.get("where"))
    {
        declared.extend(find.split_whitespace().map(str::to_string));
    }
    if declared.is_empty() {
        declared = vec![".".to_string(), "src".to_string()];
    }
    let mut roots: Vec<Vec<String>> = declared
        .iter()
        .filter_map(|path| project_path(directory, path))
        .collect();
    roots.sort();
    roots.dedup();
    PythonProject {
        roots,
        dependencies: Vec::new(),
    }
}

impl PythonConfiguration<'_> {
    /// How pytest runs from the directory at `directory`, where its files
    /// configure it, read from the one configuration file pytest itself
    /// would read there (`pytest_options`).
    fn pytest(&self, directory: &[String]) -> Option<Pytest> {
        let options = pytest_options(
            self.pytest_ini,
            self.pyproject.as_ref(),
            self.tox_ini,
            self.setup_cfg.as_ref(),
        )?;
        let option = |key: &str| options.get(key).map(Vec::as_slice).unwrap_or_default();
        let pythonpath = option("pythonpath")
            .iter()
            .filter_map(|path| project_path(directory, path))
            .collect();
        let python_files: Vec<&str> = option("python_files").iter().map(String::as_str).collect();
        let mut mode = None;
        let mut addopts = option("addopts").iter();
        while let Some(argument) = addopts.next() {
            if argument == "--import-mode" {
                mode = addopts.next().map(String::as_str);
            } else if let Some(value) = argument.strip_prefix("--import-mode=") {
                mode = Some(value);
            }
        }
        let basedir = match mode {
            Some("importlib") => Basedir::Nowhere,
            Some("append") => Basedir::Last,
            _ => Basedir::First,
        };
        Some(if python_files.is_empty() {
            Pytest {
                pythonpath,
                basedir,
                ..Pytest::default()
            }
        } else {
            Pytest::new(pythonpath, &python_files, basedir)
        })
    }
}

/// The options of the one configuration file pytest reads in a directory,
/// where one there configures it — `pytest.ini` whatever it holds, else
/// `pyproject.toml`'s `[tool.pytest.ini_options]`, `tox.ini`'s `[pytest]`,
/// `setup.cfg`'s `[tool:pytest]` — each as the whitespace-separated items
/// its value lists.
fn pytest_options(
    pytest_ini: Option<&str>,
    pyproject: Option<&toml::Table>,
    tox_ini: Option<&str>,
    setup_cfg: Option<&HashMap<String, HashMap<String, String>>>,
) -> Option<HashMap<String, Vec<String>>> {
    let ini = |section: Option<&HashMap<String, String>>| {
        section
            .into_iter()
            .flatten()
            .map(|(key, value)| (key.clone(), ini_list(Some(value))))
            .collect()
    };
    if let Some(pytest_ini) = pytest_ini {
        return Some(ini(ini_sections(pytest_ini).get("pytest")));
    }
    if let Some(options) =
        pyproject.and_then(|pyproject| toml_at(pyproject, &["tool", "pytest", "ini_options"]))
    {
        return Some(
            options
                .iter()
                .map(|(key, value)| {
                    let items = toml_strings(value)
                        .iter()
                        .flat_map(|item| item.split_whitespace().map(str::to_string))
                        .collect();
                    (key.clone(), items)
                })
                .collect(),
        );
    }
    if let Some(tox_ini) = tox_ini.map(ini_sections)
        && let Some(pytest) = tox_ini.get("pytest")
    {
        return Some(ini(Some(pytest)));
    }
    setup_cfg
        .and_then(|cfg| cfg.get("tool:pytest"))
        .map(|pytest| ini(Some(pytest)))
}

/// The package locations a `pyproject.toml` declares, as paths relative to
/// its directory.
fn pyproject_package_roots(pyproject: &toml::Table) -> Vec<String> {
    let mut roots = Vec::new();
    let setuptools = toml_at(pyproject, &["tool", "setuptools"]);
    let package_dir = setuptools
        .and_then(|setuptools| setuptools.get("package-dir"))
        .and_then(toml::Value::as_table);
    for (package, location) in package_dir.into_iter().flatten() {
        roots.extend(
            location
                .as_str()
                .and_then(|location| root_of_package(package, location)),
        );
    }
    if let Some(find) = setuptools.and_then(|setuptools| toml_at(setuptools, &["packages", "find"]))
    {
        match find.get("where") {
            Some(places) => roots.extend(toml_strings(places)),
            None => roots.push(".".to_string()),
        }
    } else if setuptools.is_some_and(|setuptools| setuptools.contains_key("packages"))
        && roots.is_empty()
    {
        // An explicit package list without a `package-dir` sits at the root.
        roots.push(".".to_string());
    }
    let poetry =
        toml_value(pyproject, &["tool", "poetry", "packages"]).and_then(toml::Value::as_array);
    roots.extend(poetry.into_iter().flatten().map(|package| {
        package
            .get("from")
            .and_then(toml::Value::as_str)
            .unwrap_or(".")
            .to_string()
    }));
    for hatch in [
        &["tool", "hatch", "build"][..],
        &["tool", "hatch", "build", "targets", "wheel"],
    ] {
        let Some(build) = toml_at(pyproject, hatch) else {
            continue;
        };
        if let Some(packages) = build.get("packages") {
            roots.extend(toml_strings(packages).iter().map(|package| {
                match package.trim_end_matches('/').rsplit_once('/') {
                    Some((parent, _)) => parent.to_string(),
                    None => ".".to_string(),
                }
            }));
        }
        match build.get("sources") {
            Some(toml::Value::Table(rewrites)) => roots.extend(
                rewrites
                    .iter()
                    .filter(|(_, to)| to.as_str() == Some(""))
                    .map(|(from, _)| from.clone()),
            ),
            Some(sources) => roots.extend(toml_strings(sources)),
            None => {}
        }
    }
    roots.extend(
        toml_value(pyproject, &["tool", "pdm", "build", "package-dir"])
            .and_then(toml::Value::as_str)
            .map(str::to_string),
    );
    roots
}

/// The directory a package mapping (`package = location`) puts on
/// `sys.path`: the location itself for the root mapping (`"" = "src"`), and
/// the one above a top-level package that keeps its own name there. A
/// mapping that renames a package is not one a directory on `sys.path`
/// expresses.
fn root_of_package(package: &str, location: &str) -> Option<String> {
    if package.is_empty() {
        return Some(location.to_string());
    }
    let location = location.trim_end_matches('/');
    let (parent, name) = location.rsplit_once('/').unwrap_or((".", location));
    (!package.contains('.') && name == package).then(|| parent.to_string())
}

/// A configuration's path, relative to the project directory, as a project
/// path; none for an absolute one or one that climbs out of the project.
pub(super) fn project_path(directory: &[String], path: &str) -> Option<Vec<String>> {
    if path.starts_with('/') {
        return None;
    }
    let mut resolved = directory.to_vec();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                resolved.pop()?;
            }
            part => resolved.push(part.to_string()),
        }
    }
    Some(resolved)
}

/// The table a TOML table holds at a dotted path of keys.
fn toml_at<'t>(table: &'t toml::Table, keys: &[&str]) -> Option<&'t toml::Table> {
    keys.iter().try_fold(table, |table, key| {
        table.get(*key).and_then(toml::Value::as_table)
    })
}

/// The value a TOML table holds at a dotted path of keys.
fn toml_value<'t>(table: &'t toml::Table, keys: &[&str]) -> Option<&'t toml::Value> {
    let (last, path) = keys.split_last()?;
    toml_at(table, path)?.get(*last)
}

/// The strings a TOML value holds: itself where it is one, its string items
/// where it is an array.
fn toml_strings(value: &toml::Value) -> Vec<String> {
    match value {
        toml::Value::String(value) => vec![value.clone()],
        toml::Value::Array(values) => values
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// The whitespace-separated items of an INI value.
fn ini_list(value: Option<&String>) -> Vec<String> {
    value
        .map(|value| value.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// An INI file's sections as Python's configparser reads them: `[section]`
/// headers, `key = value` or `key: value` entries (keys in lower case) whose
/// indented lines continue the value, and `#` or `;` comment lines.
fn ini_sections(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section: Option<String> = None;
    let mut key: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if let (Some(section), Some(key)) = (&section, &key)
                && let Some(value) = sections
                    .get_mut(section)
                    .and_then(|entries| entries.get_mut(key))
            {
                value.push('\n');
                value.push_str(trimmed);
            }
            continue;
        }
        if let Some(name) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = Some(name.trim().to_string());
            sections.entry(name.trim().to_string()).or_default();
            key = None;
            continue;
        }
        key = None;
        let (Some(current), Some(split)) = (&section, trimmed.find(['=', ':'])) else {
            continue;
        };
        let name = trimmed[..split].trim().to_lowercase();
        let value = trimmed[split + 1..].trim().to_string();
        sections
            .entry(current.clone())
            .or_default()
            .insert(name.clone(), value);
        key = Some(name);
    }
    sections
}

/// The module path a `go.mod` declares, read by that file's lexical rules:
/// tokens separated by whitespace, `//` comments to the end of a line, a path
/// that may be written as an interpreted or a raw string, and the directive
/// on one line or as a `module ( … )` block. A path whose string carries an
/// escape is not decoded here, so it declares nothing.
fn go_module_path(go_mod: &str) -> Option<Vec<String>> {
    let mut lines = go_mod.lines().map(go_mod_tokens);
    while let Some(tokens) = lines.next() {
        let path = match tokens.as_slice() {
            [Some(verb), Some(open)] if verb == "module" && open == "(" => lines
                .find(|line| !line.is_empty())
                .and_then(|line| match line.as_slice() {
                    [path] => path.clone(),
                    _ => None,
                }),
            [Some(verb), path] if verb == "module" => path.clone(),
            _ => continue,
        };
        return path
            .map(|path| split_components(&path))
            .filter(|path| !path.is_empty());
    }
    None
}

/// The tokens of one `go.mod` line, `None` standing for a string whose value
/// is not decoded here.
fn go_mod_tokens(line: &str) -> Vec<Option<String>> {
    const PUNCTUATION: [char; 7] = ['(', ')', '[', ']', '{', '}', ','];
    let mut tokens = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() && !rest.starts_with("//") {
        let first = rest.chars().next().expect("a non-empty line");
        if PUNCTUATION.contains(&first) {
            tokens.push(Some(first.to_string()));
            rest = &rest[1..];
        } else if first == '"' || first == '`' {
            let Some(end) = rest[1..].find(first) else {
                tokens.push(None);
                break;
            };
            let value = &rest[1..1 + end];
            tokens.push((first == '`' || !value.contains('\\')).then(|| value.to_string()));
            rest = &rest[end + 2..];
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || PUNCTUATION.contains(&c))
                .unwrap_or(rest.len());
            let end = rest[..end].find("//").unwrap_or(end);
            tokens.push(Some(rest[..end].to_string()));
            rest = &rest[end..];
        }
        rest = rest.trim_start();
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `pubspec.yaml` names its package in its top-level `name` key, not in
    /// an indented one.
    #[test]
    fn a_pubspec_names_its_package_in_its_top_level_name_key() {
        assert_eq!(
            dart_package_name("  name: nested\nname: \"quoted\"\n"),
            Some("quoted".into())
        );
    }

    /// A `tsconfig.json` is JSON with comments and trailing commas, and a
    /// comment marker inside a string is part of the string.
    #[test]
    fn a_typescript_configuration_is_read_as_json_with_comments() {
        assert_eq!(
            jsonc("{\"a\": \"// not a comment\", \"b\": [1, 2,], /* c */}"),
            Some(serde_json::json!({"a": "// not a comment", "b": [1, 2]}))
        );
    }

    /// `pnpm-workspace.yaml` lists its packages as a flow or a block
    /// sequence, and the list ends where the next top-level key begins.
    #[test]
    fn a_pnpm_workspace_lists_its_packages_in_either_sequence_form() {
        assert_eq!(
            pnpm_workspace_globs("packages: ['a/*', \"b/**\"]\n"),
            vec!["a/*", "b/**"]
        );
        assert_eq!(
            pnpm_workspace_globs("packages:\n  - a/*\n\n  - \"b\"\ncatalog:\n  - c\n"),
            vec!["a/*", "b"]
        );
    }

    /// A TypeScript configuration is parsed once however many `references`
    /// reach it: here each of fourteen packages references the three before
    /// it, the first references the last, and a solution references them
    /// all — paths whose number grows exponentially with the chain — yet
    /// the reading parses fifteen files, and resolving a file's owner walks
    /// the cycle to an end.
    #[test]
    fn a_typescript_configuration_is_parsed_once_whatever_references_reach_it() {
        const PACKAGES: usize = 14;
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |path: &str, contents: &str| {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        };
        let reference = |package: usize| format!("{{\"path\": \"../p{package}\"}}");
        let mut nodes = Vec::new();
        for package in 0..PACKAGES {
            let earlier: Vec<String> = match package {
                0 => vec![reference(PACKAGES - 1)],
                _ => (package.saturating_sub(3)..package)
                    .map(reference)
                    .collect(),
            };
            write(
                &format!("packages/p{package}/tsconfig.json"),
                &format!(
                    "{{\"compilerOptions\": {{\"paths\": {{\"#p{package}\": [\"./src/index.ts\"]}}}}, \
                     \"include\": [\"src\"], \"references\": [{}]}}",
                    earlier.join(", ")
                ),
            );
            nodes.push(crate::services::pack::tests::node_in(
                nodes.len(),
                &format!("packages/p{package}/src/index.ts"),
                "",
                Language::TypeScript,
            ));
        }
        let all: Vec<String> = (0..PACKAGES)
            .map(|package| format!("{{\"path\": \"./packages/p{package}\"}}"))
            .collect();
        write(
            "tsconfig.json",
            &format!("{{\"files\": [], \"references\": [{}]}}", all.join(", ")),
        );
        nodes.push(crate::services::pack::tests::node_in(
            nodes.len(),
            "packages/p5/extra/x.ts",
            "",
            Language::TypeScript,
        ));

        TS_CONFIG_PARSES.with(|parses| parses.set(0));
        let manifests = Manifests::read(dir.path(), &nodes);
        assert_eq!(TS_CONFIG_PARSES.with(std::cell::Cell::get), PACKAGES + 1);
        let last = &nodes[PACKAGES - 1];
        assert!(matches!(
            manifests.ts_owner(last),
            TsOwner::Config(config) if config.paths[0].0 == format!("#p{}", PACKAGES - 1)
        ));
        // No project the references reach compiles the stray file, and each
        // of those they reach could have been meant.
        assert!(matches!(
            manifests.ts_owner(&nodes[PACKAGES]),
            TsOwner::Uncompiled(candidates) if candidates.len() == PACKAGES
        ));
    }

    /// `go.mod` is read by its own lexical rules, not as a line beginning
    /// `module `: the path may be quoted or followed by a comment, the
    /// directive may be a block, and a commented-out directive is no
    /// directive.
    #[test]
    fn a_go_mod_module_directive_is_read_as_go_mod_spells_it() {
        for go_mod in [
            "module github.com/acme/widget\n\ngo 1.24\n",
            "module github.com/acme/widget // the widget\n",
            "module\tgithub.com/acme/widget\n",
            "module \"github.com/acme/widget\"\n",
            "module `github.com/acme/widget`\n",
            "// module example.com/fake\nmodule (\n\t// the widget\n\tgithub.com/acme/widget\n)\n",
            "  module github.com/acme/widget//vanity\r\n",
        ] {
            assert_eq!(
                go_module_path(go_mod),
                Some(split_components("github.com/acme/widget")),
                "{go_mod:?}"
            );
        }
        for go_mod in [
            "module \"github.com/acme/wi\\x64get\"\n",
            "// module example.com/fake\n",
            "module\n",
            "modules example.com/fake\n",
        ] {
            assert_eq!(go_module_path(go_mod), None, "{go_mod:?}");
        }
    }
}
