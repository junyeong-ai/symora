//! The import graph pack ranks: each file's references, resolved to the
//! project files they name by each language's own rules.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::Node;
use super::manifests::{
    JsPackage, Manifests, TsConfig, nearest, own_or_only, python_project_roots, script_path,
};
use crate::models::symbol::Language;

// BTreeMap, not HashMap: PageRank accumulates f64 scores by iterating this
// graph, and float addition is non-associative — a random HashMap iteration
// order would make scores differ by a ULP run-to-run, breaking pack's
// reproducibility contract. Ascending-id iteration is the canonical order.
pub(super) type Graph = BTreeMap<usize, Vec<usize>>;

pub(super) struct ImportGraph {
    pub(super) edges: Graph,
    pub(super) rust_relative_imports_omitted: bool,
    pub(super) undetermined_imports: BTreeSet<String>,
}

pub(super) fn build_import_graph(nodes: &[Node], manifests: &Manifests) -> ImportGraph {
    let mut exact = PathIndex::default();
    for node in nodes {
        exact
            .by_module
            .entry(node.module_path.as_slice())
            .or_default()
            .push(node.id);
        exact
            .by_directory
            .entry(node.directory.as_slice())
            .or_default()
            .push(node.id);
    }
    let index = ProjectIndex {
        nodes,
        exact,
        manifests,
        python: PythonPath::of(nodes, manifests),
        jvm: JvmIndex::of(nodes),
    };

    let mut graph: Graph = BTreeMap::new();
    let mut rust_relative_imports_omitted = false;
    let mut undetermined_imports = BTreeSet::new();
    for node in nodes {
        graph.entry(node.id).or_default();
        let importer = Importer::of(node, manifests);
        let mut seen = HashSet::new();
        for target in &node.imports.references {
            let reference = Reference::parse(target, node.language);
            let files = match index.resolve(&reference, &importer) {
                Resolution::Files(files) => files,
                Resolution::NeedsOwnership => {
                    rust_relative_imports_omitted = true;
                    continue;
                }
                Resolution::Undetermined(specifier) => {
                    undetermined_imports.insert(specifier);
                    continue;
                }
            };
            for dst in files {
                if dst != node.id && seen.insert(dst) {
                    graph.entry(node.id).or_default().push(dst);
                }
            }
        }
    }
    ImportGraph {
        edges: graph,
        rust_relative_imports_omitted,
        undetermined_imports,
    }
}

/// What a reference resolves to.
enum Resolution {
    /// The files it names: none where it names nothing in the project, or
    /// nothing unambiguous.
    Files(Vec<usize>),
    /// A Rust path whose file only module ownership, which pack does not
    /// hold, would determine.
    NeedsOwnership,
    /// A specifier the project's own configuration declares that names no
    /// file pack reads.
    Undetermined(String),
}

/// What resolving a file's references needs to know about where the file
/// sits, worked out once for the file rather than once per reference.
struct Importer<'a> {
    node: &'a Node,
    /// The directory a `crate::` path written in a Rust file starts at, where
    /// the package layout determines it.
    crate_root: Option<&'a [String]>,
    /// The TypeScript configuration a JavaScript or TypeScript file is
    /// compiled under: the nearest above it.
    ts_config: Option<&'a TsConfig>,
}

impl<'a> Importer<'a> {
    fn of(node: &'a Node, manifests: &'a Manifests) -> Self {
        let crate_root = if node.language == Language::Rust {
            manifests.rust_crate_root(node)
        } else {
            None
        };
        let ts_config = if is_javascript(node.language) {
            nearest(&manifests.ts_configs, &node.directory).map(|(_, config)| config)
        } else {
            None
        };
        Self {
            node,
            crate_root,
            ts_config,
        }
    }
}

/// What a set of candidate files names: one file, none, or several — which
/// name none of them, since an edge to each would hand every one of them the
/// same borrowed rank.
enum Found {
    One(usize),
    Nothing,
    Several,
}

impl Found {
    fn of(candidates: impl IntoIterator<Item = usize>) -> Self {
        let distinct: BTreeSet<usize> = candidates.into_iter().collect();
        let mut distinct = distinct.into_iter();
        match (distinct.next(), distinct.next()) {
            (None, _) => Self::Nothing,
            (Some(one), None) => Self::One(one),
            _ => Self::Several,
        }
    }

    /// The file the first attempt that finds anything names, none where that
    /// attempt finds several.
    fn first(attempts: impl IntoIterator<Item = Found>) -> Vec<usize> {
        for found in attempts {
            match found {
                Self::One(file) => return vec![file],
                Self::Several => return Vec::new(),
                Self::Nothing => {}
            }
        }
        Vec::new()
    }
}

/// Syntactic reference anchors. Filesystem-relative paths retain their hops;
/// an absolute name is read as its language reads it, through what the tree
/// and its manifests declare. Rust paths relative to a module require
/// ownership not held here.
#[derive(Debug, PartialEq, Eq)]
enum Reference {
    /// Relative to the language's source/module context, retaining parent hops.
    Relative { up: usize, components: Vec<String> },
    /// An absolute module name, possibly qualified by its crate or package.
    Absolute(Vec<String>),
    /// A Rust path from the root of the crate writing it (`crate::a::b`).
    Crate(Vec<String>),
    /// A JVM import of all a package or class holds (`import a.b.*`).
    Wildcard(Vec<String>),
    /// A JavaScript or TypeScript specifier that is no relative path
    /// (`react`, `@/components/Button`): a package's, or one the project's
    /// TypeScript configuration maps.
    Bare(String),
    /// A Dart `package:` URI: a file under the named package's `lib`.
    Package {
        package: String,
        components: Vec<String>,
    },
    /// A reference that names nothing in the project, or that is spelled in a
    /// form whose value is not decoded here.
    Unresolved,
}

impl Reference {
    fn parse(raw: &str, language: Language) -> Self {
        if language == Language::Rust {
            return Self::rust(raw);
        }
        if language == Language::Python {
            let dots = raw.bytes().take_while(|byte| *byte == b'.').count();
            let components = raw[dots..]
                .split('.')
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect();
            return if dots > 0 {
                Self::Relative {
                    up: dots - 1,
                    components,
                }
            } else {
                Self::Absolute(components)
            };
        }
        if language == Language::Dart {
            return Self::dart(raw);
        }
        if is_jvm(language) {
            let (name, wildcard) = match raw.strip_suffix(".*") {
                Some(name) => (name, true),
                None => (raw, false),
            };
            let components = name
                .split('.')
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect();
            return if wildcard {
                Self::Wildcard(components)
            } else {
                Self::Absolute(components)
            };
        }
        let mut rest = raw;
        let mut up = 0;
        let mut relative = false;
        loop {
            if let Some(tail) = rest.strip_prefix("../") {
                up += 1;
                relative = true;
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix("./") {
                relative = true;
                rest = tail;
            } else {
                break;
            }
        }
        if is_javascript(language) && !relative {
            return if raw.starts_with('/') {
                Self::Unresolved
            } else {
                Self::Bare(raw.to_string())
            };
        }
        let mut components = split_components(rest);
        if is_javascript(language) {
            as_script_module(&mut components);
        }
        if relative {
            Self::Relative { up, components }
        } else {
            Self::Absolute(components)
        }
    }

    /// A Rust use path: `self` and `super` hops are relative to the module
    /// writing them, `crate` starts at the root of its crate, and any other
    /// first segment names an extern crate or a name in scope.
    fn rust(path: &str) -> Self {
        let segments: Vec<&str> = path.split("::").filter(|part| !part.is_empty()).collect();
        let owned = |segments: &[&str]| segments.iter().map(|part| part.to_string()).collect();
        match segments.split_first() {
            Some((&"crate", rest)) => Self::Crate(owned(rest)),
            Some((&("self" | "super"), _)) => {
                let hops = segments
                    .iter()
                    .take_while(|part| matches!(**part, "self" | "super"))
                    .count();
                Self::Relative {
                    up: segments[..hops]
                        .iter()
                        .filter(|part| **part == "super")
                        .count(),
                    components: owned(&segments[hops..]),
                }
            }
            _ => Self::Absolute(owned(&segments)),
        }
    }

    /// A Dart URI, which the language resolves against the URI of the library
    /// it is written in: one without a scheme is relative to the importing
    /// file whether or not it starts with `./`, a `package:` URI names a file
    /// under that package's `lib`, and any other scheme (`dart:`, `http:`)
    /// names nothing in the project. A URI names a file by its whole name, so
    /// one that does not end in `.dart` names no Dart file, and one carrying
    /// percent-encoding, a query or a fragment is not decoded here.
    fn dart(uri: &str) -> Self {
        if uri.contains(['%', '?', '#']) || uri.starts_with('/') {
            return Self::Unresolved;
        }
        let scheme = uri.split_once(':').filter(|(scheme, _)| {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        });
        let (package, path) = match scheme {
            Some(("package", rest)) => match rest.split_once('/') {
                Some((package, path)) => (Some(package), path),
                None => return Self::Unresolved,
            },
            Some(_) => return Self::Unresolved,
            None => (None, uri),
        };
        let mut up = 0;
        let mut components: Vec<String> = Vec::new();
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if components.pop().is_none() {
                        up += 1;
                    }
                }
                _ => components.push(segment.to_string()),
            }
        }
        let Some(stem) = components
            .pop()
            .and_then(|last| last.strip_suffix(".dart").map(str::to_string))
        else {
            return Self::Unresolved;
        };
        components.push(stem);
        match package {
            // A `package:` path that climbs out of `lib` names no file in it.
            Some(_) if up > 0 => Self::Unresolved,
            Some(package) => Self::Package {
                package: package.to_string(),
                components,
            },
            None => Self::Relative { up, components },
        }
    }
}

