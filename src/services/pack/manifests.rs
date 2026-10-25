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
    /// configuration files that make it one.
    pub(super) python_projects: HashMap<Vec<String>, PythonProject>,
    /// The TypeScript project each `tsconfig.json`, or a `jsconfig.json`
    /// where a directory holds none, describes, by its directory.
    pub(super) ts_configs: HashMap<Vec<String>, TsProject>,
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

/// A TypeScript configuration as the project `tsc -b` builds from it: what
/// it says about its files' specifiers, the files it compiles itself, and
/// the projects its `references` name.
#[derive(Debug, Clone, Default)]
pub(super) struct TsProject {
    pub(super) config: TsConfig,
    sources: TsSources,
    references: Vec<TsProject>,
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
    /// The configuration above it has `references` naming no one project
    /// that compiles it.
    Undetermined,
}

impl TsProject {
    /// The configurations `file` is compiled under in this project: its own
    /// where it compiles the file itself — or has no `references`, a
    /// configuration's files being whatever is below it — else those of the
    /// projects it references that do, as `tsc -b` builds them.
    fn owners<'a>(&'a self, file: &Path, owners: &mut Vec<&'a TsConfig>) {
        if self.references.is_empty() || self.sources.covers(file) {
            owners.push(&self.config);
            return;
        }
        for reference in &self.references {
            reference.compiling(file, owners);
        }
    }

    /// The configurations of this referenced project, and those it
    /// references in turn, that compile `file` themselves.
    fn compiling<'a>(&'a self, file: &Path, owners: &mut Vec<&'a TsConfig>) {
        if self.sources.covers(file) {
            owners.push(&self.config);
            return;
        }
        for reference in &self.references {
            reference.compiling(file, owners);
        }
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
    /// implies one, selects, where that is one deciding which of the
    /// `import`, `require` and `node` conditions TypeScript matches.
    pub(super) module_resolution: Option<ModuleResolution>,
    /// The conditions `customConditions` adds.
    pub(super) custom_conditions: Vec<String>,
}

