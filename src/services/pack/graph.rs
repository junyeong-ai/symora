//! The import graph pack ranks: each file's references, resolved to the
//! project files they name by each language's own rules.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::Node;
use super::manifests::{
    JsPackage, Manifests, ModuleResolution, Pytest, PythonProject, TsConfig, TsOwner, nearest,
    own_or_only, project_path, python_packages, python_project,
};
use crate::models::symbol::Language;
use crate::services::imports::{FileClass, ModuleDeclaration, ModulePath};

// BTreeMap, not HashMap: PageRank accumulates f64 scores by iterating this
// graph, and float addition is non-associative — a random HashMap iteration
// order would make scores differ by a ULP run-to-run, breaking pack's
// reproducibility contract. Ascending-id iteration is the canonical order.
pub(super) type Graph = BTreeMap<usize, Vec<usize>>;

pub(super) struct ImportGraph {
    pub(super) edges: Graph,
    pub(super) rust_relative_imports_omitted: bool,
    pub(super) undetermined_imports: BTreeSet<String>,
    /// The files writing a non-relative specifier whose TypeScript
    /// configuration the `references` above them leave undetermined.
    pub(super) ts_owner_undetermined: BTreeSet<String>,
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
        if let Some(name) = Path::new(&node.rel_path)
            .file_name()
            .and_then(|name| name.to_str())
        {
            let file = [node.directory.as_slice(), &[name.to_string()]].concat();
            exact.by_file.insert(file, node.id);
        }
    }
    let index = ProjectIndex {
        nodes,
        manifests,
        python: PythonPath::of(nodes, manifests),
        jvm: JvmIndex::of(nodes),
        rust: RustModules::of(nodes, manifests, &exact.by_file),
        exact,
    };

    let mut graph: Graph = BTreeMap::new();
    let mut rust_relative_imports_omitted = false;
    let mut undetermined_imports = BTreeSet::new();
    let mut ts_owner_undetermined = BTreeSet::new();
    for node in nodes {
        graph.entry(node.id).or_default();
        let importer = Importer::of(node, manifests);
        let mut seen = HashSet::new();
        for target in &node.imports.references {
            let reference = Reference::parse(target, node.language);
            if importer.ts_owner_undetermined && matches!(reference, Reference::Bare(_)) {
                ts_owner_undetermined.insert(node.rel_path.clone());
            }
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
        ts_owner_undetermined,
    }
}

/// What a reference resolves to.
enum Resolution {
    /// The files it names: none where it names nothing in the project, or
    /// nothing unambiguous.
    Files(Vec<usize>),
    /// A Rust path whose file the crates' module trees do not determine.
    NeedsOwnership,
    /// A specifier the project's own configuration declares that names no
    /// file pack reads.
    Undetermined(String),
}

/// What resolving a file's references needs to know about where the file
/// sits, worked out once for the file rather than once per reference.
struct Importer<'a> {
    node: &'a Node,
    /// The TypeScript configuration a JavaScript or TypeScript file is
    /// compiled under (`Manifests::ts_owner`).
    ts_config: Option<&'a TsConfig>,
    /// Whether the configuration above the file names no one project
    /// compiling it, so that its specifiers are read without one.
    ts_owner_undetermined: bool,
    /// The module resolution the file's imports are read under, where it is
    /// one deciding which mode-dependent `exports` conditions apply.
    mode: Option<ImportMode>,
}

#[derive(Debug, Clone, Copy)]
enum ImportMode {
    Bundler,
    /// `node16` or `nodenext`, for a file Node reads as an ES module or not.
    Node16 {
        esm: bool,
    },
}

impl<'a> Importer<'a> {
    fn of(node: &'a Node, manifests: &'a Manifests) -> Self {
        let owner = if is_javascript(node.language) {
            manifests.ts_owner(node)
        } else {
            TsOwner::None
        };
        let ts_config = match owner {
            TsOwner::Config(config) => Some(config),
            TsOwner::None | TsOwner::Undetermined => None,
        };
        let mode = ts_config
            .and_then(|config| config.module_resolution)
            .map(|resolution| match resolution {
                ModuleResolution::Bundler => ImportMode::Bundler,
                ModuleResolution::Node16 => ImportMode::Node16 {
                    esm: is_es_module(node, manifests),
                },
            });
        Self {
            node,
            ts_config,
            ts_owner_undetermined: matches!(owner, TsOwner::Undetermined),
            mode,
        }
    }

    /// Whether TypeScript matches an `exports` condition for this file's
    /// imports, `None` where its configuration leaves that open: `types`,
    /// `default` and the `customConditions` always; `import` under `bundler`
    /// or for an ES module under `node16`; `require` for any other file under
    /// `node16`; `node` under `node16`; nothing else. A versioned
    /// `types@<range>` key depends on the compiler's own version.
    fn matches(&self, condition: &str) -> Option<bool> {
        if matches!(condition, "types" | "default")
            || self
                .ts_config
                .is_some_and(|config| config.custom_conditions.iter().any(|c| c == condition))
        {
            return Some(true);
        }
        match condition {
            "import" | "require" | "node" => Some(match self.mode? {
                ImportMode::Bundler => condition == "import",
                ImportMode::Node16 { esm } => match condition {
                    "import" => esm,
                    "require" => !esm,
                    _ => true,
                },
            }),
            _ if condition.starts_with("types@") => None,
            _ => Some(false),
        }
    }
}

/// Whether Node reads a JavaScript or TypeScript file as an ES module: one
/// whose extension says so (`.mjs`, `.mts`), or one with an ordinary
/// extension below a `package.json` declaring `"type": "module"`, the nearest
/// above it deciding.
fn is_es_module(node: &Node, manifests: &Manifests) -> bool {
    match Path::new(&node.rel_path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("mjs" | "mts") => true,
        Some("cjs" | "cts") => false,
        _ => nearest(&manifests.module_types, &node.directory).is_some_and(|(_, module)| *module),
    }
}

/// What a package's `exports` target names for one importer.
#[derive(Debug, PartialEq, Eq)]
enum Exported {
    /// The files it resolves to: none where the file is only on disk.
    Files(Vec<usize>),
    /// No file: a target naming none, or no condition the importer matches.
    Nothing,
    /// Which file depends on a condition the importer's configuration leaves
    /// open.
    Undetermined,
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
/// and its manifests declare; a Rust path through its crate's module tree.
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
        let components = split_components(rest);
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
    /// Each file by its whole path, extension included.
    by_file: HashMap<Vec<String>, usize>,
}

/// The project's files by the names a reference can give them. Every lookup
/// is exact: a module path a reference only ends with is some other module's.
struct ProjectIndex<'a> {
    nodes: &'a [Node],
    exact: PathIndex<'a>,
    manifests: &'a Manifests,
    python: PythonPath<'a>,
    jvm: JvmIndex<'a>,
    rust: RustModules,
}

/// Each crate's module tree, as Rust builds it from the crate's root file
/// through the `mod` declarations its files make. A module declared without
/// a body is the file its `#[path]` attribute names, relative to the
/// directory of the module declaring it, or else `name.rs` or `name/mod.rs`
/// — in the declaring module's directory where it is a crate root, a
/// `mod.rs` or a file a `#[path]` named, and otherwise in a directory named
/// for the declaring module, the way `a.rs` holds `a/name.rs`. An inline
/// module is a directory of its own below the module declaring it, or the
/// one its `#[path]` names. A module whose file both or neither candidate
/// is, whose declarations disagree, or whose path a `cfg_attr` may set, is
/// left open. A file sits wherever each crate reaching it puts it.
#[derive(Default)]
struct RustModules {
    /// Where each file sits: each crate whose tree holds it, by the crate's
    /// root file, with the file's module path there.
    positions: HashMap<usize, Vec<(usize, Vec<String>)>>,
    /// Each crate's modules by their paths from its root.
    modules: HashMap<usize, HashMap<Vec<String>, ModuleFile>>,
}

/// What a module of a crate's tree is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleFile {
    /// The file holding it: its own, or the one declaring it inline.
    File(usize),
    /// A file pack does not read.
    Missing,
    /// A file its source leaves open.
    Undetermined,
}

impl RustModules {
    fn of(nodes: &[Node], manifests: &Manifests, files: &HashMap<Vec<String>, usize>) -> Self {
        let mut builder = TreeBuilder {
            nodes,
            files,
            tree: Self::default(),
        };
        let roots: BTreeSet<usize> = manifests
            .rust_packages
            .values()
            .flat_map(|package| &package.crates)
            .filter_map(|root| files.get(root).copied())
            .collect();
        for root in roots {
            let directory = nodes[root].directory.clone();
            builder.walk(root, root, Vec::new(), directory, None, 0);
        }
        builder.tree
    }

    /// Record what a module of a crate is; two different answers for one
    /// module leave it open.
    fn place(&mut self, krate: usize, module: Vec<String>, file: ModuleFile) {
        let modules = self.modules.entry(krate).or_default();
        let placed = modules.entry(module).or_insert(file);
        if *placed != file {
            *placed = ModuleFile::Undetermined;
        }
    }

    /// What a path below the module `base` of a crate names: the module it
    /// names whole, else the one without its last segment, which names an
    /// item there; `None` where neither is a module of the crate.
    fn lookup(&self, krate: usize, base: &[String], path: &[String]) -> Option<ModuleFile> {
        let modules = self.modules.get(&krate)?;
        let whole = [base, path].concat();
        let parent = (!path.is_empty()).then(|| &whole[..whole.len() - 1]);
        std::iter::once(whole.as_slice())
            .chain(parent)
            .find_map(|candidate| modules.get(candidate).copied())
    }
}