/// The module path a JavaScript or TypeScript specifier's components name:
/// without the script extension it may or may not write, and with a
/// directory's `index` folded into the directory, as `module_path` folds it.
pub(super) fn as_script_module(components: &mut Vec<String>) {
    if let Some(last) = components.last_mut()
        && let Some((stem, extension)) = last.rsplit_once('.')
        && matches!(
            extension,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "vue"
        )
    {
        *last = stem.to_string();
    }
    if components.last().is_some_and(|last| last == "index") {
        components.pop();
    }
}

pub(super) fn split_components(raw: &str) -> Vec<String> {
    raw.split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
struct PathIndex<'a> {
    by_module: HashMap<&'a [String], Vec<usize>>,
    by_directory: HashMap<&'a [String], Vec<usize>>,
}

/// The project's files by the names a reference can give them. Every lookup
/// is exact: a module path a reference only ends with is some other module's.
struct ProjectIndex<'a> {
    nodes: &'a [Node],
    exact: PathIndex<'a>,
    manifests: &'a Manifests,
    python: PythonPath<'a>,
    jvm: JvmIndex<'a>,
}

/// What a project's JVM files declare themselves to be. A file's class is
/// named by the package its declaration states and the file's own name,
/// wherever the file sits; its source root is its directory without the path
/// that package spells, where the directory ends in that path, and its source
/// set is what that root belongs to (`SourceSet`).
struct JvmIndex<'a> {
    by_class: HashMap<Vec<String>, Vec<usize>>,
    by_package: HashMap<Vec<String>, Vec<usize>>,
    sets: HashMap<usize, SourceSet<'a>>,
}

impl<'a> JvmIndex<'a> {
    fn of(nodes: &'a [Node]) -> Self {
        let mut index = Self {
            by_class: HashMap::new(),
            by_package: HashMap::new(),
            sets: HashMap::new(),
        };
        for node in nodes.iter().filter(|node| is_jvm(node.language)) {
            let package: Vec<String> = node
                .imports
                .package
                .as_deref()
                .map(|package| package.split('.').map(str::to_string).collect())
                .unwrap_or_default();
            if let Some(root) = node.directory.strip_suffix(package.as_slice()) {
                index.sets.insert(node.id, SourceSet::of(root));
            }
            if let Some(stem) = node.module_path.last() {
                let class = [package.as_slice(), std::slice::from_ref(stem)].concat();
                index.by_class.entry(class).or_default().push(node.id);
            }
            index.by_package.entry(package).or_default().push(node.id);
        }
        index
    }
}

/// The set of sources a JVM source root is compiled with. In the layout
/// Maven and Gradle share, `<module>/src/<set>/<language>`, it is
/// `<module>/src/<set>`, so `src/main/java` and `src/main/kotlin` are one set;
/// any other root is a set of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SourceSet<'a> {
    Named { module: &'a [String], name: &'a str },
    Root(&'a [String]),
}

impl<'a> SourceSet<'a> {
    fn of(root: &'a [String]) -> Self {
        match root {
            [module @ .., src, name, _language] if src == "src" => Self::Named { module, name },
            _ => Self::Root(root),
        }
    }

    /// Whether code in this set sees the classes of `other`: its own, and a
    /// `test` set those of the `main` set beside it, which Maven and Gradle
    /// put on the test classpath. Main code never sees test sources.
    fn sees(self, other: SourceSet<'_>) -> bool {
        match (self, other) {
            (
                Self::Named {
                    module,
                    name: "test",
                },
                SourceSet::Named {
                    module: other,
                    name: "main",
                },
            ) => module == other,
            (Self::Named { module, name }, SourceSet::Named { module: a, name: b }) => {
                module == a && name == b
            }
            (Self::Root(root), SourceSet::Root(other)) => root == other,
            _ => false,
        }
    }

    /// Whether code in this set can see the classes of `other` through any
    /// build: those of a set it sees, of another module's `main` set, which
    /// a build can make it depend on, and of a set whose role the layout does
    /// not say. Another module's `test` set, like every named set besides
    /// `main`, is on no classpath but its own module's.
    fn may_see(self, other: SourceSet<'_>) -> bool {
        self.sees(other)
            || match other {
                SourceSet::Named { name, .. } => name == "main",
                SourceSet::Root(_) => true,
            }
    }
}

/// The directories a Python tree puts on `sys.path`, as its own tooling
/// establishes them: the directory above each top-level package (a package is
/// a directory holding `__init__.py`), and those each project's configuration
/// declares or its build backend discovers.
struct PythonPath<'a> {
    packages: HashSet<&'a [String]>,
    /// The directory above each top-level package, where it is imported from.
    package_roots: Vec<&'a [String]>,
    /// The directories each project puts on `sys.path`, by the project's
    /// directory. The project root is a project whether or not it holds a
    /// configuration file of its own.
    projects: HashMap<Vec<String>, Vec<Vec<String>>>,
}

impl<'a> PythonPath<'a> {
    fn of(nodes: &'a [Node], manifests: &Manifests) -> Self {
        let packages: HashSet<&[String]> = nodes
            .iter()
            .filter(|node| {
                node.language == Language::Python
                    && Path::new(&node.rel_path)
                        .file_stem()
                        .is_some_and(|stem| stem == "__init__")
            })
            .map(|node| node.directory.as_slice())
            .collect();
        let mut package_roots: Vec<&[String]> = packages
            .iter()
            .filter_map(|package| package.split_last().map(|(_, parent)| parent))
            .filter(|parent| !packages.contains(parent))
            .collect();
        package_roots.sort();
        package_roots.dedup();
        let mut projects = manifests.python_projects.clone();
        projects
            .entry(Vec::new())
            .or_insert_with(|| python_project_roots(&[], &[]));
        Self {
            packages,
            package_roots,
            projects,
        }
    }

    /// The directory a file in `directory` is imported from where that
    /// directory is a package: the one above its top-level package, which
    /// has to be on `sys.path` for the file to run as part of its package.
    fn package_root_of(&self, directory: &'a [String]) -> Option<&'a [String]> {
        if !self.packages.contains(directory) {
            return None;
        }
        (0..directory.len())
            .rev()
            .map(|depth| &directory[..depth])
            .find(|directory| !self.packages.contains(directory))
    }
}

impl ProjectIndex<'_> {
    /// What a reference names, nothing where it names something outside the
    /// project or nothing unambiguous.
    fn resolve(&self, reference: &Reference, importer: &Importer) -> Resolution {
        let from = importer.node;
        let files = match reference {
            Reference::Relative { .. } if from.language == Language::Rust => {
                return Resolution::NeedsOwnership;
            }
            Reference::Relative { up, components } => {
                let base = from.directory.len().checked_sub(*up);
                let Some(base) = base else {
                    return Resolution::Files(Vec::new());
                };
                let mut path = from.directory[..base].to_vec();
                path.extend_from_slice(components);
                if package_is_directory(from.language) {
                    self.package(&path, from.language)
                } else {
                    self.module(&path, from.language)
                }
            }
            Reference::Package {
                package,
                components,
            } => match self.manifests.dart_package(package, from) {
                Some(directory) => {
                    let path = [directory, &["lib".to_string()], components].concat();
                    self.module(&path, from.language)
                }
                None => Vec::new(),
            },
            Reference::Unresolved => Vec::new(),
            Reference::Absolute(components) if from.language == Language::Go => self
                .manifests
                .go_package(components, from)
                .map(|directory| self.package(&directory, Language::Go))
                .unwrap_or_default(),
            Reference::Absolute(components) if from.language == Language::Python => {
                self.python_module(components, from)
            }
            Reference::Absolute(name) if is_jvm(from.language) => self.jvm_class(name, from),
            Reference::Wildcard(name) => self.jvm_wildcard(name, from),
            Reference::Crate(path) => match importer.crate_root {
                Some(root) => self.rust_module(root, path),
                None => return Resolution::NeedsOwnership,
            },
            Reference::Absolute(path) if from.language == Language::Rust => {
                return self.rust_path(path, from);
            }
            Reference::Bare(specifier) => return self.bare_specifier(specifier, importer),
            // Anywhere else an absolute name (a Terraform registry source)
            // names what the project's dependencies provide, never a project
            // file by its spelling.
            Reference::Absolute(_) => Vec::new(),
        };
        Resolution::Files(files)
    }