/// A module resolution strategy that says which `exports` conditions apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModuleResolution {
    /// `bundler`: an import matches `import`, never `require` or `node`.
    Bundler,
    /// `node16` or `nodenext`: an import matches `node`, and `import` or
    /// `require` by the module format of the file writing it.
    Node16,
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
        Self {
            root: Some(root.to_path_buf()),
            go_modules,
            dart_packages,
            rust_packages,
            python_projects: python_projects(root, nodes),
            ts_configs: ts_configs(root, nodes),
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
    /// it agree on (`TsProject::owners`). Projects that say different things
    /// about specifiers, or none compiling it, leave it undetermined.
    pub(super) fn ts_owner(&self, from: &Node) -> TsOwner<'_> {
        let Some((_, project)) = nearest(&self.ts_configs, &from.directory) else {
            return TsOwner::None;
        };
        let mut owners = Vec::new();
        project.owners(Path::new(&from.rel_path), &mut owners);
        match owners.split_first() {
            Some((first, rest)) if rest.iter().all(|other| other == first) => {
                TsOwner::Config(first)
            }
            _ => TsOwner::Undetermined,
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

/// Every TypeScript configuration above a JavaScript or TypeScript file, by
/// its directory: the `tsconfig.json` there, or else its `jsconfig.json`.
fn ts_configs(root: &Path, nodes: &[Node]) -> HashMap<Vec<String>, TsProject> {
    let mut configs = HashMap::new();
    for file in ["tsconfig.json", "jsconfig.json"] {
        for (directory, contents) in manifests_above(root, nodes, is_javascript, file) {
            if configs.contains_key(&directory) {
                continue;
            }
            let path = [directory.as_slice(), &[file.to_string()]].concat();
            if let Some(project) = ts_project(root, &path, Some(&contents), 0) {
                configs.insert(directory, project);
            }
        }
    }
    configs
}

/// The project the TypeScript configuration in `file` describes, with each
/// project its `references` name — a configuration file, or a directory's
/// `tsconfig.json` — read the same way. `references` is not inherited.
fn ts_project(
    root: &Path,
    file: &[String],
    contents: Option<&str>,
    depth: usize,
) -> Option<TsProject> {
    // A project referencing itself through others would never end.
    const DEPTH: usize = 16;
    let mut chain = Vec::new();
    ts_config_chain(root, file, contents, &mut chain);
    let (directory, head) = chain.first()?;
    let references = head
        .get("references")
        .and_then(|references| references.as_array())
        .into_iter()
        .flatten()
        .filter(|_| depth < DEPTH)
        .filter_map(|reference| project_path(directory, reference.get("path")?.as_str()?))
        .filter_map(|mut path| {
            if !project_file(root, &path).is_file() {
                path.push("tsconfig.json".to_string());
            }
            ts_project(root, &path, None, depth + 1)
        })
        .collect();
    Some(TsProject {
        config: ts_config(&chain),
        sources: ts_sources(&chain),
        references,
    })
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
    // a `module` implies.
    let module_resolution = match setting("moduleResolution").as_deref() {
        Some("bundler") => Some(ModuleResolution::Bundler),
        Some("node16" | "nodenext") => Some(ModuleResolution::Node16),
        Some(_) => None,
        None => match setting("module").as_deref() {
            Some("preserve") => Some(ModuleResolution::Bundler),
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

/// The configuration in `file`, then each it extends, nearest first: what an
/// extending configuration sets overrides what it extends, and of several it
/// extends at once the later wins. Only an `extends` written as a relative
/// path names a file in the project; a package's configuration is not read.
fn ts_config_chain(
    root: &Path,
    file: &[String],
    contents: Option<&str>,
    chain: &mut Vec<(Vec<String>, serde_json::Value)>,
) {
    // A configuration extending itself through others would never end.
    const DEPTH: usize = 16;
    if chain.len() >= DEPTH {
        return;
    }
    let text = match contents {
        Some(contents) => contents.to_string(),
        None => match std::fs::read_to_string(project_file(root, file)) {
            Ok(text) => text,
            Err(_) => return,
        },
    };
    let Some(config) = jsonc(&text) else {
        return;
    };
    let directory = file[..file.len().saturating_sub(1)].to_vec();
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
            && !project_file(root, &path).is_file()
            && let Some(last) = path.last_mut()
        {
            last.push_str(".json");
        }
        ts_config_chain(root, &path, None, chain);
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

/// The files whose presence makes a directory a Python project's: its build
/// configuration, and the files pytest takes its own from.
const PYTHON_PROJECT_FILES: [&str; 5] = [
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
    /// How pytest runs, where a file of the project configures it.
    pub(super) pytest: Option<Pytest>,
}

/// What a pytest configuration puts on `sys.path`: the directories its
/// `pythonpath` names, and, unless its import mode is `importlib`, the
/// basedir of each test module and `conftest.py` it imports — test modules
/// being the files its `python_files` patterns match (`test_*.py` and
/// `*_test.py` by default), by name, or by path for a pattern holding `/`.
#[derive(Debug, Clone)]
pub(super) struct Pytest {
    pub(super) pythonpath: Vec<Vec<String>>,
    test_names: globset::GlobSet,
    test_paths: globset::GlobSet,
    pub(super) inserts_basedir: bool,
}

impl Pytest {
    fn new(pythonpath: Vec<Vec<String>>, python_files: &[&str], inserts_basedir: bool) -> Self {
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
            inserts_basedir,
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
        Self::new(Vec::new(), &["test_*.py", "*_test.py"], true)
    }
}

/// Every Python project above a Python file, with what it puts on
/// `sys.path`.
fn python_projects(root: &Path, nodes: &[Node]) -> HashMap<Vec<String>, PythonProject> {
    let mut configurations: BTreeMap<Vec<String>, Vec<(&str, String)>> = BTreeMap::new();
    for file in PYTHON_PROJECT_FILES {
        for (directory, contents) in
            manifests_above(root, nodes, |language| language == Language::Python, file)
        {
            configurations
                .entry(directory)
                .or_default()
                .push((file, contents));
        }
    }
    configurations
        .into_iter()
        .map(|(directory, files)| {
            let project = python_project(&directory, &files);
            (directory, project)
        })
        .collect()
}

/// What a Python project at `directory` puts on `sys.path`, from its
/// configuration `files`: the package locations its build configuration
/// declares — setuptools' `package-dir` and `packages.find.where` (in
/// `pyproject.toml` or `setup.cfg`), Poetry's `packages`, Hatch's `packages`
/// and `sources`, PDM's `package-dir` — or, where it declares none,
/// setuptools' automatic discovery: the directory itself (a flat layout) and
/// its `src/` (a src layout); and what pytest does, read from the one
/// configuration file pytest itself would read (`pytest_options`).
pub(super) fn python_project(directory: &[String], files: &[(&str, String)]) -> PythonProject {
    let file = |name: &str| {
        files
            .iter()
            .find(|(file, _)| *file == name)
            .map(|(_, contents)| contents.as_str())
    };
    let pyproject: Option<toml::Table> =
        file("pyproject.toml").and_then(|contents| toml::from_str(contents).ok());
    let setup_cfg = file("setup.cfg").map(ini_sections);
    let mut declared: Vec<String> = Vec::new();
    if let Some(pyproject) = &pyproject {
        declared.extend(pyproject_package_roots(pyproject));
    }
    if let Some(package_dir) = setup_cfg
        .as_ref()
        .and_then(|cfg| cfg.get("options"))
        .and_then(|options| options.get("package_dir"))
    {
        declared.extend(package_dir.lines().filter_map(|line| {
            let (package, location) = line.split_once('=')?;
            root_of_package(package.trim(), location.trim())
        }));
    }
    if let Some(find) = setup_cfg
        .as_ref()
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
    let pytest = pytest_options(
        file("pytest.ini"),
        pyproject.as_ref(),
        file("tox.ini"),
        setup_cfg.as_ref(),
    )
    .map(|options| {
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
        let inserts_basedir = mode != Some("importlib");
        if python_files.is_empty() {
            Pytest {
                pythonpath,
                inserts_basedir,
                ..Pytest::default()
            }
        } else {
            Pytest::new(pythonpath, &python_files, inserts_basedir)
        }
    });
    PythonProject { roots, pytest }
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