/// The module trees being read from the project's Rust files, which it
/// finds by their paths.
struct TreeBuilder<'a> {
    nodes: &'a [Node],
    files: &'a HashMap<Vec<String>, usize>,
    tree: RustModules,
}

impl TreeBuilder<'_> {
    /// Place `file` at `module` of the crate `krate`, then the modules it
    /// declares, `directory` being where its own declarations' files are
    /// found and `relative` the directory below it a non-`mod.rs` module
    /// file adds for them.
    fn walk(
        &mut self,
        krate: usize,
        file: usize,
        module: Vec<String>,
        directory: Vec<String>,
        relative: Option<String>,
        depth: usize,
    ) {
        // `#[path]` attributes can lead a tree back into itself.
        const DEPTH: usize = 64;
        let (nodes, files) = (self.nodes, self.files);
        self.tree
            .positions
            .entry(file)
            .or_default()
            .push((krate, module.clone()));
        self.tree
            .place(krate, module.clone(), ModuleFile::File(file));
        if depth >= DEPTH {
            return;
        }
        let declarations = &nodes[file].imports.modules;
        for declaration in declarations {
            let path = [
                module.as_slice(),
                &declaration.scope,
                std::slice::from_ref(&declaration.name),
            ]
            .concat();
            if declaration.inline {
                self.tree.place(krate, path, ModuleFile::File(file));
                continue;
            }
            let Some((base, relative)) =
                scope_directory(declarations, &declaration.scope, &directory, &relative)
            else {
                self.tree.place(krate, path, ModuleFile::Undetermined);
                continue;
            };
            let (child, directory, relative) = match &declaration.path {
                ModulePath::Undetermined => {
                    self.tree.place(krate, path, ModuleFile::Undetermined);
                    continue;
                }
                ModulePath::Attribute(written) => {
                    let Some(target) = project_path(&base, written) else {
                        self.tree.place(krate, path, ModuleFile::Missing);
                        continue;
                    };
                    let directory = target[..target.len().saturating_sub(1)].to_vec();
                    (files.get(&target).copied(), directory, None)
                }
                ModulePath::Default => {
                    let mut prefix = base;
                    prefix.extend(relative);
                    let name = &declaration.name;
                    let flat = [prefix.as_slice(), &[format!("{name}.rs")]].concat();
                    let nested =
                        [prefix.as_slice(), &[name.clone(), "mod.rs".to_string()]].concat();
                    match (files.get(&flat), files.get(&nested)) {
                        (Some(flat), None) => (Some(*flat), prefix, Some(name.clone())),
                        (None, Some(nested)) => {
                            prefix.push(name.clone());
                            (Some(*nested), prefix, None)
                        }
                        (Some(_), Some(_)) => {
                            self.tree.place(krate, path, ModuleFile::Undetermined);
                            continue;
                        }
                        (None, None) => (None, prefix, None),
                    }
                }
            };
            match child {
                Some(child) => self.walk(krate, child, path, directory, relative, depth + 1),
                None => self.tree.place(krate, path, ModuleFile::Missing),
            }
        }
    }
}

/// The directory the out-of-line modules declared in the inline modules
/// `scope` of a file are found in, and the one a non-`mod.rs` file adds
/// below it, from the file's own: each inline module adds its name, after
/// the file's added directory, or replaces it with the one its `#[path]`
/// names. `None` where an inline module's path is left open.
fn scope_directory(
    declarations: &[ModuleDeclaration],
    scope: &[String],
    directory: &[String],
    relative: &Option<String>,
) -> Option<(Vec<String>, Option<String>)> {
    let mut directory = directory.to_vec();
    let mut relative = relative.clone();
    for (depth, name) in scope.iter().enumerate() {
        let declared = declarations.iter().find(|declaration| {
            declaration.inline && declaration.scope == scope[..depth] && declaration.name == *name
        });
        match declared.map(|declaration| &declaration.path) {
            Some(ModulePath::Attribute(written)) => {
                directory = project_path(&directory, written)?;
                relative = None;
            }
            Some(ModulePath::Undetermined) => return None,
            _ => {
                directory.extend(relative.take());
                directory.push(name.clone());
            }
        }
    }
    Some((directory, relative))
}

/// What a project's JVM files declare themselves to be. Each name a file
/// declares at its top level — a class, an object, a Kotlin or Scala
/// function — is named by the package its declaration states and that name,
/// whatever the file is called and wherever it sits, and so is the class a
/// Kotlin file's top-level functions and properties compile into, which
/// Java imports them through (`kotlin_file_class`); its source root is its
/// directory without the path that package spells, where the directory ends
/// in that path, and its source set is what that root belongs to
/// (`SourceSet`).
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
            let file_class = node
                .imports
                .file_class
                .as_ref()
                .and_then(|class| match class {
                    FileClass::Named(name) => Some(name.clone()),
                    FileClass::OfFileName => kotlin_file_class(&node.rel_path),
                });
            for name in node.imports.declares.iter().chain(&file_class) {
                let class = [package.as_slice(), std::slice::from_ref(name)].concat();
                index.by_class.entry(class).or_default().push(node.id);
            }
            index.by_package.entry(package).or_default().push(node.id);
        }
        index
    }
}

/// The class Kotlin compiles a `.kt` file's top-level functions and
/// properties into where no `@file:JvmName` names one: the file's name
/// without `.kt`, each character neither a letter nor a digit read as `_`,
/// with its first letter in upper case if it is an ASCII one, and `Kt`.
fn kotlin_file_class(path: &str) -> Option<String> {
    let stem = Path::new(path).file_name()?.to_str()?.strip_suffix(".kt")?;
    let mut class: String = stem
        .chars()
        .map(|c| {
            if c.is_alphabetic() || c.is_numeric() {
                c
            } else {
                '_'
            }
        })
        .collect();
    if class.is_empty() {
        class.push('_');
    }
    if let Some(first) = class.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    class.push_str("Kt");
    Some(class)
}

/// The set of sources a JVM source root is compiled with. In the layout
/// Maven, Gradle and Kotlin Multiplatform share, `<module>/src/<set>/<language>`,
/// it is `<module>/src/<set>`, so `src/main/java` and `src/main/kotlin` are
/// one set; any other root is a set of its own.
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

    /// Whether the set holds tests, by the names the build tools give test
    /// sets: Maven's and Gradle's `test`, and a name ending in `Test` —
    /// Kotlin Multiplatform's `<target>Test` (`commonTest`, `jvmTest`),
    /// Android's `androidUnitTest` and `androidInstrumentedTest`, a Gradle
    /// test suite's `integrationTest`. Gradle's `testFixtures` is no test
    /// set: it is what tests build on.
    fn is_test(self) -> bool {
        match self {
            Self::Named { name, .. } => {
                name == "test" || (name.len() > "Test".len() && name.ends_with("Test"))
            }
            Self::Root(_) => false,
        }
    }

    /// Whether code in this set sees the classes of `other` by the build
    /// tools' defaults: its own; in one module, a `test` set those of `main`
    /// and of `testFixtures`, and `testFixtures` those of `main`, which
    /// Maven, Gradle and its `java-test-fixtures` plugin put on their
    /// classpaths.
    fn sees(self, other: SourceSet<'_>) -> bool {
        match (self, other) {
            (Self::Named { module, name }, SourceSet::Named { module: a, name: b }) => {
                module == a
                    && (name == b
                        || matches!(
                            (name, b),
                            ("test", "main" | "testFixtures") | ("testFixtures", "main")
                        ))
            }
            (Self::Root(root), SourceSet::Root(other)) => root == other,
            _ => false,
        }
    }

    /// Whether code in this set can see the classes of `other` through any
    /// build. Within a module, every set but two: a test set, from a set
    /// that holds no tests, and `testFixtures` from `main`, which
    /// `testFixtures` itself is compiled against. Across modules, any set
    /// that holds no tests, which a build can make this one depend on —
    /// another module's `testFixtures` among them, through
    /// `testFixtures(project(…))` — and a set whose role the layout does not
    /// say; never another module's test set, which is on no classpath but
    /// its own module's.
    fn may_see(self, other: SourceSet<'_>) -> bool {
        match (self, other) {
            (_, SourceSet::Root(_)) => true,
            (Self::Named { module, name }, SourceSet::Named { module: a, name: b })
                if module == a =>
            {
                (self.is_test() || !other.is_test()) && !(name == "main" && b == "testFixtures")
            }
            _ => !other.is_test(),
        }
    }
}

/// The directories a Python tree puts on `sys.path`, as its own tooling
/// establishes them: the directory above a file's top-level package (a
/// package is a directory holding `__init__.py`), those each project's
/// configuration declares or its build backend discovers, and those pytest
/// adds.
struct PythonPath<'a> {
    packages: HashSet<&'a [String]>,
    /// What each project puts on `sys.path`, by the project's directory. The
    /// project root is a project whether or not it holds a configuration
    /// file of its own.
    projects: HashMap<Vec<String>, PythonProject>,
    /// How pytest runs from each directory whose files configure it.
    pytest: &'a HashMap<Vec<String>, Pytest>,
    /// How pytest runs where no configuration file says.
    default_pytest: Pytest,
}