    /// What a non-relative JavaScript or TypeScript specifier names, as
    /// TypeScript resolves one: through the `paths` pattern it matches, each
    /// substitution in turn, then below `baseUrl`, then as a workspace
    /// package the project links in. Any other names a package outside the
    /// project, however a project file's path ends. One a `paths` pattern or
    /// a workspace package's name matches that names no file pack reads is
    /// disclosed rather than dropped.
    fn bare_specifier(&self, specifier: &str, importer: &Importer) -> Resolution {
        let language = importer.node.language;
        let config = importer.ts_config;
        let mapped = config.and_then(|config| config.mapped(specifier));
        for path in mapped.iter().flatten() {
            let found = self.module(path, language);
            if !found.is_empty() {
                return Resolution::Files(found);
            }
        }
        if let Some(path) = config
            .and_then(|config| config.base_url.as_ref())
            .and_then(|base| script_path(base, specifier))
        {
            let found = self.module(&path, language);
            if !found.is_empty() {
                return Resolution::Files(found);
            }
        }
        let package = self.workspace_package(specifier, importer.node);
        match (package, mapped) {
            (Some(found), _) if !found.is_empty() => Resolution::Files(found),
            (Some(_), _) | (None, Some(_)) => Resolution::Undetermined(specifier.to_string()),
            (None, None) => Resolution::Files(Vec::new()),
        }
    }

    /// The file a workspace package's name, or a subpath below it, stands
    /// for, or `None` where the specifier names no package of a workspace
    /// the importer is in. A package with `exports` exports only what it
    /// maps: the name stands for the file `.` maps to, a subpath for the one
    /// `./<subpath>` maps to. Without `exports`, the name stands for the file
    /// its `types`, `typings` or `main` names, else its `index`, and a
    /// subpath for the file below the package. An empty answer is a package
    /// whose file pack cannot determine: a condition or pattern `exports`
    /// maps, an entry naming no file pack reads, or a name two packages of
    /// the workspace give.
    fn workspace_package(&self, specifier: &str, from: &Node) -> Option<Vec<usize>> {
        let scoped = specifier.starts_with('@');
        let mut segments = specifier.splitn(if scoped { 3 } else { 2 }, '/');
        let name: Vec<&str> = segments.by_ref().take(if scoped { 2 } else { 1 }).collect();
        let name = name.join("/");
        let subpath = segments.next().unwrap_or_default();
        let declares = |package: &JsPackage| {
            package.name == name && from.directory.starts_with(&package.workspace)
        };
        let Some((directory, package)) = own_or_only(&self.manifests.js_packages, from, declares)
        else {
            return self
                .manifests
                .js_packages
                .values()
                .any(declares)
                .then(Vec::new);
        };
        let entries: Vec<&str> = match (&package.exports, subpath) {
            (None, "") => package
                .types
                .as_deref()
                .into_iter()
                .chain(package.main.as_deref())
                .chain(["index"])
                .collect(),
            (None, subpath) => vec![subpath],
            (Some(serde_json::Value::String(entry)), "") => vec![entry.as_str()],
            (Some(serde_json::Value::Object(map)), subpath) => map
                .get(&if subpath.is_empty() {
                    ".".to_string()
                } else {
                    format!("./{subpath}")
                })
                .and_then(|entry| entry.as_str())
                .into_iter()
                .collect(),
            (Some(_), _) => Vec::new(),
        };
        let language = from.language;
        Some(
            entries
                .into_iter()
                .filter_map(|entry| script_path(directory, entry))
                .map(|path| self.module(&path, language))
                .find(|found| !found.is_empty())
                .unwrap_or_default(),
        )
    }

    /// What a Rust path from a name, rather than from `crate`, `self` or
    /// `super`, names. Through a module the file declares (`mod config; use
    /// config::Settings;`) it is relative to the module writing it, which only
    /// module ownership places; from a project library's name it starts at
    /// that library's root; any other name is an extern crate's, which names
    /// nothing in the project and leaves nothing out of its graph.
    fn rust_path(&self, path: &[String], from: &Node) -> Resolution {
        let Some((name, path)) = path.split_first() else {
            return Resolution::Files(Vec::new());
        };
        if from.imports.modules.contains(name) {
            return Resolution::NeedsOwnership;
        }
        Resolution::Files(
            self.manifests
                .rust_library(name, from)
                .map(|root| self.rust_module(root, path))
                .unwrap_or_default(),
        )
    }

    /// The module a Rust path below a crate's root names: the file of that
    /// module path, read whole and then without its last segment, which
    /// names an item in the module where it is not a module itself.
    fn rust_module(&self, root: &[String], path: &[String]) -> Vec<usize> {
        let parent = path.split_last().map(|(_, parent)| parent);
        Found::first(
            std::iter::once(path)
                .chain(parent)
                .filter(|candidate| !candidate.is_empty())
                .map(|candidate| {
                    self.modules_at([[root, candidate].concat()], |language| {
                        language == Language::Rust
                    })
                }),
        )
    }

    /// The file an import of a JVM class names: the one whose declared package
    /// and name spell the class — read whole, then without each trailing
    /// segment in turn, which nested classes and members add to the class
    /// their file declares. Scala imports a package by its name as well,
    /// which names the files the package holds.
    fn jvm_class(&self, name: &[String], from: &Node) -> Vec<usize> {
        for length in (1..=name.len()).rev() {
            if let Some(files) = self.jvm.by_class.get(&name[..length]) {
                return self.jvm_one(files, from).into_iter().collect();
            }
        }
        if from.language == Language::Scala {
            return self.jvm_package(name, from);
        }
        Vec::new()
    }

    /// The files `import a.b.*` names: the class `a.b`, whose members it
    /// imports, or else everything the package `a.b` holds.
    fn jvm_wildcard(&self, name: &[String], from: &Node) -> Vec<usize> {
        match self.jvm.by_class.get(name) {
            Some(files) => self.jvm_one(files, from).into_iter().collect(),
            None => self.jvm_package(name, from),
        }
    }

    /// The one file of several declaring a class that an importer means: the
    /// one in its own source set, which shadows the rest on its classpath,
    /// else the one in a set it sees, else the only one in a set it may see.
    fn jvm_one(&self, files: &[usize], from: &Node) -> Option<usize> {
        let own = self.jvm.sets.get(&from.id).copied();
        let set = |id: &usize| self.jvm.sets.get(id).copied();
        let found = Found::first([
            Found::of(
                files
                    .iter()
                    .copied()
                    .filter(|id| own.is_some() && set(id) == own),
            ),
            Found::of(
                files
                    .iter()
                    .copied()
                    .filter(|id| own.zip(set(id)).is_some_and(|(own, other)| own.sees(other))),
            ),
            Found::of(
                files
                    .iter()
                    .copied()
                    .filter(|id| self.jvm_may_see(from, *id)),
            ),
        ]);
        found.first().copied()
    }

    /// Whether the importer `from` may see the classes of the JVM file `id`
    /// (`SourceSet::may_see`). An importer whose set its layout does not say
    /// may be anything, a test included.
    fn jvm_may_see(&self, from: &Node, id: usize) -> bool {
        let set = self
            .jvm
            .sets
            .get(&id)
            .copied()
            .unwrap_or(SourceSet::Root(&self.nodes[id].directory));
        self.jvm
            .sets
            .get(&from.id)
            .is_none_or(|own| own.may_see(set))
    }

    /// The files an import of a whole JVM package names: every file declaring
    /// it in a source set the importer's sees — its own, and `main` from
    /// `test`. Where no set it sees holds the package, the package is another
    /// module's, which only a build file would say it depends on: it names
    /// the files of the one source set holding it that the importer may see
    /// — never another module's `test` set — or none where several do.
    fn jvm_package(&self, package: &[String], from: &Node) -> Vec<usize> {
        let Some(files) = self.jvm.by_package.get(package) else {
            return Vec::new();
        };
        if let Some(own) = self.jvm.sets.get(&from.id) {
            let seen: Vec<usize> = files
                .iter()
                .copied()
                .filter(|id| self.jvm.sets.get(id).is_some_and(|set| own.sees(*set)))
                .collect();
            if !seen.is_empty() {
                return seen;
            }
        }
        let set = |id: &usize| {
            self.jvm
                .sets
                .get(id)
                .copied()
                .unwrap_or(SourceSet::Root(&self.nodes[*id].directory))
        };
        let visible: Vec<usize> = files
            .iter()
            .copied()
            .filter(|id| self.jvm_may_see(from, *id))
            .collect();
        let holding: HashSet<SourceSet> = visible.iter().map(set).collect();
        if holding.len() == 1 {
            visible
        } else {
            Vec::new()
        }
    }

    /// The one file at the module path `path` that a `language` file can
    /// import, or none where none or several are.
    fn module(&self, path: &[String], language: Language) -> Vec<usize> {
        Found::first([self.modules_at([path.to_vec()], |target| imports_across(language, target))])
    }

    /// What the files at any of `paths`, module paths, of a language
    /// `accepts` takes name.
    fn modules_at(
        &self,
        paths: impl IntoIterator<Item = Vec<String>>,
        accepts: impl Fn(Language) -> bool,
    ) -> Found {
        Found::of(
            paths
                .into_iter()
                .flat_map(|path| {
                    self.exact
                        .by_module
                        .get(path.as_slice())
                        .into_iter()
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>()
                })
                .filter(|id| accepts(self.nodes[*id].language)),
        )
    }

    /// The module an absolute Python import names: the file at that dotted
    /// path below a directory on `sys.path` (`PythonPath`), searched first
    /// among the importer's own — the one above its top-level package, and
    /// those of its own project — then among every other. Two files on one
    /// footing name neither. A path below none of them (the standard library,
    /// an installed distribution) names nothing in the project, however its
    /// last segments match a project file: `logging` is not
    /// `app/util/logging.py`, whose name is `app.util.logging`, and a
    /// directory is on `sys.path` only where the tooling puts it there, never
    /// because a script happens to sit in it.
    fn python_module(&self, module: &[String], from: &Node) -> Vec<usize> {
        if module.is_empty() {
            return Vec::new();
        }
        let own_project = nearest(&self.python.projects, &from.directory)
            .map(|(_, roots)| roots.as_slice())
            .unwrap_or_default();
        let own: Vec<&[String]> = self
            .python
            .package_root_of(&from.directory)
            .into_iter()
            .chain(own_project.iter().map(Vec::as_slice))
            .collect();
        let every: Vec<&[String]> = self
            .python
            .package_roots
            .iter()
            .copied()
            .chain(self.python.projects.values().flatten().map(Vec::as_slice))
            .collect();
        let mut searched = HashSet::new();
        Found::first([own, every].into_iter().map(|tier| {
            let bases: Vec<&[String]> = tier
                .into_iter()
                .filter(|base| searched.insert(*base))
                .collect();
            self.modules_at(
                bases.into_iter().map(|base| [base, module].concat()),
                |language| language == Language::Python,
            )
        }))
    }