impl<'a> PythonPath<'a> {
    fn of(nodes: &'a [Node], manifests: &'a Manifests) -> Self {
        let mut projects = manifests.python_projects.clone();
        projects
            .entry(Vec::new())
            .or_insert_with(|| python_project(&[], None));
        Self {
            packages: python_packages(nodes),
            projects,
            pytest: &manifests.pytest_configurations,
            default_pytest: Pytest::default(),
        }
    }

    /// The directory a file in `directory` is imported from where that
    /// directory is a package: the one above its top-level package, which
    /// has to be on `sys.path` for the file to run as part of its package.
    fn package_root_of<'d>(&self, directory: &'d [String]) -> Option<&'d [String]> {
        if !self.packages.contains(directory) {
            return None;
        }
        (0..directory.len())
            .rev()
            .map(|depth| &directory[..depth])
            .find(|directory| !self.packages.contains(directory))
    }

    /// The pytest configuration a file in `directory` runs under: the
    /// nearest above it, as pytest looks for one upward from where it runs.
    fn pytest_of(&self, directory: &[String]) -> &Pytest {
        nearest(self.pytest, directory)
            .map(|(_, pytest)| pytest)
            .unwrap_or(&self.default_pytest)
    }

    /// The directories on a Python file's `sys.path` that its own project
    /// and pytest establish: the one above its top-level package; the roots
    /// of its own project and of the projects in the tree that installing it
    /// installs, its dependencies' dependencies included; the `pythonpath`
    /// of the pytest configuration it runs under; and, for a test module or
    /// `conftest.py` that configuration imports by inserting its basedir,
    /// that basedir — the first directory upward without `__init__.py`.
    fn of_file<'s>(&'s self, from: &'s Node) -> Vec<&'s [String]> {
        let package_root = self.package_root_of(&from.directory);
        let pytest = self.pytest_of(&from.directory);
        let basedir = (pytest.inserts_basedir && pytest.imports_as_test(Path::new(&from.rel_path)))
            .then(|| package_root.unwrap_or(&from.directory));
        let mut installed = Vec::new();
        let mut pending: Vec<&[String]> = nearest(&self.projects, &from.directory)
            .map(|(directory, _)| directory.as_slice())
            .into_iter()
            .collect();
        let mut seen = HashSet::new();
        while let Some(directory) = pending.pop() {
            if !seen.insert(directory) {
                continue;
            }
            if let Some(project) = self.projects.get(directory) {
                installed.push(project);
                pending.extend(project.dependencies.iter().map(Vec::as_slice));
            }
        }
        package_root
            .into_iter()
            .chain(
                installed
                    .into_iter()
                    .flat_map(|project| project.roots.iter().map(Vec::as_slice)),
            )
            .chain(pytest.pythonpath.iter().map(Vec::as_slice))
            .chain(basedir)
            .collect()
    }
}