    /// The files of the package that is the directory at `path`, in a
    /// language whose packages are directories. A file beside it named like
    /// the directory is not the package. A Go package is the directory's `.go`
    /// files the build reads: neither its tests (`_test.go`, whichever package
    /// they declare) nor the files it ignores (named from `_` or `.`).
    fn package(&self, path: &[String], language: Language) -> Vec<usize> {
        let Some(files) = self.exact.by_directory.get(path) else {
            return Vec::new();
        };
        files
            .iter()
            .copied()
            .filter(|id| {
                let node = &self.nodes[*id];
                let name = Path::new(&node.rel_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                node.language == language
                    && !(language == Language::Go
                        && (name.ends_with("_test.go") || name.starts_with(['_', '.'])))
            })
            .collect()
    }
}

/// Whether the language defines a package as a directory, so that a
/// reference to one names every file in it and never a single module file.
/// Go's specification says so outright, and a Terraform module's source is
/// its directory; the rest address a directory through a `mod`/`index` file,
/// which `module_path` already folds in.
fn package_is_directory(language: Language) -> bool {
    matches!(language, Language::Go | Language::Terraform)
}

/// Whether the language is one of the JVM's, which share one namespace of
/// packages and classes: a Kotlin file imports a Java class by the name its
/// package declaration gives it, as a Java file does.
fn is_jvm(language: Language) -> bool {
    matches!(
        language,
        Language::Java | Language::Kotlin | Language::Scala
    )
}

/// Whether a `from` file's import can name a `to` file: one of its own
/// language, or any of the JavaScript family, which import each other.
fn imports_across(from: Language, to: Language) -> bool {
    from == to || (is_javascript(from) && is_javascript(to))
}

/// Whether the language is one of the JavaScript family, which share module
/// resolution and import each other's files.
pub(super) fn is_javascript(language: Language) -> bool {
    matches!(
        language,
        Language::JavaScript | Language::TypeScript | Language::Vue
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::pack::manifests::{RustLibrary, RustPackage};
    use crate::services::pack::tests::{node, node_in};

    fn edges_of(nodes: &[Node], id: usize) -> Vec<String> {
        edges_under(nodes, id, &Manifests::default())
    }

    /// Manifests declaring each Go module path at its directory, as the
    /// `go.mod` files there would.
    fn go_modules(modules: &[(&str, &str)]) -> Manifests {
        Manifests {
            go_modules: modules
                .iter()
                .map(|(directory, module)| (split_components(directory), split_components(module)))
                .collect(),
            ..Manifests::default()
        }
    }

    /// Manifests declaring one Cargo package at the project root, its
    /// library `app` rooted in `src/`, as the default layout has it.
    fn default_cargo_layout() -> Manifests {
        let src = vec!["src".to_string()];
        Manifests {
            rust_packages: HashMap::from([(
                Vec::new(),
                RustPackage {
                    library: Some(RustLibrary {
                        name: "app".to_string(),
                        root: src.clone(),
                    }),
                    roots: vec![src],
                },
            )]),
            ..Manifests::default()
        }
    }

    fn rust_edges(nodes: &[Node], id: usize) -> Vec<String> {
        edges_under(nodes, id, &default_cargo_layout())
    }

    /// The manifests a project holding these files declares, read from disk
    /// as a pack over `nodes` would read them.
    fn manifests_of(files: &[(&str, &str)], nodes: &[Node]) -> Manifests {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write manifest");
        }
        Manifests::read(dir.path(), nodes)
    }

    fn edges_under(nodes: &[Node], id: usize, manifests: &Manifests) -> Vec<String> {
        build_import_graph(nodes, manifests)
            .edges
            .get(&id)
            .map(|dsts| {
                dsts.iter()
                    .map(|d| nodes[*d].rel_path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// A reference is parsed with the anchor it was written against, and a
    /// relative one keeps the hop count that makes it exact.
    #[test]
    fn a_reference_keeps_the_anchor_it_was_written_against() {
        let abs = |c: &[&str]| Reference::Absolute(c.iter().map(|s| s.to_string()).collect());
        let rel = |up, c: &[&str]| Reference::Relative {
            up,
            components: c.iter().map(|s| s.to_string()).collect(),
        };

        assert_eq!(
            Reference::parse("crate::services::pack", Language::Rust),
            Reference::Crate(vec!["services".into(), "pack".into()])
        );
        assert_eq!(
            Reference::parse("serde::Serialize", Language::Rust),
            abs(&["serde", "Serialize"])
        );
        assert_eq!(
            Reference::parse("github.com/acme/w/internal/store", Language::Go),
            abs(&["github.com", "acme", "w", "internal", "store"])
        );
        assert_eq!(
            Reference::parse("./services/pack", Language::TypeScript),
            rel(0, &["services", "pack"])
        );
        assert_eq!(
            Reference::parse("../../shared/util", Language::TypeScript),
            rel(2, &["shared", "util"])
        );
        assert_eq!(
            Reference::parse("super::super::util", Language::Rust),
            rel(2, &["util"])
        );
        assert_eq!(
            Reference::parse("self::helper", Language::Rust),
            rel(0, &["helper"])
        );
        assert_eq!(
            Reference::parse("..pkg.sibling", Language::Python),
            rel(1, &["pkg", "sibling"])
        );
        assert_eq!(Reference::parse(".", Language::Python), rel(0, &[]));
        assert_eq!(Reference::parse("self", Language::Rust), rel(0, &[]));
        assert_eq!(Reference::parse("super", Language::Rust), rel(1, &[]));
        assert_eq!(
            Reference::parse("crate", Language::Rust),
            Reference::Crate(Vec::new())
        );
        assert_eq!(Reference::parse("b.dart", Language::Dart), rel(0, &["b"]));
        assert_eq!(
            Reference::parse("src/../../x.g.dart", Language::Dart),
            rel(1, &["x.g"])
        );
        assert_eq!(
            Reference::parse("package:app/src/a.dart", Language::Dart),
            Reference::Package {
                package: "app".into(),
                components: vec!["src".into(), "a".into()]
            }
        );
        for unresolved in [
            "dart:async",
            "http://x/a.dart",
            "b",
            "a%20b.dart",
            "/abs.dart",
        ] {
            assert_eq!(
                Reference::parse(unresolved, Language::Dart),
                Reference::Unresolved,
                "{unresolved}"
            );
        }
    }

    /// Dart resolves every URI against the library it is written in, so one
    /// without a scheme names the file beside the importer whether or not it
    /// starts with `./` — never a same-named file elsewhere.
    #[test]
    fn a_dart_uri_without_a_scheme_is_relative_to_the_importing_file() {
        let nodes = vec![
            node_in(
                0,
                "lib/src/a.dart",
                "import 'b.dart';\nimport 'sub/../c.dart';\nimport 'd';\n\
                 import 'dart:async';\npart 'a.g.dart';\n",
                Language::Dart,
            ),
            node_in(1, "lib/src/b.dart", "", Language::Dart),
            node_in(2, "test/b.dart", "", Language::Dart),
            node_in(3, "lib/src/a.g.dart", "part of 'a.dart';\n", Language::Dart),
            node_in(4, "lib/src/c.dart", "", Language::Dart),
            node_in(5, "lib/src/d.dart", "", Language::Dart),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec!["lib/src/b.dart", "lib/src/c.dart", "lib/src/a.g.dart"]
        );
        assert_eq!(edges_of(&nodes, 3), vec!["lib/src/a.dart"]);
    }

    /// Two files can share a trailing name and be reached by relative
    /// references of different depth. Dropping the depth made both
    /// references ambiguous and cost the edges entirely.
    #[test]
    fn relative_references_of_different_depth_reach_different_files() {
        let nodes = vec![
            node_in(
                0,
                "src/a/b/caller.ts",
                "import { x } from \"./utils\";\n",
                Language::TypeScript,
            ),
            node_in(1, "src/a/b/utils.ts", "", Language::TypeScript),
            node_in(2, "src/a/utils.ts", "", Language::TypeScript),
            node_in(
                3,
                "src/a/b/other.ts",
                "import { y } from \"../utils\";\n",
                Language::TypeScript,
            ),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/a/b/utils.ts"]);
        assert_eq!(edges_of(&nodes, 3), vec!["src/a/utils.ts"]);
    }

    #[test]
    fn relative_paths_only_reach_exact_project_paths() {
        for (language, caller, import, target, nested) in [
            (
                Language::TypeScript,
                "caller.ts",
                "import {helper} from './helper';\n",
                "helper.ts",
                "nested/helper.ts",
            ),
            (
                Language::Python,
                "pkg/caller.py",
                "from .helper import value\n",
                "pkg/helper.py",
                "nested/pkg/helper.py",
            ),
            (
                Language::Terraform,
                "main.tf",
                "module \"network\" { source = \"./network\" }\n",
                "network/main.tf",
                "nested/network/main.tf",
            ),
        ] {
            let mut nodes = vec![
                node_in(0, caller, import, language),
                node_in(1, nested, "", language),
            ];
            assert!(edges_of(&nodes, 0).is_empty(), "{language:?}");
            nodes.push(node_in(2, target, "", language));
            assert_eq!(edges_of(&nodes, 0), vec![target], "{language:?}");
        }
    }

    /// Go repeats its declared module path in every intra-project import,
    /// and that prefix is not a directory anywhere in the tree. Without
    /// reading it out of `go.mod` a Go project forms no edges at all, and
    /// the ranking it feeds degenerates to uniform.
    #[test]
    fn a_go_import_resolves_through_the_module_path_go_mod_declares() {
        let nodes = vec![
            node_in(
                0,
                "cmd/server/main.go",
                "import \"github.com/acme/widget/internal/store\"\n",
                Language::Go,
            ),
            node_in(1, "internal/store/store.go", "", Language::Go),
            node_in(2, "internal/store/query.go", "", Language::Go),
        ];
        // A Go package is a directory, so the reference names every file in it.
        assert_eq!(
            edges_under(&nodes, 0, &go_modules(&[("", "github.com/acme/widget")])),
            vec!["internal/store/store.go", "internal/store/query.go"]
        );
        // Without the declared prefix the reference matches nothing.
        assert!(edges_of(&nodes, 0).is_empty());
    }

    /// A Go import path always names a package directory — the build's files
    /// in it — and never a file beside it that happens to share its name, nor
    /// the package's tests. One under no module the project declares (the
    /// standard library, a dependency) names nothing here, however its last
    /// segments match a project path.
    #[test]
    fn a_go_import_names_a_package_directory_of_a_declared_module_only() {
        let nodes = vec![
            node_in(
                0,
                "cmd/app/main.go",
                "package main\nimport (\n\t\"example.com/svc/internal/config\"\n\t\
                 \"example.com/svc/internal/store\"\n\t\"log\"\n)\n",
                Language::Go,
            ),
            node_in(1, "cmd/app/log.go", "package main\n", Language::Go),
            node_in(2, "internal/config.go", "package internal\n", Language::Go),
            node_in(
                3,
                "internal/config/load.go",
                "package config\n",
                Language::Go,
            ),
            node_in(4, "internal/context.go", "package internal\n", Language::Go),
            node_in(
                5,
                "internal/store/store.go",
                "package store\nimport (\n\t\"context\"\n\t\"errors\"\n\t\"log\"\n\t\
                 \"github.com/other/svc/internal/config\"\n)\n",
                Language::Go,
            ),
            node_in(
                6,
                "internal/store/errors.go",
                "package store\n",
                Language::Go,
            ),
            node_in(
                7,
                "internal/store/store_test.go",
                "package store_test\n",
                Language::Go,
            ),
            node_in(
                8,
                "internal/store/_scratch.go",
                "package store\n",
                Language::Go,
            ),
        ];
        let manifests = go_modules(&[("", "example.com/svc")]);
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            vec![
                "internal/config/load.go",
                "internal/store/store.go",
                "internal/store/errors.go"
            ]
        );
        assert!(edges_under(&nodes, 5, &manifests).is_empty());
    }

    /// Every `go.mod` roots its own module, wherever it sits: a workspace's
    /// modules import each other by their declared paths, a directory with a
    /// `go.mod` of its own is no package of the module above it, and a path
    /// under no declared module names what the importer's module vendors.
    #[test]
    fn every_go_mod_maps_its_module_path_to_its_own_directory() {
        let util = "import \"example.com/backend/pkg/util\"\n";
        let nodes = vec![
            node_in(
                0,
                "backend/cmd/main.go",
                &format!("{util}import \"example.com/backend/plugins/auth\"\n"),
                Language::Go,
            ),
            node_in(1, "backend/pkg/util/u.go", "", Language::Go),
            node_in(2, "backend/plugins/auth/auth.go", "", Language::Go),
            node_in(
                3,
                "tools/gen/g.go",
                &format!("{util}import \"github.com/dep/lib\"\n"),
                Language::Go,
            ),
            node_in(
                4,
                "tools/vendor/github.com/dep/lib/lib.go",
                "",
                Language::Go,
            ),
            node_in(
                5,
                "backend/vendor/github.com/dep/lib/lib.go",
                "",
                Language::Go,
            ),
        ];
        let manifests = manifests_of(
            &[
                ("backend/go.mod", "module example.com/backend\n"),
                ("tools/go.mod", "module example.com/tools\n"),
                ("backend/plugins/go.mod", "module example.com/plugins\n"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            vec!["backend/pkg/util/u.go"]
        );
        assert_eq!(
            edges_under(&nodes, 3, &manifests),
            vec![
                "backend/pkg/util/u.go",
                "tools/vendor/github.com/dep/lib/lib.go"
            ]
        );
    }

    /// Two `go.mod` files can declare one module path — a copied example, a
    /// fork. A file of either imports its own module's packages; any other
    /// importer cannot tell which of them it builds against.
    #[test]
    fn a_go_module_path_two_go_mods_declare_names_only_the_importers_own() {
        let import = "package main\nimport \"example.com/app/lib\"\n";
        let nodes = vec![
            node_in(0, "a/cmd/main.go", import, Language::Go),
            node_in(1, "a/lib/lib.go", "package lib\n", Language::Go),
            node_in(2, "b/cmd/main.go", import, Language::Go),
            node_in(3, "b/lib/lib.go", "package lib\n", Language::Go),
            node_in(4, "c/main.go", import, Language::Go),
        ];
        let manifests = go_modules(&[
            ("a", "example.com/app"),
            ("b", "example.com/app"),
            ("c", "example.com/c"),
        ]);
        assert_eq!(edges_under(&nodes, 0, &manifests), vec!["a/lib/lib.go"]);
        assert_eq!(edges_under(&nodes, 2, &manifests), vec!["b/lib/lib.go"]);
        assert!(edges_under(&nodes, 4, &manifests).is_empty());
    }

    /// A Dart file imports its own package's files as `package:name/…`, which
    /// names the `lib` directory of the package whose `pubspec.yaml` declares
    /// that name, wherever in the project it sits.
    #[test]
    fn a_dart_package_uri_resolves_under_the_lib_of_the_package_its_pubspec_names() {
        let nodes = vec![
            node_in(
                0,
                "lib/main.dart",
                "import 'package:app/src/a.dart';\nimport 'package:core/model.dart';\n\
                 import 'package:flutter/material.dart';\n",
                Language::Dart,
            ),
            node_in(1, "lib/src/a.dart", "", Language::Dart),
            node_in(2, "packages/core/lib/model.dart", "", Language::Dart),
            node_in(3, "packages/core/lib/src/a.dart", "", Language::Dart),
        ];
        let manifests = manifests_of(
            &[
                ("pubspec.yaml", "name: app # the package\nversion: 1.0.0\n"),
                ("packages/core/pubspec.yaml", "name: core\n"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            vec!["lib/src/a.dart", "packages/core/lib/model.dart"]
        );
    }

    /// Two pubspecs can declare one name — a fork, a copied example. The
    /// importer's own package answers to its name whatever else declares it;
    /// any other importer cannot tell which of them it depends on, so the URI
    /// names neither, whichever manifest was read last.
    #[test]
    fn a_dart_package_name_two_pubspecs_declare_names_only_the_importers_own() {
        let import = "import 'package:core/src/a.dart';\n";
        let nodes = vec![
            node_in(0, "app/lib/main.dart", import, Language::Dart),
            node_in(1, "legacy/core/lib/core.dart", import, Language::Dart),
            node_in(2, "legacy/core/lib/src/a.dart", "", Language::Dart),
            node_in(3, "packages/core/lib/core.dart", import, Language::Dart),
            node_in(4, "packages/core/lib/src/a.dart", "", Language::Dart),
        ];
        let manifests = manifests_of(
            &[
                ("app/pubspec.yaml", "name: app\n"),
                ("packages/core/pubspec.yaml", "name: core\n"),
                ("legacy/core/pubspec.yaml", "name: core\n"),
            ],
            &nodes,
        );
        assert!(edges_under(&nodes, 0, &manifests).is_empty());
        assert_eq!(
            edges_under(&nodes, 1, &manifests),
            vec!["legacy/core/lib/src/a.dart"]
        );
        assert_eq!(
            edges_under(&nodes, 3, &manifests),
            vec!["packages/core/lib/src/a.dart"]
        );
    }

    /// A JVM import names classes and packages by what files declare, not by
    /// where they sit: a class is its file's declared package and name, and a
    /// whole package is the files declaring it in every source set the
    /// importer sees — Java and Kotlin roots of one set alike, `main` from
    /// `test`, but never test sources from main code. Java, Kotlin and Scala
    /// share that one namespace.
    #[test]
    fn a_jvm_import_names_what_files_declare_in_the_source_sets_it_sees() {
        let java = |id, rel, source| node_in(id, rel, source, Language::Java);
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let util = "package com.acme.util;\n";
        let nodes = vec![
            java(
                0,
                "src/main/java/com/acme/app/App.java",
                "package com.acme.app;\nimport com.acme.util.*;\nimport java.util.List;\n\
                 import com.acme.model.User;\n",
            ),
            java(1, "src/main/java/com/acme/util/Strings.java", util),
            java(2, "src/main/java/com/acme/util/Numbers.java", util),
            java(3, "src/test/java/com/acme/util/StringsTest.java", util),
            java(4, "src/test/java/com/acme/util/Numbers.java", util),
            java(5, "src/main/java/util/List.java", "package util;\n"),
            kotlin(
                6,
                "src/main/kotlin/com/acme/kt/Main.kt",
                "package com.acme.kt\nimport com.acme.util.Strings\n\
                 import com.acme.util.Numbers\nimport com.acme.model.User.Companion.create\n",
            ),
            // Kotlin does not tie a file's directory to its package.
            kotlin(
                7,
                "src/main/kotlin/model/User.kt",
                "package com.acme.model\n",
            ),
            java(
                8,
                "src/test/java/com/acme/app/AppTest.java",
                "package com.acme.app;\nimport com.acme.util.*;\n",
            ),
            kotlin(
                9,
                "src/main/kotlin/com/acme/util/Text.kt",
                "package com.acme.util\n",
            ),
            java(
                10,
                "lib/src/main/java/com/lib/Lib.java",
                "package com.lib;\n",
            ),
            java(
                11,
                "cli/src/main/java/com/cli/Cli.java",
                "package com.cli;\nimport com.lib.*;\nimport com.shared.*;\n\
                 import com.lib.LibTest;\nimport com.lib.Twin;\n",
            ),
            java(
                12,
                "lib/src/test/java/com/lib/LibTest.java",
                "package com.lib;\n",
            ),
            java(
                13,
                "a/src/main/java/com/shared/A.java",
                "package com.shared;\n",
            ),
            java(
                14,
                "b/src/main/java/com/shared/B.java",
                "package com.shared;\n",
            ),
            java(
                15,
                "lib/src/main/java/com/lib/Twin.java",
                "package com.lib;\n",
            ),
            java(
                16,
                "lib/src/test/java/com/lib/Twin.java",
                "package com.lib;\n",
            ),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec![
                "src/main/java/com/acme/util/Strings.java",
                "src/main/java/com/acme/util/Numbers.java",
                "src/main/kotlin/com/acme/util/Text.kt",
                "src/main/kotlin/model/User.kt"
            ]
        );
        // `Numbers` is declared in main and in test; main code means its own.
        assert_eq!(
            edges_of(&nodes, 6),
            vec![
                "src/main/java/com/acme/util/Strings.java",
                "src/main/java/com/acme/util/Numbers.java",
                "src/main/kotlin/model/User.kt"
            ]
        );
        assert_eq!(
            edges_of(&nodes, 8),
            vec![
                "src/main/java/com/acme/util/Strings.java",
                "src/main/java/com/acme/util/Numbers.java",
                "src/test/java/com/acme/util/StringsTest.java",
                "src/test/java/com/acme/util/Numbers.java",
                "src/main/kotlin/com/acme/util/Text.kt"
            ]
        );
        // Another module's package is the files of the one set holding it
        // that the importer may see, its `main` — its `test` set is on no
        // other module's classpath — and one two modules' `main` sets hold
        // names neither. A class is another module's `main` one alike.
        assert_eq!(
            edges_of(&nodes, 11),
            vec![
                "lib/src/main/java/com/lib/Lib.java",
                "lib/src/main/java/com/lib/Twin.java"
            ]
        );
    }

    #[test]
    fn a_crate_path_resolves_to_the_file_of_its_module_below_the_crate_root() {
        let nodes = vec![
            node(0, "src/cli/output.rs", "use crate::services::pack;\n"),
            node(1, "src/services/pack.rs", ""),
            node(2, "src/cli/services/pack.rs", ""),
        ];
        assert_eq!(rust_edges(&nodes, 0), vec!["src/services/pack.rs"]);
        // No `Cargo.toml` governs the file, so nothing says where its crate's
        // root is.
        assert!(edges_of(&nodes, 0).is_empty());
    }

    #[test]
    fn a_reference_that_names_an_item_resolves_to_its_module() {
        let nodes = vec![
            node(
                0,
                "src/cli/output.rs",
                "use crate::services::pack::PackConfig;\n",
            ),
            node(1, "src/services/pack.rs", ""),
        ];
        assert_eq!(rust_edges(&nodes, 0), vec!["src/services/pack.rs"]);
    }

    /// The failure a bare-name index cannot avoid: an external crate whose
    /// segments happen to match a project file's name. `std::process` and
    /// `serde::Serialize` name nothing in this project, so they contribute
    /// no edge, while the project's own library is found by its name.
    #[test]
    fn an_external_reference_never_lands_on_a_same_named_project_file() {
        let nodes = vec![
            node(
                0,
                "src/cli/output.rs",
                "use std::process::Command;\nuse serde::Serialize;\nuse log::info;\n",
            ),
            node(1, "src/services/dist/process.rs", ""),
            node(2, "src/util/serde.rs", ""),
            node(3, "src/log.rs", ""),
            node(4, "tests/cli.rs", "use app::services::pack;\n"),
            node(5, "src/services/pack.rs", ""),
        ];
        assert!(rust_edges(&nodes, 0).is_empty());
        assert_eq!(rust_edges(&nodes, 4), vec!["src/services/pack.rs"]);
    }

    /// An absolute Python import names a module below a directory on
    /// `sys.path`: `import logging` is the standard library's, not
    /// `app/util/logging.py`, whose name is `app.util.logging`, nor a
    /// `logging.py` beside the importer, since a directory is on `sys.path`
    /// only where the tooling puts it there.
    #[test]
    fn an_absolute_python_import_names_a_module_on_sys_path_only() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "app/__init__.py", ""),
            python(
                1,
                "app/main.py",
                "import logging\nimport util.logging\nimport app.util.logging\n\
                 from app.util import helpers\n",
            ),
            python(2, "app/util/__init__.py", ""),
            python(3, "app/util/logging.py", ""),
            python(4, "src/pkg/__init__.py", ""),
            python(5, "src/pkg/core.py", ""),
            python(
                6,
                "tests/test_core.py",
                "import pkg.core\nimport conftest\n",
            ),
            python(7, "tests/conftest.py", ""),
            python(8, "lib/util/a.py", "import logging\n"),
            python(9, "lib/util/logging.py", ""),
            python(10, "src/ns/a.py", "import ns.core\n"),
            python(11, "src/ns/core.py", ""),
            python(12, "a/app/__init__.py", ""),
            python(13, "a/app/db.py", ""),
            python(14, "b/app/__init__.py", ""),
            python(15, "b/app/db.py", ""),
            python(16, "a/app/serve.py", "import app.db\n"),
            python(17, "c/serve.py", "import app.db\n"),
        ];
        assert_eq!(
            edges_of(&nodes, 1),
            vec!["app/util/logging.py", "app/util/__init__.py"]
        );
        // `src/` is on `sys.path` by the src layout; the test's own directory
        // is not, whatever the test runner does with it.
        assert_eq!(edges_of(&nodes, 6), vec!["src/pkg/core.py"]);
        assert!(edges_of(&nodes, 8).is_empty());
        // A namespace package needs no `__init__.py` below a root, and `src/`
        // is one by the src layout even where no package says so.
        assert_eq!(edges_of(&nodes, 10), vec!["src/ns/core.py"]);
        let namespace = vec![
            python(0, "src/app/a.py", "import app.core\n"),
            python(1, "src/app/core.py", ""),
        ];
        assert_eq!(edges_of(&namespace, 0), vec!["src/app/core.py"]);
        // An importer inside a top-level package finds that package first;
        // one outside every `app` cannot tell which of them it means.
        assert_eq!(edges_of(&nodes, 16), vec!["a/app/db.py"]);
        assert!(edges_of(&nodes, 17).is_empty());
    }

    /// A project's configuration says where its packages are imported from,
    /// which replaces the layouts setuptools would discover, and pytest's
    /// `pythonpath` adds to them.
    #[test]
    fn a_python_project_puts_on_sys_path_what_its_configuration_declares() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "svc/code/app/core.py", ""),
            python(1, "svc/code/app/main.py", "import app.core\n"),
            python(2, "svc/src/app/core.py", ""),
            python(3, "svc/checks/helpers.py", ""),
            python(
                4,
                "svc/checks/test_main.py",
                "import helpers\nimport app.core\n",
            ),
            python(5, "poetry/source/pkg/a.py", "import pkg.b\n"),
            python(6, "poetry/source/pkg/b.py", ""),
            python(7, "cfg/lib/mod/a.py", "import mod.b\nimport shared\n"),
            python(8, "cfg/lib/mod/b.py", ""),
            python(9, "cfg/tools/shared.py", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "svc/pyproject.toml",
                    "[tool.setuptools]\npackage-dir = {\"\" = \"code\"}\n\
                     [tool.pytest.ini_options]\npythonpath = [\"checks\"]\n",
                ),
                (
                    "poetry/pyproject.toml",
                    "[tool.poetry]\npackages = [{ include = \"pkg\", from = \"source\" }]\n",
                ),
                (
                    "cfg/setup.cfg",
                    "[options]\npackage_dir =\n    =lib\n\n[tool:pytest]\npythonpath = tools\n",
                ),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 1, &manifests),
            vec!["svc/code/app/core.py"]
        );
        assert_eq!(
            edges_under(&nodes, 4, &manifests),
            vec!["svc/checks/helpers.py", "svc/code/app/core.py"]
        );
        assert_eq!(
            edges_under(&nodes, 5, &manifests),
            vec!["poetry/source/pkg/b.py"]
        );
        assert_eq!(
            edges_under(&nodes, 7, &manifests),
            vec!["cfg/lib/mod/b.py", "cfg/tools/shared.py"]
        );
    }

    /// A name that is no path in the project — a JavaScript package
    /// specifier, a Terraform registry source — names what the project's
    /// dependencies provide, however a project file's path ends.
    #[test]
    fn a_package_specifier_never_lands_on_a_project_file_it_spells() {
        let nodes = vec![
            node_in(
                0,
                "src/app.ts",
                "import React from \"react\";\nimport merge from \"lodash/merge\";\n\
                 import { util } from \"./util\";\n",
                Language::TypeScript,
            ),
            node_in(1, "src/react.ts", "", Language::TypeScript),
            node_in(2, "src/lodash/merge.ts", "", Language::TypeScript),
            node_in(3, "src/util.ts", "", Language::TypeScript),
            node_in(
                4,
                "main.tf",
                "module \"vpc\" { source = \"terraform-aws-modules/vpc/aws\" }\n",
                Language::Terraform,
            ),
            node_in(5, "modules/vpc/aws/main.tf", "", Language::Terraform),
        ];
        assert_eq!(edges_of(&nodes, 0), vec!["src/util.ts"]);
        assert!(edges_of(&nodes, 4).is_empty());
    }

    /// TypeScript maps a non-relative specifier through the `paths` pattern
    /// it matches, then below `baseUrl`, from the nearest configuration and
    /// what it extends; `paths` resolve against `baseUrl`, or without one
    /// against the configuration declaring them. A specifier a pattern
    /// matches that names no file is disclosed; one nothing maps is a
    /// package's.
    #[test]
    fn a_typescript_specifier_resolves_through_the_paths_and_base_url_its_config_declares() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let nodes = vec![
            ts(
                0,
                "web/src/app.ts",
                "import a from \"@/components/Button\";\nimport b from \"utils/format\";\n\
                 import c from \"@/styles/missing\";\nimport d from \"~lib\";\n",
            ),
            ts(1, "web/src/components/Button.tsx", ""),
            ts(2, "web/src/utils/format.ts", ""),
            ts(3, "web/vendor/lib/index.ts", ""),
            ts(
                4,
                "other/app.ts",
                "import a from \"#lib/x\";\nimport b from \"utils/format\";\n",
            ),
            ts(5, "other/lib/x.ts", ""),
            ts(6, "plain/app.ts", "import a from \"utils/format\";\n"),
        ];
        let manifests = manifests_of(
            &[
                (
                    "web/tsconfig.json",
                    "{\n  // the app\n  \"extends\": \"./tsconfig.base\",\n  \"compilerOptions\": {\n    \
                     \"paths\": { \"@/*\": [\"*\"], \"~lib\": [\"../missing\", \"../vendor/lib\"], },\n  },\n}\n",
                ),
                (
                    "web/tsconfig.base.json",
                    "{ \"compilerOptions\": { /* shared */ \"baseUrl\": \"src\" } }",
                ),
                (
                    "other/tsconfig.json",
                    "{ \"compilerOptions\": { \"paths\": { \"#lib/*\": [\"./lib/*\"] } } }",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        // `paths` resolve against the inherited `baseUrl`, `src/`, and a
        // pattern's substitutions are tried in turn.
        assert_eq!(
            edges(0),
            vec![
                "web/src/components/Button.tsx",
                "web/src/utils/format.ts",
                "web/vendor/lib/index.ts"
            ]
        );
        assert_eq!(edges(4), vec!["other/lib/x.ts"]);
        assert!(edges(6).is_empty());
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["@/styles/missing".to_string()])
        );
    }

    /// A specifier can name a directory's `index` file outright, which is the
    /// directory's module, as the language folds it.
    #[test]
    fn a_specifier_naming_an_index_file_names_its_directory_module() {
        let nodes = vec![
            node_in(
                0,
                "web/app.ts",
                "import a from \"./components/index\";\nimport b from \"./lib/index.js\";\n",
                Language::TypeScript,
            ),
            node_in(1, "web/components/index.ts", "", Language::TypeScript),
            node_in(2, "web/lib/index.ts", "", Language::TypeScript),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec!["web/components/index.ts", "web/lib/index.ts"]
        );
    }

    /// A workspace package is imported by the name its `package.json` gives
    /// it, from anywhere below the workspace root that lists it. With
    /// `exports` it exports only what that maps; without, the name stands for
    /// the file `types` or `main` names, else its `index`, and a subpath for
    /// the file below it. A listed name whose file pack cannot determine is
    /// disclosed; a package no workspace lists is no workspace's.
    #[test]
    fn a_workspace_package_resolves_by_the_name_its_package_json_gives_it() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let nodes = vec![
            ts(
                0,
                "apps/web/src/app.ts",
                "import a from \"@acme/ui\";\nimport b from \"@acme/ui/button\";\n\
                 import c from \"@acme/utils\";\nimport d from \"@acme/utils/deep/x\";\n\
                 import e from \"@acme/built\";\nimport f from \"@acme/outside\";\n\
                 import g from \"@acme/ui/hidden\";\nimport h from \"@acme/cond\";\n\
                 import i from \"@acme/typed\";\nimport j from \"@acme/twin\";\n\
                 import k from \"core-lib\";\n",
            ),
            ts(1, "packages/ui/src/index.ts", ""),
            ts(2, "packages/ui/src/button.ts", ""),
            ts(3, "packages/ui/hidden.ts", ""),
            ts(4, "packages/utils/index.ts", ""),
            ts(5, "packages/utils/deep/x.ts", ""),
            ts(6, "packages/built/src/lib.ts", ""),
            ts(7, "tools/outside/index.ts", ""),
            ts(
                8,
                "pn/app.ts",
                "import a from \"core-lib\";\nimport b from \"legacy-lib\";\n",
            ),
            ts(9, "pn/libs/core/index.ts", ""),
            ts(10, "pn/libs/legacy/index.ts", ""),
            ts(11, "packages/cond/index.ts", ""),
            ts(12, "packages/typed/index.ts", ""),
            ts(13, "packages/typed/src/entry.ts", ""),
            ts(14, "packages/twin-a/index.ts", ""),
            ts(15, "packages/twin-b/index.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"name\": \"root\", \"workspaces\": [\"packages/*\", \"apps/*\"]}",
                ),
                (
                    "packages/ui/package.json",
                    "{\"name\": \"@acme/ui\", \"exports\": {\".\": \"./src/index.ts\", \
                     \"./button\": \"./src/button.ts\"}}",
                ),
                ("packages/utils/package.json", "{\"name\": \"@acme/utils\"}"),
                (
                    "packages/built/package.json",
                    "{\"name\": \"@acme/built\", \"main\": \"dist/index.js\"}",
                ),
                ("apps/web/package.json", "{\"name\": \"web\"}"),
                (
                    "tools/outside/package.json",
                    "{\"name\": \"@acme/outside\"}",
                ),
                (
                    "packages/cond/package.json",
                    "{\"name\": \"@acme/cond\", \"exports\": {\".\": {\"import\": \"./dist/index.mjs\"}}}",
                ),
                (
                    "packages/typed/package.json",
                    "{\"name\": \"@acme/typed\", \"types\": \"./src/entry.ts\", \"main\": \"dist/index.js\"}",
                ),
                ("packages/twin-a/package.json", "{\"name\": \"@acme/twin\"}"),
                ("packages/twin-b/package.json", "{\"name\": \"@acme/twin\"}"),
                (
                    "pn/pnpm-workspace.yaml",
                    "packages:\n- 'libs/*' # the libraries\n- '!libs/legacy'\n",
                ),
                ("pn/libs/core/package.json", "{\"name\": \"core-lib\"}"),
                ("pn/libs/legacy/package.json", "{\"name\": \"legacy-lib\"}"),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(
            edges(0),
            vec![
                "packages/ui/src/index.ts",
                "packages/ui/src/button.ts",
                "packages/utils/index.ts",
                "packages/utils/deep/x.ts",
                "packages/typed/src/entry.ts"
            ]
        );
        assert_eq!(edges(8), vec!["pn/libs/core/index.ts"]);
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from([
                "@acme/built".to_string(),
                "@acme/cond".to_string(),
                "@acme/twin".to_string(),
                "@acme/ui/hidden".to_string()
            ])
        );
    }

    /// Four files can share a basename; a reference short enough to fit all
    /// of them identifies none, and inventing an edge to each would hand
    /// every one of them the same borrowed rank.
    #[test]
    fn an_ambiguous_reference_yields_no_edge() {
        let nodes = vec![
            node(0, "src/cli/output.rs", "use symbols;\n"),
            node(1, "src/services/store/symbols.rs", ""),
            node(2, "src/services/lsp/symbols.rs", ""),
        ];
        assert!(rust_edges(&nodes, 0).is_empty());

        let qualified = vec![
            node(
                0,
                "src/cli/output.rs",
                "use crate::services::store::symbols;\n",
            ),
            node(1, "src/services/store/symbols.rs", ""),
            node(2, "src/services/lsp/symbols.rs", ""),
        ];
        assert_eq!(
            rust_edges(&qualified, 0),
            vec!["src/services/store/symbols.rs"]
        );
    }

    /// A path from a name is relative to the module writing it only where the
    /// file declares that name as a module; any other names an extern crate,
    /// which no project file stands for, so nothing is left out of the graph
    /// and nothing is disclosed.
    #[test]
    fn only_a_path_through_a_declared_module_needs_module_ownership() {
        for (source, omitted) in [
            ("use anyhow::Result;\nuse std::fmt;\n", false),
            (
                "mod config { pub struct Settings; }\nuse config::Settings;\n",
                true,
            ),
            ("mod config;\n", true),
        ] {
            let nodes = vec![node(0, "src/main.rs", source)];
            let graph = build_import_graph(&nodes, &default_cargo_layout());
            assert_eq!(graph.rust_relative_imports_omitted, omitted, "{source}");
        }
    }

    /// A directory module is addressed by its directory, not by `mod`.
    #[test]
    fn a_directory_module_resolves_through_its_directory_name() {
        let nodes = vec![
            node(0, "src/app.rs", "use crate::services::lsp::LspService;\n"),
            node(1, "src/services/lsp/mod.rs", ""),
        ];
        assert_eq!(rust_edges(&nodes, 0), vec!["src/services/lsp/mod.rs"]);
    }

    #[test]
    fn rust_relative_imports_require_module_ownership() {
        for (file, source, child, decoy) in [
            (
                "src/foo.rs",
                "mod child; use self::child as dependency;",
                "src/foo/child.rs",
                "src/child.rs",
            ),
            (
                "src/custom_root.rs",
                "mod child; use self::child as dependency;",
                "src/child.rs",
                "src/custom_root/child.rs",
            ),
            (
                "src/handlers/mod.rs",
                "mod child;",
                "src/handlers/child.rs",
                "src/child.rs",
            ),
            (
                "src/lib.rs",
                "mod child;",
                "src/child.rs",
                "src/lib/child.rs",
            ),
            (
                "src/lib.rs",
                "mod outer { mod child; use self::child as dependency; use super::other; }",
                "src/outer/child.rs",
                "src/child.rs",
            ),
            (
                "src/lib.rs",
                "mod outer { use self as current; use super as parent; }",
                "src/self.rs",
                "src/super.rs",
            ),
        ] {
            let nodes = vec![
                node(
                    0,
                    file,
                    &format!("{source}\nuse crate as root;\nuse crate::known;\n"),
                ),
                node(1, child, ""),
                node(2, decoy, ""),
                node(3, "src/known.rs", ""),
                node(4, "src/crate.rs", ""),
            ];
            let graph = build_import_graph(&nodes, &default_cargo_layout());
            assert!(graph.rust_relative_imports_omitted, "{file}");
            assert_eq!(graph.edges[&0], vec![3], "{file}");
        }
    }

    /// A `crate::` path starts at the root of the crate writing it, which
    /// Cargo's layout determines for a package's library and `src/main.rs`
    /// but not for a target directory of its own; a workspace member's
    /// library is imported by the name its `Cargo.toml` gives it.
    #[test]
    fn a_rust_path_starts_where_cargo_roots_its_crate() {
        let nodes = vec![
            node(0, "crates/core/src/lib.rs", ""),
            node(1, "crates/core/src/model.rs", ""),
            node(
                2,
                "crates/cli/src/main.rs",
                "use acme_core::model::User;\nuse cli::args;\nuse crate::run;\n",
            ),
            node(3, "crates/cli/src/run.rs", ""),
            node(4, "crates/cli/lib/root.rs", ""),
            node(5, "crates/cli/lib/args.rs", "use crate::flags;\n"),
            node(6, "crates/cli/lib/flags.rs", ""),
            node(7, "crates/cli/src/bin/tool.rs", "use crate::run;\n"),
            node(8, "crates/core/tests/it.rs", "use crate::model;\n"),
            node(9, "crates/cli/src/tools/gen.rs", ""),
            node(10, "crates/cli/src/tools/emit.rs", "use crate::run;\n"),
        ];
        let manifests = manifests_of(
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
                (
                    "crates/core/Cargo.toml",
                    "[package]\nname = \"acme-core\"\n",
                ),
                (
                    "crates/cli/Cargo.toml",
                    "[package]\nname = \"acme-cli\"\n[lib]\nname = \"cli\"\npath = \"lib/root.rs\"\n\
                     [[bin]]\nname = \"gen\"\npath = \"src/tools/gen.rs\"\n",
                ),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 2, &manifests),
            vec![
                "crates/core/src/model.rs",
                "crates/cli/lib/args.rs",
                "crates/cli/src/run.rs"
            ]
        );
        assert_eq!(
            edges_under(&nodes, 5, &manifests),
            vec!["crates/cli/lib/flags.rs"]
        );
        // A binary of `src/bin/` and an integration test are crates of their
        // own, whose `crate::` paths are omitted and disclosed.
        assert!(edges_under(&nodes, 7, &manifests).is_empty());
        assert!(edges_under(&nodes, 8, &manifests).is_empty());
        // Below both `src/` and the `gen` binary's root, a file could be a
        // module of either.
        assert!(edges_under(&nodes, 10, &manifests).is_empty());
        let rooted = build_import_graph(&nodes[..7], &manifests);
        assert!(!rooted.rust_relative_imports_omitted);
        assert!(build_import_graph(&nodes, &manifests).rust_relative_imports_omitted);
    }

    #[test]
    fn import_graph_links_use_to_target() {
        let a = node(0, "src/a.rs", "use crate::b;\npub fn a() {}\n");
        let b = node(1, "src/b.rs", "pub fn b() {}\n");
        let graph = build_import_graph(&[a, b], &default_cargo_layout());
        assert!(graph.edges[&0].contains(&1));
        assert!(graph.edges[&1].is_empty());
        assert!(!graph.rust_relative_imports_omitted);
    }

    #[test]
    fn relative_imports_preserve_python_depth_and_literal_file_stems() {
        let python = vec![
            node_in(
                0,
                "pkg/a/b/caller.py",
                "from ...shared import value\n",
                Language::Python,
            ),
            node_in(1, "pkg/shared.py", "", Language::Python),
            node_in(2, "pkg/a/shared.py", "", Language::Python),
        ];
        assert_eq!(edges_of(&python, 0), vec!["pkg/shared.py"]);
        let js = vec![
            node_in(
                0,
                "web/caller.ts",
                "import {value} from './util.test.js';\n",
                Language::TypeScript,
            ),
            node_in(1, "web/util.test.ts", "", Language::TypeScript),
            node_in(2, "web/util.test.py", "", Language::Python),
        ];
        assert_eq!(edges_of(&js, 0), vec!["web/util.test.ts"]);
        let escaped = vec![
            node_in(
                0,
                "web/caller.ts",
                "import {helper} from './he\\u006cper';\n",
                Language::TypeScript,
            ),
            node_in(1, "web/helper.ts", "", Language::TypeScript),
            node_in(2, "web/he\\u006cper.ts", "", Language::TypeScript),
        ];
        assert!(edges_of(&escaped, 0).is_empty());
        for (literal, target, decoy) in [
            ("./helper'", "web/helper'.ts", "web/helper.ts"),
            ("./helper`", "web/helper`.ts", "web/helper.ts"),
            ("./ helper ", "web/ helper .ts", "web/ helper.ts"),
        ] {
            let nodes = vec![
                node_in(
                    0,
                    "web/caller.ts",
                    &format!("import \"{literal}\";\n"),
                    Language::TypeScript,
                ),
                node_in(1, target, "", Language::TypeScript),
                node_in(2, decoy, "", Language::TypeScript),
            ];
            assert_eq!(edges_of(&nodes, 0), vec![target], "{literal:?}");
        }
        let tf = vec![
            node_in(
                0,
                "main.tf",
                "module \"network\" { source = \"./network\" }\n",
                Language::Terraform,
            ),
            node_in(1, "network/main.tf", "", Language::Terraform),
            node_in(2, "network/variables.tf", "", Language::Terraform),
            // A module's source is its directory, never a file named like it.
            node_in(3, "network.tf", "", Language::Terraform),
        ];
        assert_eq!(
            edges_of(&tf, 0),
            vec!["network/main.tf", "network/variables.tf"]
        );
        for (literal, target, decoy) in [
            ("./he\\u006cper", "helper/main.tf", "he\\u006cper/main.tf"),
            ("./$${helper}", "${helper}/main.tf", "$${helper}/main.tf"),
            ("./%%{helper}", "%{helper}/main.tf", "%%{helper}/main.tf"),
        ] {
            let nodes = vec![
                node_in(
                    0,
                    "main.tf",
                    &format!("module \"helper\" {{ source = \"{literal}\" }}\n"),
                    Language::Terraform,
                ),
                node_in(1, target, "", Language::Terraform),
                node_in(2, decoy, "", Language::Terraform),
            ];
            assert!(edges_of(&nodes, 0).is_empty(), "{literal:?}");
        }
    }
}