impl ProjectIndex<'_> {
    /// What a reference names, nothing where it names something outside the
    /// project or nothing unambiguous.
    fn resolve(&self, reference: &Reference, importer: &Importer) -> Resolution {
        let from = importer.node;
        let files = match reference {
            Reference::Relative { up, components } if from.language == Language::Rust => {
                return self.rust_in_crate(from, Some(*up), components);
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
                } else if is_javascript(from.language) {
                    self.script_target(&path, from.language).unwrap_or_default()
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
            Reference::Crate(path) => return self.rust_in_crate(from, None, path),
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
    /// a workspace package's name matches that names no file is disclosed
    /// rather than dropped — except where only the catch-all `*` pattern
    /// matches it, which maps every package name too.
    fn bare_specifier(&self, specifier: &str, importer: &Importer) -> Resolution {
        let language = importer.node.language;
        let config = importer.ts_config;
        let mapped = config.and_then(|config| config.mapped(specifier));
        for path in mapped.iter().flat_map(|mapping| &mapping.paths) {
            if let Some(found) = self.script_target(path, language) {
                return Resolution::Files(found);
            }
        }
        if let Some(found) = config
            .and_then(|config| config.base_url.as_ref())
            .and_then(|base| project_path(base, specifier))
            .and_then(|path| self.script_target(&path, language))
        {
            return Resolution::Files(found);
        }
        if let Some(package) = self.workspace_package(specifier, importer) {
            return package;
        }
        match mapped {
            Some(mapping) if !mapping.catch_all => Resolution::Undetermined(specifier.to_string()),
            _ => Resolution::Files(Vec::new()),
        }
    }

    /// What a workspace package's name, or a subpath below it, stands for,
    /// or `None` where the specifier names no package of a workspace the
    /// importer is in. A package with `exports` exports only what it maps:
    /// the name stands for the file `.` maps to, a subpath for the one
    /// `./<subpath>` maps to, read through its conditions (`export_target`).
    /// Without `exports`, the name stands for the file its `types`, `typings`
    /// or `main` names, else its `index`, and a subpath for the file below
    /// the package. A package whose file pack cannot determine is disclosed:
    /// a pattern `exports` maps, a condition the importer's configuration
    /// leaves open, an entry naming no file, or a name two packages of the
    /// workspace give.
    fn workspace_package(&self, specifier: &str, importer: &Importer) -> Option<Resolution> {
        let from = importer.node;
        let scoped = specifier.starts_with('@');
        let mut segments = specifier.splitn(if scoped { 3 } else { 2 }, '/');
        let name: Vec<&str> = segments.by_ref().take(if scoped { 2 } else { 1 }).collect();
        let name = name.join("/");
        let subpath = segments.next().unwrap_or_default();
        let declares = |package: &JsPackage| {
            package.name == name && from.directory.starts_with(&package.workspace)
        };
        let undetermined = || Resolution::Undetermined(specifier.to_string());
        let Some((directory, package)) = own_or_only(&self.manifests.js_packages, from, declares)
        else {
            return self
                .manifests
                .js_packages
                .values()
                .any(declares)
                .then(undetermined);
        };
        let Some(exports) = &package.exports else {
            let entries: Vec<&str> = if subpath.is_empty() {
                package
                    .types
                    .as_deref()
                    .into_iter()
                    .chain(package.main.as_deref())
                    .chain(["index"])
                    .collect()
            } else {
                vec![subpath]
            };
            return Some(
                entries
                    .into_iter()
                    .filter_map(|entry| project_path(directory, entry))
                    .find_map(|path| self.script_target(&path, from.language))
                    .map_or_else(undetermined, Resolution::Files),
            );
        };
        let key = if subpath.is_empty() {
            ".".to_string()
        } else {
            format!("./{subpath}")
        };
        // Without subpath keys, `exports` is what `.` maps to.
        let target = match exports {
            serde_json::Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => {
                map.get(&key)
            }
            _ => (key == ".").then_some(exports),
        };
        Some(
            match target.map(|target| self.export_target(target, directory, importer)) {
                Some(Exported::Files(files)) => Resolution::Files(files),
                _ => undetermined(),
            },
        )
    }

    /// What an `exports` target names for an importer, as TypeScript reads
    /// one: a path, which has to start `./`, the file it spells; an array,
    /// what its first entry naming a file names; a condition object, what the
    /// target of the first of its keys, in their written order, that the
    /// importer matches names, where that names a file — nested objects read
    /// alike. Where a key the importer's configuration leaves open names a
    /// file that the keys after it do not, the target is undetermined.
    fn export_target(
        &self,
        target: &serde_json::Value,
        directory: &[String],
        importer: &Importer,
    ) -> Exported {
        match target {
            serde_json::Value::String(path) if path.starts_with("./") => {
                project_path(directory, path)
                    .and_then(|path| self.script_target(&path, importer.node.language))
                    .map_or(Exported::Nothing, Exported::Files)
            }
            serde_json::Value::Array(entries) => entries
                .iter()
                .map(|entry| self.export_target(entry, directory, importer))
                .find(|exported| *exported != Exported::Nothing)
                .unwrap_or(Exported::Nothing),
            serde_json::Value::Object(conditions) => {
                let conditions: Vec<_> = conditions.iter().collect();
                self.conditional_target(&conditions, directory, importer)
            }
            _ => Exported::Nothing,
        }
    }

    fn conditional_target(
        &self,
        conditions: &[(&String, &serde_json::Value)],
        directory: &[String],
        importer: &Importer,
    ) -> Exported {
        for (at, (condition, target)) in conditions.iter().enumerate() {
            let matched = importer.matches(condition);
            if matched == Some(false) {
                continue;
            }
            match self.export_target(target, directory, importer) {
                Exported::Nothing => {}
                Exported::Files(files) if matched.is_none() => {
                    let rest = self.conditional_target(&conditions[at + 1..], directory, importer);
                    return if rest == Exported::Files(files) {
                        rest
                    } else {
                        Exported::Undetermined
                    };
                }
                exported => return exported,
            }
        }
        Exported::Nothing
    }

    /// What a path a JavaScript or TypeScript specifier or configuration
    /// writes names: the script module it spells, with its extension or
    /// without, else the file it names whole, whatever its kind — an edge
    /// where pack reads that file, none where it is only on disk (an image,
    /// code pack does not read). `None` where it names no file at all.
    fn script_target(&self, written: &[String], language: Language) -> Option<Vec<usize>> {
        let mut module = written.to_vec();
        as_script_module(&mut module);
        let found = self.module(&module, language);
        if !found.is_empty() {
            return Some(found);
        }
        if let Some(file) = self.exact.by_file.get(written) {
            return Some(vec![*file]);
        }
        self.manifests.is_script_on_disk(written).then(Vec::new)
    }

    /// What a Rust path from a name, rather than from `crate`, `self` or
    /// `super`, names. A path from a module declared where it is written
    /// reads as one from `self` (`FileImports`); from a project library's
    /// name it starts at that library's root; any other name is an extern
    /// crate's, which names nothing in the project and leaves nothing out of
    /// its graph.
    fn rust_path(&self, path: &[String], from: &Node) -> Resolution {
        let Some((name, path)) = path.split_first() else {
            return Resolution::Files(Vec::new());
        };
        let root = self
            .manifests
            .rust_library(name, from)
            .and_then(|root| self.exact.by_file.get(root));
        match root.and_then(|root| self.rust.lookup(*root, &[], path)) {
            Some(ModuleFile::File(file)) => Resolution::Files(vec![file]),
            Some(ModuleFile::Undetermined) => Resolution::NeedsOwnership,
            Some(ModuleFile::Missing) | None => Resolution::Files(Vec::new()),
        }
    }

    /// What a Rust path from the module writing it, `up` modules above it
    /// (`self`, `super`), or from its crate's root (`crate`, `up` none)
    /// names: in each crate whose module tree holds the file, the module the
    /// path reaches (`RustModules::lookup`). A file no crate's tree holds,
    /// and a path through a module whose file the tree leaves open, are
    /// undetermined.
    fn rust_in_crate(&self, from: &Node, up: Option<usize>, path: &[String]) -> Resolution {
        let Some(positions) = self.rust.positions.get(&from.id) else {
            return Resolution::NeedsOwnership;
        };
        let mut files = Vec::new();
        for (krate, module) in positions {
            let base = match up {
                None => Some(&module[..0]),
                Some(up) => module.len().checked_sub(up).map(|keep| &module[..keep]),
            };
            let Some(base) = base else {
                continue;
            };
            match self.rust.lookup(*krate, base, path) {
                Some(ModuleFile::File(file)) => files.push(file),
                Some(ModuleFile::Undetermined) => return Resolution::NeedsOwnership,
                Some(ModuleFile::Missing) | None => {}
            }
        }
        files.sort_unstable();
        files.dedup();
        Resolution::Files(files)
    }

    /// The file an import of a JVM class names: the one declaring, in the
    /// package the import spells, the top-level name it spells — read whole,
    /// then without each trailing segment in turn, which nested classes and
    /// members add to what their file declares. Scala imports a package by
    /// its name as well, which names the files the package holds.
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
    /// `test` (`SourceSet::sees`). Where no set it sees holds the package,
    /// only a build file would say which set it comes from: it names the
    /// files of the one source set holding it that the importer may see
    /// (`SourceSet::may_see`) — never another module's test set — or none
    /// where several do.
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
    /// path below a directory on the importer's `sys.path` that its own
    /// project and pytest establish (`PythonPath::of_file`). Another
    /// project's roots, and the directory above some other top-level
    /// package, are on it only where an installation puts them, which pack
    /// does not read, so they name nothing. Two files on that footing name
    /// neither. A path below none of them (the standard library, an
    /// installed distribution) names nothing in the project, however its
    /// last segments match a project file: `logging` is not
    /// `app/util/logging.py`, whose name is `app.util.logging`, and a
    /// directory is on `sys.path` only where the tooling puts it there, never
    /// because a script happens to sit in it.
    fn python_module(&self, module: &[String], from: &Node) -> Vec<usize> {
        if module.is_empty() {
            return Vec::new();
        }
        let bases = self.python.of_file(from);
        Found::first([self.modules_at(
            bases.into_iter().map(|base| [base, module].concat()),
            |language| language == Language::Python,
        )])
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
        let lib = split_components("src/lib.rs");
        Manifests {
            rust_packages: HashMap::from([(
                Vec::new(),
                RustPackage {
                    library: Some(RustLibrary {
                        name: "app".to_string(),
                        root: lib.clone(),
                    }),
                    crates: vec![lib],
                },
            )]),
            ..Manifests::default()
        }
    }

    fn rust_edges(nodes: &[Node], id: usize) -> Vec<String> {
        edges_under(nodes, id, &default_cargo_layout())
    }

    /// The manifests a project holding these files declares, read from disk
    /// as a pack over `nodes` would read them, with the project directory
    /// kept for as long as they are, since resolution looks files up in it.
    fn manifests_of(files: &[(&str, &str)], nodes: &[Node]) -> ProjectManifests {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write manifest");
        }
        ProjectManifests {
            manifests: Manifests::read(dir.path(), nodes),
            _dir: dir,
        }
    }

    struct ProjectManifests {
        manifests: Manifests,
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for ProjectManifests {
        type Target = Manifests;

        fn deref(&self) -> &Manifests {
            &self.manifests
        }
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
        // Each file declares the class its name names, as Java has it.
        let declaring = |id, rel: &str, source: &str, language| {
            let class = Path::new(rel).file_stem().unwrap().to_str().unwrap();
            node_in(id, rel, &format!("{source}class {class} {{}}\n"), language)
        };
        let java = |id, rel, source| declaring(id, rel, source, Language::Java);
        let kotlin = |id, rel, source| declaring(id, rel, source, Language::Kotlin);
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

    /// A JVM file may see every source set of its own module but the test
    /// sets, from code that is no test, and `testFixtures` from `main`, and
    /// every set of another module that holds no tests: Kotlin
    /// Multiplatform's `commonMain` from `jvmMain`, `commonTest` and another
    /// module's `main`, Gradle's `testFixtures` from its module's `test` and
    /// another module's tests.
    #[test]
    fn a_jvm_file_may_see_its_modules_sets_and_other_modules_sets_without_tests() {
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let java = |id, rel, source| node_in(id, rel, source, Language::Java);
        let nodes = vec![
            kotlin(
                0,
                "shared/src/commonMain/kotlin/com/acme/core/Model.kt",
                "package com.acme.core\nclass Model\n",
            ),
            kotlin(
                1,
                "shared/src/jvmMain/kotlin/com/acme/app/A.kt",
                "package com.acme.app\nimport com.acme.core.Model\nimport com.acme.probe.Probe\n",
            ),
            kotlin(
                2,
                "shared/src/commonTest/kotlin/com/acme/core/ModelTest.kt",
                "package com.acme.core\nimport com.acme.probe.Probe\nclass ModelTest\n",
            ),
            kotlin(
                3,
                "shared/src/jvmTest/kotlin/com/acme/probe/Probe.kt",
                "package com.acme.probe\nimport com.acme.core.Model\nclass Probe\n",
            ),
            kotlin(
                4,
                "app/src/main/kotlin/com/acme/x/U.kt",
                "package com.acme.x\nimport com.acme.core.Model\nimport com.acme.probe.Probe\n\
                 import com.acme.fix.Fixture\n",
            ),
            java(
                5,
                "lib/src/main/java/com/acme/lib/Lib.java",
                "package com.acme.lib;\nimport com.acme.fix.Fixture;\nclass Lib {}\n",
            ),
            java(
                6,
                "lib/src/testFixtures/java/com/acme/fix/Fixture.java",
                "package com.acme.fix;\nimport com.acme.lib.Lib;\npublic class Fixture {}\n",
            ),
            java(
                7,
                "lib/src/test/java/com/acme/t/T1.java",
                "package com.acme.t;\nimport com.acme.fix.Fixture;\nclass T1 {}\n",
            ),
            kotlin(
                8,
                "app/src/test/kotlin/com/acme/x/UTest.kt",
                "package com.acme.x\nimport com.acme.fix.Fixture\nimport com.acme.probe.Probe\n",
            ),
        ];
        let model = "shared/src/commonMain/kotlin/com/acme/core/Model.kt";
        let fixture = "lib/src/testFixtures/java/com/acme/fix/Fixture.java";
        // Main code sees neither its module's test sets nor its fixtures.
        assert_eq!(edges_of(&nodes, 1), vec![model]);
        assert_eq!(
            edges_of(&nodes, 2),
            vec!["shared/src/jvmTest/kotlin/com/acme/probe/Probe.kt"]
        );
        assert_eq!(edges_of(&nodes, 3), vec![model]);
        assert_eq!(edges_of(&nodes, 4), vec![model, fixture]);
        assert!(edges_of(&nodes, 5).is_empty());
        assert_eq!(
            edges_of(&nodes, 6),
            vec!["lib/src/main/java/com/acme/lib/Lib.java"]
        );
        assert_eq!(edges_of(&nodes, 7), vec![fixture]);
        assert_eq!(edges_of(&nodes, 8), vec![fixture]);

        let named = |name| SourceSet::Named { module: &[], name };
        for test in [
            "test",
            "commonTest",
            "jvmTest",
            "androidUnitTest",
            "androidInstrumentedTest",
            "integrationTest",
        ] {
            assert!(named(test).is_test(), "{test}");
        }
        for set in ["main", "commonMain", "jvmMain", "testFixtures", "Test"] {
            assert!(!named(set).is_test(), "{set}");
        }
    }

    /// A JVM import names a file by what it declares at its top level, not by
    /// the file's own name: a Kotlin file holds several classes, functions
    /// and properties, imported by their names, and a file named like a
    /// class it does not declare is not that class.
    #[test]
    fn a_jvm_import_names_the_file_declaring_its_top_level_name() {
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let nodes = vec![
            kotlin(
                0,
                "src/main/kotlin/com/acme/app/Main.kt",
                "package com.acme.app\nimport com.acme.model.Account\nimport com.acme.util.format\n\
                 import com.acme.util.Strings\nimport com.acme.util.DEFAULT\n",
            ),
            kotlin(
                1,
                "src/main/kotlin/com/acme/model/Models.kt",
                "package com.acme.model\nclass User\nclass Account\n",
            ),
            kotlin(
                2,
                "src/main/kotlin/com/acme/util/Text.kt",
                "package com.acme.util\nfun format() = 1\nval DEFAULT = 1\n",
            ),
            kotlin(
                3,
                "src/main/kotlin/com/acme/util/Strings.kt",
                "package com.acme.util\nobject StringUtils\n",
            ),
            node_in(
                4,
                "src/main/java/com/acme/j/J.java",
                "package com.acme.j;\nimport com.acme.model.User;\nclass J {}\n",
                Language::Java,
            ),
            node_in(
                5,
                "src/main/scala/com/acme/s/Defs.scala",
                "package com.acme.s\ndef helper = 1\nobject Registry\n",
                Language::Scala,
            ),
            node_in(
                6,
                "src/main/scala/com/acme/t/Use.scala",
                "package com.acme.t\nimport com.acme.s.helper\nimport com.acme.s.Registry\n",
                Language::Scala,
            ),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec![
                "src/main/kotlin/com/acme/model/Models.kt",
                "src/main/kotlin/com/acme/util/Text.kt"
            ]
        );
        assert_eq!(
            edges_of(&nodes, 4),
            vec!["src/main/kotlin/com/acme/model/Models.kt"]
        );
        assert_eq!(
            edges_of(&nodes, 6),
            vec!["src/main/scala/com/acme/s/Defs.scala"]
        );
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
            python(18, "x/lone/__init__.py", ""),
            python(19, "x/lone/m.py", ""),
            python(20, "d/run.py", "import lone.m\n"),
        ];
        assert_eq!(
            edges_of(&nodes, 1),
            vec!["app/util/logging.py", "app/util/__init__.py"]
        );
        // `src/` is on `sys.path` by the src layout, and pytest puts a test
        // module's own directory there.
        assert_eq!(
            edges_of(&nodes, 6),
            vec!["src/pkg/core.py", "tests/conftest.py"]
        );
        assert!(edges_of(&nodes, 8).is_empty());
        // A namespace package needs no `__init__.py` below a root, and `src/`
        // is one by the src layout even where no package says so.
        assert_eq!(edges_of(&nodes, 10), vec!["src/ns/core.py"]);
        let namespace = vec![
            python(0, "src/app/a.py", "import app.core\n"),
            python(1, "src/app/core.py", ""),
        ];
        assert_eq!(edges_of(&namespace, 0), vec!["src/app/core.py"]);
        // An importer inside a top-level package imports it from the
        // directory above it; one outside every `app` has none of those
        // directories on its `sys.path`.
        assert_eq!(edges_of(&nodes, 16), vec!["a/app/db.py"]);
        assert!(edges_of(&nodes, 17).is_empty() && edges_of(&nodes, 20).is_empty());
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
            python(
                5,
                "poetry/source/pkg/a.py",
                "import pkg.b\nimport helpers\nimport secrets\n",
            ),
            python(6, "poetry/source/pkg/b.py", ""),
            python(7, "cfg/lib/mod/a.py", "import mod.b\nimport shared\n"),
            python(8, "cfg/lib/mod/b.py", ""),
            python(9, "cfg/tools/shared.py", ""),
            python(10, "tools/secrets.py", ""),
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
                ("tools/pyproject.toml", "[project]\nname = \"tools\"\n"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 1, &manifests),
            vec!["svc/code/app/core.py"]
        );
        // Another project's roots are on no `sys.path` of this one: `secrets`
        // is the standard library's, `helpers` the other project's own.
        assert_eq!(
            edges_under(&nodes, 5, &manifests),
            vec!["poetry/source/pkg/b.py"]
        );
        assert_eq!(
            edges_under(&nodes, 4, &manifests),
            vec!["svc/checks/helpers.py", "svc/code/app/core.py"]
        );
        assert_eq!(
            edges_under(&nodes, 7, &manifests),
            vec!["cfg/lib/mod/b.py", "cfg/tools/shared.py"]
        );
    }

    /// A Python project is a directory whose files configure a build — a
    /// `setup.py`, a `pyproject.toml` or `setup.cfg` with more than pytest's
    /// table — and that is no package: a package's `setup.py` is one of its
    /// modules. A file that only configures pytest (`pytest.ini`,
    /// `tox.ini`, a `pyproject.toml` or `setup.cfg` holding only pytest's
    /// table) sets how pytest runs below it, and the files there stay the
    /// enclosing project's.
    #[test]
    fn a_python_project_is_a_directory_whose_files_configure_a_build() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "src/app/__init__.py", ""),
            python(1, "src/app/setup.py", "import logging\n"),
            python(2, "src/app/a.py", "import core_pkg.m\n"),
            python(3, "core/src/core_pkg/__init__.py", ""),
            python(4, "core/src/core_pkg/m.py", ""),
            python(5, "tests/test_a.py", "import core_pkg.m\nimport helper\n"),
            python(6, "tests/helpers_dir/helper.py", ""),
            python(7, "checks/test_b.py", "import core_pkg.m\nimport aid\n"),
            python(8, "checks/support/aid.py", ""),
            python(9, "more/test_c.py", "import core_pkg.m\nimport bit\n"),
            python(10, "more/extra/bit.py", ""),
            python(11, "side/run.py", "import core_pkg.m\nimport sidekick\n"),
            python(12, "side/sidekick.py", ""),
        ];
        let core = "core/src/core_pkg/m.py";
        let manifests = manifests_of(
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"root\"\ndependencies = [\"core\"]\n\
                     [tool.uv.sources]\ncore = { path = \"core\" }\n",
                ),
                ("core/pyproject.toml", "[project]\nname = \"core\"\n"),
                ("src/app/setup.py", "import logging\n"),
                ("tests/pytest.ini", "[pytest]\npythonpath = helpers_dir\n"),
                (
                    "checks/pyproject.toml",
                    "[tool.pytest.ini_options]\npythonpath = [\"support\"]\n",
                ),
                ("more/setup.cfg", "[tool:pytest]\npythonpath = extra\n"),
                (
                    "side/setup.cfg",
                    "[metadata]\nname = side\n\n[tool:pytest]\naddopts = -q\n",
                ),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(edges(2), vec![core]);
        assert_eq!(edges(5), vec![core, "tests/helpers_dir/helper.py"]);
        assert_eq!(edges(7), vec![core, "checks/support/aid.py"]);
        assert_eq!(edges(9), vec![core, "more/extra/bit.py"]);
        // A `setup.cfg` with build metadata makes a project of its own, which
        // declares no dependency.
        assert_eq!(edges(11), vec!["side/sidekick.py"]);
    }

    /// pytest's default import mode puts on `sys.path` the basedir of each
    /// test module and `conftest.py` it imports — the first directory upward
    /// without `__init__.py` — so a test imports the helpers beside it; a
    /// module that is neither gets no such directory. The configuration
    /// pytest reads, the nearest above the file that configures it, names
    /// test modules by `python_files` and adds its `pythonpath`; its
    /// `importlib` mode inserts no basedir.
    #[test]
    fn a_test_module_imports_from_the_basedir_pytest_puts_on_sys_path() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "tests/test_a.py", "import factories\nimport app.core\n"),
            python(1, "tests/factories.py", ""),
            python(2, "tests/helpers.py", "import factories\n"),
            python(3, "tests/conftest.py", "import factories\n"),
            python(4, "src/app/__init__.py", ""),
            python(5, "src/app/core.py", ""),
            python(
                6,
                "outer/inner/t/spec_one.py",
                "import near\nimport common\n",
            ),
            python(7, "outer/inner/t/test_two.py", "import near\n"),
            python(8, "outer/inner/t/near.py", ""),
            python(9, "outer/shared/common.py", ""),
            python(10, "iso/tests/test_z.py", "import factories\n"),
            python(11, "iso/tests/factories.py", ""),
            python(12, "outer/inner/t/check_x.py", "import near\n"),
            python(13, "iso2/tests/test_z.py", "import factories\n"),
            python(14, "iso2/tests/factories.py", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "outer/pytest.ini",
                    "[pytest]\npython_files =\n    spec_*.py\n    t/check_*.py\npythonpath = shared\n",
                ),
                (
                    "outer/inner/pyproject.toml",
                    "[project]\nname = \"inner\"\n",
                ),
                (
                    "iso/pyproject.toml",
                    "[tool.pytest.ini_options]\naddopts = [\"-q\", \"--import-mode\", \"importlib\"]\n",
                ),
                (
                    "iso2/pytest.ini",
                    "[pytest]\naddopts = -q --import-mode=importlib\n",
                ),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(edges(0), vec!["tests/factories.py", "src/app/core.py"]);
        assert!(edges(2).is_empty());
        assert_eq!(edges(3), vec!["tests/factories.py"]);
        assert_eq!(
            edges(6),
            vec!["outer/inner/t/near.py", "outer/shared/common.py"]
        );
        assert_eq!(edges(12), vec!["outer/inner/t/near.py"]);
        assert!(edges(7).is_empty() && edges(10).is_empty() && edges(13).is_empty());
    }

    /// Installing a project installs the projects in the tree it depends on
    /// through its tooling — a uv workspace member its sources mark
    /// `{ workspace = true }`, a uv or Poetry `path` dependency on a directory
    /// any build configuration makes a project — and theirs, so their
    /// packages are on its `sys.path`; a member nothing it installs depends
    /// on is not, nor a directory that is no project.
    #[test]
    fn a_python_project_imports_the_projects_its_dependencies_install() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "packages/app/src/fixture_app/__init__.py", ""),
            python(
                1,
                "packages/app/src/fixture_app/report.py",
                "from fixture_core.geometry import area\nimport fixture_util\nimport other_pkg\n\
                 import legacy_pkg.thing\nimport loose_pkg\n",
            ),
            python(2, "packages/core/src/fixture_core/__init__.py", ""),
            python(
                3,
                "packages/core/src/fixture_core/geometry.py",
                "import fixture_util.text\nimport fixture_app\n",
            ),
            python(4, "libs/util/fixture_util/__init__.py", ""),
            python(5, "libs/util/fixture_util/text.py", ""),
            python(6, "packages/other/src/other_pkg/__init__.py", ""),
            python(7, "poetry/app/main.py", "import poetry_lib\n"),
            python(8, "poetry/lib/poetry_lib/__init__.py", ""),
            // A project of the same name that is no workspace member.
            python(9, "vendor/core/src/fixture_core/geometry.py", ""),
            python(10, "legacy/legacy_pkg/__init__.py", ""),
            python(11, "legacy/legacy_pkg/thing.py", ""),
            python(12, "loose/loose_pkg/__init__.py", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"root\"\n[tool.uv.workspace]\nmembers = [\"packages/*\"]\n\
                     [tool.uv.sources]\nfixture-core = { workspace = true }\n\
                     other_pkg = { workspace = true }\n",
                ),
                (
                    "packages/app/pyproject.toml",
                    "[project]\nname = \"fixture-app\"\n\
                     dependencies = [\"Fixture_Core>=1.0\", \"legacy\", \"loose\"]\n\
                     [tool.uv.sources]\nlegacy = { path = \"../../legacy\" }\n\
                     loose = { path = \"../../loose\" }\n",
                ),
                (
                    "packages/core/pyproject.toml",
                    "[project]\nname = \"fixture.core\"\ndependencies = [\"fixture-util\"]\n\
                     [tool.uv.sources]\nfixture-util = { path = \"../../libs/util\" }\n",
                ),
                (
                    "packages/other/pyproject.toml",
                    "[project]\nname = \"other-pkg\"\n",
                ),
                (
                    "libs/util/pyproject.toml",
                    "[project]\nname = \"fixture-util\"\n",
                ),
                (
                    "poetry/app/pyproject.toml",
                    "[tool.poetry.dependencies]\npython = \"^3.12\"\n\
                     poetry-lib = { path = \"../lib\", develop = true }\n",
                ),
                (
                    "poetry/lib/pyproject.toml",
                    "[tool.poetry]\nname = \"poetry-lib\"\n",
                ),
                (
                    "vendor/core/pyproject.toml",
                    "[project]\nname = \"fixture-core\"\n",
                ),
                // A project by its `setup.py` alone; a pytest file makes none.
                ("legacy/setup.py", "from setuptools import setup\n"),
                ("loose/pytest.ini", "[pytest]\n"),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(
            edges(1),
            vec![
                "packages/core/src/fixture_core/geometry.py",
                "libs/util/fixture_util/__init__.py",
                "legacy/legacy_pkg/thing.py"
            ]
        );
        assert_eq!(edges(3), vec!["libs/util/fixture_util/text.py"]);
        assert_eq!(edges(7), vec!["poetry/lib/poetry_lib/__init__.py"]);
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
                 import c from \"@/styles/missing\";\nimport d from \"~lib\";\n\
                 import \"@/styles/app.css\";\nimport logo from \"@/assets/logo.svg\";\n\
                 import tokens from \"@/generated/tokens\";\nimport \"./theme.css\";\n",
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
            node_in(7, "web/src/styles/app.css", "", Language::Css),
            node_in(8, "web/src/theme.css", "", Language::Css),
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
                // Files on disk pack does not read: an asset, generated code.
                ("web/src/assets/logo.svg", "<svg/>"),
                ("web/src/generated/tokens.ts", ""),
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
        // pattern's substitutions are tried in turn. A file of any kind the
        // specifier names is what it resolves to: an edge where pack reads
        // it, none and nothing disclosed where it is only on disk.
        assert_eq!(
            edges(0),
            vec![
                "web/src/components/Button.tsx",
                "web/src/utils/format.ts",
                "web/vendor/lib/index.ts",
                "web/src/styles/app.css",
                "web/src/theme.css"
            ]
        );
        assert_eq!(edges(4), vec!["other/lib/x.ts"]);
        assert!(edges(6).is_empty());
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["@/styles/missing".to_string()])
        );
    }

    /// A configuration with `references` that does not compile a file itself
    /// — a Vite root, `"files": []` — leaves it to the referenced project
    /// whose `files`, `include` and `exclude` take it, as `tsc -b` builds
    /// them; a reference names a configuration file or a directory's
    /// `tsconfig.json`. Projects taking one file that say the same about
    /// specifiers agree; ones that differ, or none taking it, leave the file
    /// without a configuration, and that is disclosed.
    #[test]
    fn a_file_below_project_references_is_compiled_under_the_project_including_it() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let button = "import { Button } from \"@/components/Button\";\n";
        let nodes = vec![
            ts(0, "src/pages/a.tsx", button),
            ts(1, "src/components/Button.tsx", ""),
            ts(
                2,
                "src/a.test.ts",
                &format!("import user from \"#fixtures/user\";\n{button}"),
            ),
            ts(3, "fixtures/user.ts", ""),
            ts(4, "vite.config.ts", "import cfg from \"~cfg\";\n"),
            ts(5, "cfg.ts", ""),
            ts(6, "scripts/gen.ts", button),
            ts(7, "src/lint/rule.ts", button),
            ts(8, "src/test-utils/render.tsx", button),
            // A configuration without `references` is that of every file
            // below it.
            ts(9, "plain/tools/x.ts", "import y from \"#y\";\n"),
            ts(10, "plain/y.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "tsconfig.json",
                    "{\"files\": [], \"references\": [{\"path\": \"./tsconfig.app.json\"}, \
                     {\"path\": \"./tsconfig.test.json\"}, {\"path\": \"./tsconfig.lint.json\"}, \
                     {\"path\": \"./tools\"}]}",
                ),
                (
                    "tsconfig.app.json",
                    "{\"compilerOptions\": {\"paths\": {\"@/*\": [\"./src/*\"]}}, \
                     \"include\": [\"src\"], \"exclude\": [\"src/**/*.test.ts\"]}",
                ),
                (
                    "tsconfig.test.json",
                    "{\"compilerOptions\": {\"paths\": {\"@/*\": [\"./src/*\"], \
                     \"#fixtures/*\": [\"./fixtures/*\"]}}, \
                     \"include\": [\"src/**/*.test.ts\", \"src/test-utils\"]}",
                ),
                (
                    "tsconfig.lint.json",
                    "{\"extends\": \"./tsconfig.app.json\", \"include\": [\"src/lint\"], \
                     \"exclude\": []}",
                ),
                (
                    "tools/tsconfig.json",
                    "{\"compilerOptions\": {\"paths\": {\"~cfg\": [\"../cfg.ts\"]}}, \
                     \"files\": [\"../vite.config.ts\"]}",
                ),
                (
                    "plain/tsconfig.json",
                    "{\"compilerOptions\": {\"paths\": {\"#y\": [\"./y.ts\"]}}, \
                     \"include\": [\"src\"]}",
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
        assert_eq!(edges(0), vec!["src/components/Button.tsx"]);
        assert_eq!(
            edges(2),
            vec!["fixtures/user.ts", "src/components/Button.tsx"]
        );
        assert_eq!(edges(4), vec!["cfg.ts"]);
        assert_eq!(edges(7), vec!["src/components/Button.tsx"]);
        assert_eq!(edges(9), vec!["plain/y.ts"]);
        assert!(edges(6).is_empty() && edges(8).is_empty());
        assert_eq!(
            graph.ts_owner_undetermined,
            BTreeSet::from([
                "scripts/gen.ts".to_string(),
                "src/test-utils/render.tsx".to_string()
            ])
        );
    }

    /// Every specifier matches the catch-all pattern `*`, and TypeScript looks
    /// one it maps to no file up as a package, so that one is a package's,
    /// not an alias naming nothing.
    #[test]
    fn a_specifier_only_the_catch_all_pattern_maps_is_a_package_where_it_names_no_file() {
        let nodes = vec![
            node_in(
                0,
                "src/app.ts",
                "import React from \"react\";\nimport type { T } from \"shared\";\n\
                 import x from \"@/missing\";\n",
                Language::TypeScript,
            ),
            node_in(1, "src/types/shared.ts", "", Language::TypeScript),
        ];
        let manifests = manifests_of(
            &[(
                "tsconfig.json",
                "{ \"compilerOptions\": { \"paths\": { \"*\": [\"src/types/*\"], \"@/*\": [\"src/*\"] } } }",
            )],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert_eq!(graph.edges[&0], vec![1]);
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["@/missing".to_string()])
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
                 import k from \"core-lib\";\nimport \"@acme/ui/styles.css\";\n\
                 import icon from \"@acme/ui/icon.svg\";\n",
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
            node_in(16, "packages/ui/src/styles.css", "", Language::Css),
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
                     \"./button\": \"./src/button.ts\", \"./styles.css\": \"./src/styles.css\", \
                     \"./icon.svg\": \"./assets/icon.svg\"}}",
                ),
                ("packages/ui/assets/icon.svg", "<svg/>"),
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
                "packages/typed/src/entry.ts",
                "packages/ui/src/styles.css"
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

    /// A package's `exports` maps by conditions, read in their written order
    /// as TypeScript matches them: `types`, `default` and the configured
    /// `customConditions` always, `import` under `bundler` or for an ES
    /// module under `node16`, `require` for any other file under `node16`,
    /// `node` outside `bundler`. A key the importer's configuration leaves
    /// open decides nothing unless the keys after it name the same file.
    #[test]
    fn a_conditional_export_names_the_file_of_the_conditions_typescript_matches() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let imports = "import a from \"@acme/typed\";\nimport b from \"@acme/modes\";\n\
                       import c from \"@acme/nested\";\nimport d from \"@acme/same\";\n\
                       import e from \"@acme/bare\";\n";
        let nodes = vec![
            ts(0, "bundler/app.ts", imports),
            ts(1, "esm/app.ts", imports),
            ts(2, "cjs/app.ts", imports),
            ts(3, "plain/app.ts", imports),
            ts(4, "packages/typed/src/index.ts", ""),
            ts(5, "packages/modes/src/esm.ts", ""),
            ts(6, "packages/modes/src/cjs.ts", ""),
            ts(7, "packages/nested/src/node.ts", ""),
            ts(8, "packages/nested/src/other.ts", ""),
            ts(9, "packages/same/src/index.ts", ""),
            ts(10, "packages/same/src/source.ts", ""),
            ts(11, "packages/bare/src/index.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                ("package.json", "{\"workspaces\": [\"packages/*\"]}"),
                (
                    "packages/typed/package.json",
                    "{\"name\": \"@acme/typed\", \"exports\": {\"types\": \"./src/index.ts\", \
                     \"import\": \"./dist/index.mjs\"}}",
                ),
                (
                    "packages/modes/package.json",
                    "{\"name\": \"@acme/modes\", \"exports\": {\".\": {\"import\": \"./src/esm.ts\", \
                     \"require\": \"./src/cjs.ts\"}}}",
                ),
                (
                    "packages/nested/package.json",
                    "{\"name\": \"@acme/nested\", \"exports\": {\".\": {\"node\": {\"import\": \
                     \"./src/node.ts\"}, \"default\": \"./src/other.ts\"}}}",
                ),
                (
                    "packages/same/package.json",
                    "{\"name\": \"@acme/same\", \"exports\": {\".\": {\"source\": \"./src/source.ts\", \
                     \"import\": \"./src/index.ts\", \"default\": \"./src/index.ts\"}}}",
                ),
                // A target has to start `./`.
                (
                    "packages/bare/package.json",
                    "{\"name\": \"@acme/bare\", \"exports\": \"src/index.ts\"}",
                ),
                (
                    "bundler/tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"Bundler\", \
                     \"customConditions\": [\"source\"]}}",
                ),
                (
                    "esm/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"NodeNext\"}}",
                ),
                ("esm/package.json", "{\"type\": \"module\"}"),
                (
                    "cjs/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"node16\", \"moduleResolution\": \"node16\"}}",
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
        assert_eq!(
            edges(0),
            vec![
                "packages/typed/src/index.ts",
                "packages/modes/src/esm.ts",
                "packages/nested/src/other.ts",
                "packages/same/src/source.ts"
            ]
        );
        assert_eq!(
            edges(1),
            vec![
                "packages/typed/src/index.ts",
                "packages/modes/src/esm.ts",
                "packages/nested/src/node.ts",
                "packages/same/src/index.ts"
            ]
        );
        assert_eq!(
            edges(2),
            vec![
                "packages/typed/src/index.ts",
                "packages/modes/src/cjs.ts",
                "packages/nested/src/other.ts",
                "packages/same/src/index.ts"
            ]
        );
        // Without a configuration saying which of `import` and `require`
        // applies, only what every reading agrees on is resolved.
        assert_eq!(
            edges(3),
            vec!["packages/typed/src/index.ts", "packages/same/src/index.ts"]
        );
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from([
                "@acme/bare".to_string(),
                "@acme/modes".to_string(),
                "@acme/nested".to_string()
            ])
        );
    }

    /// Java imports a Kotlin file's top-level functions and properties
    /// through the class they compile into: the one its `@file:JvmName`
    /// names, or else the file's name with `Kt` — `Strings.kt` holds
    /// `StringsKt`, `date-utils.kt` `Date_utilsKt` — and a file declaring
    /// none holds no such class.
    #[test]
    fn a_java_import_of_a_kotlin_file_class_names_that_file() {
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let nodes = vec![
            node_in(
                0,
                "src/main/java/com/acme/app/App.java",
                "package com.acme.app;\nimport com.acme.util.StringsKt;\n\
                 import static com.acme.util.Utils.helper;\nimport com.acme.util.Date_utilsKt;\n\
                 import com.acme.util.ModelsKt;\nclass App {}\n",
                Language::Java,
            ),
            kotlin(
                1,
                "src/main/kotlin/com/acme/util/Strings.kt",
                "package com.acme.util\nfun format() = 1\n",
            ),
            kotlin(
                2,
                "src/main/kotlin/com/acme/util/Helpers.kt",
                "@file:JvmName(\"Utils\")\npackage com.acme.util\nfun helper() = 1\n",
            ),
            kotlin(
                3,
                "src/main/kotlin/com/acme/util/date-utils.kt",
                "package com.acme.util\nval today = 1\n",
            ),
            kotlin(
                4,
                "src/main/kotlin/com/acme/util/Models.kt",
                "package com.acme.util\nclass Model\n",
            ),
            kotlin(
                5,
                "src/main/kotlin/com/acme/util/Utils.kt",
                "package com.acme.util\nclass Other\n",
            ),
        ];
        assert_eq!(
            edges_of(&nodes, 0),
            vec![
                "src/main/kotlin/com/acme/util/Strings.kt",
                "src/main/kotlin/com/acme/util/Helpers.kt",
                "src/main/kotlin/com/acme/util/date-utils.kt"
            ]
        );
    }

    /// A `crate::` path names the module the crate's tree puts at that path,
    /// not a file whose path merely ends the same way, and a path naming an
    /// item names the module holding it.
    #[test]
    fn a_rust_path_names_the_module_its_crate_tree_holds() {
        let nodes = vec![
            node(0, "src/lib.rs", "mod cli;\nmod services;\n"),
            node(1, "src/cli/mod.rs", "mod output;\n"),
            node(
                2,
                "src/cli/output.rs",
                "use crate::services::store::symbols;\nuse crate::services::lsp::Client;\n\
                 use symbols;\n",
            ),
            node(3, "src/services/mod.rs", "mod store;\nmod lsp;\n"),
            node(4, "src/services/store/mod.rs", "mod symbols;\n"),
            node(5, "src/services/store/symbols.rs", ""),
            node(6, "src/services/lsp/mod.rs", ""),
            node(7, "src/cli/services/store/symbols.rs", ""),
            node(8, "src/symbols.rs", ""),
        ];
        assert_eq!(
            rust_edges(&nodes, 2),
            vec!["src/services/store/symbols.rs", "src/services/lsp/mod.rs"]
        );
        // No `Cargo.toml` governs the files, so no crate holds them.
        let graph = build_import_graph(&nodes, &Manifests::default());
        assert!(graph.edges[&2].is_empty() && graph.rust_relative_imports_omitted);
    }

    /// The failure a bare-name index cannot avoid: an external crate whose
    /// segments happen to match a project file's name. `std::process` and
    /// `serde::Serialize` name nothing in this project, so they contribute
    /// no edge, while the project's own library is found by its name.
    #[test]
    fn an_external_reference_never_lands_on_a_same_named_project_file() {
        let nodes = vec![
            node(0, "src/lib.rs", "pub mod services;\nmod cli;\n"),
            node(
                1,
                "src/cli.rs",
                "use std::process::Command;\nuse serde::Serialize;\nuse log::info;\n",
            ),
            node(2, "src/services/dist/process.rs", ""),
            node(3, "src/util/serde.rs", ""),
            node(4, "src/log.rs", ""),
            node(5, "tests/cli.rs", "use app::services::pack;\n"),
            node(6, "src/services/mod.rs", "pub mod pack;\n"),
            node(7, "src/services/pack.rs", ""),
        ];
        let graph = build_import_graph(&nodes, &default_cargo_layout());
        assert!(graph.edges[&1].is_empty());
        assert_eq!(graph.edges[&5], vec![7]);
        assert!(!graph.rust_relative_imports_omitted);
    }

    /// Rust builds a crate's module tree from its root file through its
    /// `mod` declarations: a crate root, a `mod.rs` and a file a `#[path]`
    /// names hold their modules' files beside them, any other module file
    /// in a directory named for it; an inline module is a directory below
    /// the module declaring it, or the one its `#[path]` names; a `#[path]`
    /// on a module is relative to the declaring module's directory. `self::`,
    /// `super::`, `crate::`, and a path from a module declared where it is
    /// written, resolve through that tree.
    #[test]
    fn a_rust_module_path_resolves_through_the_crate_module_tree() {
        let nodes = vec![
            node(
                0,
                "src/lib.rs",
                "mod a;\nmod b;\npub mod c {\n    mod d;\n    use super::a::X;\n}\n\
                 #[path = \"x/y.rs\"]\nmod weird;\nmod gone;\n#[path = \"p\"]\nmod q {\n    mod r;\n}\n",
            ),
            node(
                1,
                "src/a.rs",
                "mod inner;\nmod m {\n    mod n;\n}\nmod deep;\n#[path = \"pa.rs\"]\nmod pa;\n\
                 use self::inner::T;\nuse super::b;\nuse crate::c::d::Z;\n",
            ),
            node(
                2,
                "src/a/inner.rs",
                "use super::super::b::Y;\nuse crate as root;\n",
            ),
            node(
                3,
                "src/b/mod.rs",
                "mod e;\nuse e::Thing;\nuse self::e as _;\n",
            ),
            node(4, "src/b/e.rs", ""),
            node(5, "src/c/d.rs", ""),
            node(6, "src/x/y.rs", "mod z;\n"),
            node(7, "src/x/z.rs", ""),
            node(8, "src/a/m/n.rs", ""),
            node(9, "src/p/r.rs", ""),
            // Where no module is: `inner` of `a.rs` is `src/a/inner.rs`.
            node(10, "src/inner.rs", ""),
            node(11, "src/d.rs", ""),
            node(12, "src/m/n.rs", ""),
            node(13, "src/q/r.rs", ""),
            node(14, "src/a/deep/mod.rs", ""),
            node(15, "src/pa.rs", ""),
            node(16, "src/a/pa.rs", ""),
        ];
        let graph = build_import_graph(&nodes, &default_cargo_layout());
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(
            edges(0),
            vec![
                "src/a.rs",
                "src/b/mod.rs",
                "src/c/d.rs",
                "src/x/y.rs",
                "src/p/r.rs"
            ]
        );
        assert_eq!(
            edges(1),
            vec![
                "src/a/inner.rs",
                "src/a/m/n.rs",
                "src/a/deep/mod.rs",
                "src/pa.rs",
                "src/b/mod.rs",
                "src/c/d.rs"
            ]
        );
        assert_eq!(edges(2), vec!["src/b/mod.rs", "src/lib.rs"]);
        assert_eq!(edges(3), vec!["src/b/e.rs"]);
        assert_eq!(edges(6), vec!["src/x/z.rs"]);
        assert!(!graph.rust_relative_imports_omitted);
    }

    /// What the module tree leaves open is omitted and disclosed: a module
    /// both `x.rs` and `x/mod.rs` could be, one whose path a `cfg_attr` may
    /// set or whose `#[path]` string carries an escape, one two declarations
    /// put in different files, and a path from a file no crate's `mod`
    /// declarations reach.
    #[test]
    fn a_rust_path_the_module_tree_leaves_open_is_omitted_and_disclosed() {
        let orphan = "use super::a;\nuse crate::a::B;\n";
        for (source, other, uses, candidate) in [
            (
                "#[cfg(unix)]\nmod sys;\n#[cfg(not(unix))]\n#[path = \"win.rs\"]\nmod sys;\n",
                "src/sys.rs",
                "",
                "src/win.rs",
            ),
            (
                "mod both;\nuse both::X;\n",
                "src/both.rs",
                "",
                "src/both/mod.rs",
            ),
            (
                "#[cfg_attr(unix, path = \"u.rs\")]\nmod cond;\n",
                "src/cond.rs",
                "",
                "src/u.rs",
            ),
            (
                "#[path = \"\\x61.rs\"]\nmod esc;\n",
                "src/a.rs",
                "",
                "src/esc.rs",
            ),
            ("", "src/orphan.rs", orphan, "src/a.rs"),
        ] {
            let nodes = vec![
                node(0, "src/lib.rs", source),
                node(1, other, uses),
                node(2, candidate, ""),
            ];
            let graph = build_import_graph(&nodes, &default_cargo_layout());
            assert!(graph.rust_relative_imports_omitted, "{source}");
            assert!(graph.edges[&0].is_empty(), "{source}");
        }
    }

    /// Every crate a package builds has a module tree of its own: binaries,
    /// integration tests and examples are crates whose `crate::` paths start
    /// at their own root, and a module file two crates declare is, in each,
    /// what that crate's tree makes it, so its paths name what each reaches.
    #[test]
    fn every_crate_a_package_builds_has_its_own_module_tree() {
        let nodes = vec![
            node(0, "src/lib.rs", "pub mod util;\n"),
            node(1, "src/main.rs", "mod util;\nmod config;\nuse app::util;\n"),
            node(2, "src/util.rs", "use crate::config::Settings;\n"),
            node(3, "src/config.rs", ""),
            node(
                4,
                "tests/it.rs",
                "mod common;\nmod fixture;\nuse app::util::helper;\n",
            ),
            node(5, "tests/common/mod.rs", "use super::fixture;\n"),
            node(6, "tests/fixture.rs", ""),
            node(
                7,
                "src/bin/tool.rs",
                "mod helpers;\nuse crate::helpers::x;\n",
            ),
            node(8, "src/bin/helpers.rs", ""),
            node(9, "examples/demo/main.rs", "mod part;\n"),
            node(10, "examples/demo/part.rs", ""),
        ];
        let manifests = manifests_of(&[("Cargo.toml", "[package]\nname = \"app\"\n")], &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(edges(1), vec!["src/util.rs", "src/config.rs"]);
        assert_eq!(edges(2), vec!["src/config.rs"]);
        assert_eq!(
            edges(4),
            vec!["tests/common/mod.rs", "tests/fixture.rs", "src/util.rs"]
        );
        assert_eq!(edges(5), vec!["tests/fixture.rs"]);
        assert_eq!(edges(7), vec!["src/bin/helpers.rs"]);
        assert_eq!(edges(9), vec!["examples/demo/part.rs"]);
        assert!(!graph.rust_relative_imports_omitted);
    }

    /// Cargo discovers a package's crates unless its manifest turns that off,
    /// and builds `build.rs` unless `build = false`; a target table's `path`
    /// names a crate either way.
    #[test]
    fn a_package_builds_the_crates_its_manifest_leaves_cargo_to_discover() {
        let nodes = vec![
            node(0, "p/examples/auto.rs", "mod a;\n"),
            node(1, "p/examples/a.rs", ""),
            node(2, "p/examples/named.rs", "mod b;\n"),
            node(3, "p/examples/b.rs", ""),
            node(4, "p/build.rs", "mod c;\n"),
            node(5, "p/c.rs", ""),
            node(6, "q/build.rs", "mod d;\n"),
            node(7, "q/d.rs", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "p/Cargo.toml",
                    "[package]\nname = \"p\"\nautoexamples = false\nbuild = false\n\
                     [[example]]\nname = \"named\"\npath = \"examples/named.rs\"\n",
                ),
                ("q/Cargo.toml", "[package]\nname = \"q\"\n"),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert!(graph.edges[&0].is_empty() && graph.edges[&4].is_empty());
        assert_eq!(graph.edges[&2], vec![3]);
        assert_eq!(graph.edges[&6], vec![7]);
    }

    /// A workspace member's library is imported by the name its `Cargo.toml`
    /// gives it, from the root file its `[lib] path` names, and a target
    /// table's `path` roots a crate of its own.
    #[test]
    fn a_rust_path_starts_where_cargo_roots_its_crate() {
        let nodes = vec![
            node(0, "crates/core/src/lib.rs", "pub mod model;\n"),
            node(1, "crates/core/src/model.rs", ""),
            node(
                2,
                "crates/cli/src/main.rs",
                "mod run;\nuse acme_core::model::User;\nuse cli::args;\nuse crate::run::go;\n",
            ),
            node(3, "crates/cli/src/run.rs", ""),
            node(4, "crates/cli/lib/root.rs", "pub mod args;\nmod flags;\n"),
            node(5, "crates/cli/lib/args.rs", "use crate::flags;\n"),
            node(6, "crates/cli/lib/flags.rs", ""),
            node(7, "crates/cli/src/tools/gen.rs", "mod emit;\n"),
            node(8, "crates/cli/src/tools/emit.rs", "use crate::emit as _;\n"),
            node(9, "crates/cli/src/tools/stray.rs", "use crate::run;\n"),
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
                "crates/cli/src/run.rs",
                "crates/core/src/model.rs",
                "crates/cli/lib/args.rs"
            ]
        );
        assert_eq!(
            edges_under(&nodes, 5, &manifests),
            vec!["crates/cli/lib/flags.rs"]
        );
        assert_eq!(
            edges_under(&nodes, 7, &manifests),
            vec!["crates/cli/src/tools/emit.rs"]
        );
        // No crate's `mod` declarations reach the file, so no tree holds it.
        assert!(edges_under(&nodes, 9, &manifests).is_empty());
        let rooted = build_import_graph(&nodes[..9], &manifests);
        assert!(!rooted.rust_relative_imports_omitted);
        assert!(build_import_graph(&nodes, &manifests).rust_relative_imports_omitted);
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
