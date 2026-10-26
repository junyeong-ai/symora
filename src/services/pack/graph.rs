//! The import graph pack ranks: each file's references, resolved to the
//! project files they name by each language's own rules.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::Node;
use super::manifests::{
    self, Basedir, Extensions, Hit, JsPackage, Manifests, ModuleKind, ModuleOptions,
    ModuleResolution, PackageType, Pytest, PythonProject, Rootdir, TsConfig, TsOwner, TypeScript,
    either, export_path, export_subpath, mangle_scope, module_names, nearest, node_modules_in,
    own_or_only, package_module, project_file, project_path, python_packages, python_project,
    reads_base_url, split_package_name, type_root_holds, type_root_name,
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
    /// The specifiers `classic` resolution names a file in the project for
    /// where a declaration it takes first, which pack does not read or whose
    /// `typesVersions` it does not follow, may stand for them.
    pub(super) declaration_undetermined_imports: BTreeSet<String>,
    /// The specifiers a declaration in a `typeRoots` directory may stand
    /// for outside `classic`, where pack cannot tell whether TypeScript
    /// takes it.
    pub(super) type_root_undetermined_imports: BTreeSet<String>,
    /// The specifiers the TypeScript majors that may compile an importer
    /// read as different files under its one configuration, where no
    /// installed or declared version fixes one major.
    pub(super) version_undetermined_imports: BTreeSet<String>,
    /// The specifiers the projects of a tsconfig's `references` compiling
    /// an importer read as different files.
    pub(super) projects_undetermined_imports: BTreeSet<String>,
    /// The specifiers the TypeScript majors that may read the
    /// `jsconfig.json` of an importer's JavaScript project read as different
    /// files.
    pub(super) jsconfig_undetermined_imports: BTreeSet<String>,
    /// The specifiers naming a directory, a workspace package or a
    /// `node_modules` package whose `package.json` holds `typesVersions`,
    /// which pack does not follow.
    pub(super) types_versions_undetermined_imports: BTreeSet<String>,
    /// The declared `typescript` ranges pack reads no major from 5 to 7
    /// from, of the importers of those specifiers.
    pub(super) typescript_ranges: BTreeSet<String>,
    /// Whether an importer of those specifiers has no `typescript`
    /// installed: none at all, or a range declared.
    pub(super) typescript_not_installed: bool,
    /// Whether an importer of those specifiers has a `typescript` installed
    /// that pack reads no major from 5 to 7 from, one above 7 or no number,
    /// which a declared range does not settle: the installed one is read
    /// first.
    pub(super) typescript_installed_unread: bool,
    /// The files writing a non-relative specifier whose TypeScript
    /// configuration the `references` above them leave undetermined: the
    /// projects compiling the file disagree, or none compiles it and one
    /// that could have been meant maps the specifier.
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
        classic: RefCell::default(),
        type_roots: RefCell::default(),
        node_modules: RefCell::default(),
    };

    let mut graph: Graph = BTreeMap::new();
    let mut rust_relative_imports_omitted = false;
    let mut undetermined_imports = BTreeSet::new();
    let mut declaration_undetermined_imports = BTreeSet::new();
    let mut type_root_undetermined_imports = BTreeSet::new();
    let mut version_undetermined_imports = BTreeSet::new();
    let mut projects_undetermined_imports = BTreeSet::new();
    let mut jsconfig_undetermined_imports = BTreeSet::new();
    let mut types_versions_undetermined_imports = BTreeSet::new();
    let mut typescript_ranges = BTreeSet::new();
    let mut typescript_not_installed = false;
    let mut typescript_installed_unread = false;
    let mut ts_owner_undetermined = BTreeSet::new();
    // The configurations each TypeScript project's references reach, and
    // whether one of them maps a specifier, each worked out once for every
    // file no project compiles below it. A specifier's mapping is the same
    // for every JavaScript-family importer, which imports any of their files.
    let mut reached: HashMap<usize, Vec<&TsConfig>> = HashMap::new();
    let mut mapped: HashMap<usize, HashMap<String, bool>> = HashMap::new();
    for node in nodes {
        graph.entry(node.id).or_default();
        let importer = Importer::of(node, manifests);
        let mut seen = HashSet::new();
        let declarations = node.imports.references.iter();
        let requires = node.imports.requires.iter();
        let written = declarations
            .map(|target| (target, ImportForm::Declaration))
            .chain(requires.map(|target| (target, ImportForm::Require)));
        for (target, form) in written {
            let reference = Reference::parse(target, node.language);
            if let Reference::Bare(specifier) = &reference {
                let uncompiled_maps = importer.ts_uncompiled.is_some_and(|project| {
                    let answers = mapped.entry(project).or_default();
                    if let Some(answer) = answers.get(specifier.as_str()) {
                        return *answer;
                    }
                    let configs = reached
                        .entry(project)
                        .or_insert_with(|| manifests.reached(project));
                    let answer = configs
                        .iter()
                        .any(|config| index.config_maps(config, specifier, node.language));
                    answers.insert(specifier.clone(), answer);
                    answer
                });
                if !importer.ts_owners.is_empty() || uncompiled_maps {
                    ts_owner_undetermined.insert(node.rel_path.clone());
                }
            }
            let files = match index.resolve(target, &reference, &importer, form) {
                Resolution::Files(files) => files,
                Resolution::NeedsOwnership => {
                    rust_relative_imports_omitted = true;
                    continue;
                }
                Resolution::Undetermined(specifier) => {
                    undetermined_imports.insert(specifier);
                    continue;
                }
                Resolution::DeclarationUndetermined(specifier) => {
                    declaration_undetermined_imports.insert(specifier);
                    continue;
                }
                Resolution::TypeRootUndetermined(specifier) => {
                    type_root_undetermined_imports.insert(specifier);
                    continue;
                }
                Resolution::ProjectsUndetermined(specifier) => {
                    projects_undetermined_imports.insert(specifier);
                    continue;
                }
                Resolution::JsConfigUndetermined(specifier) => {
                    jsconfig_undetermined_imports.insert(specifier);
                    continue;
                }
                Resolution::TypesVersionsUndetermined(specifier) => {
                    types_versions_undetermined_imports.insert(specifier);
                    continue;
                }
                Resolution::VersionUndetermined(specifier) => {
                    version_undetermined_imports.insert(specifier);
                    match importer.typescript {
                        Some(TypeScript::Above(Some(range)) | TypeScript::Unknown(Some(range))) => {
                            typescript_ranges.insert(range.clone());
                            typescript_not_installed = true;
                        }
                        Some(TypeScript::Above(None) | TypeScript::Unknown(None)) => {
                            typescript_installed_unread = true;
                        }
                        Some(TypeScript::Absent) => typescript_not_installed = true,
                        Some(TypeScript::Major(_)) | None => {}
                    }
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
        declaration_undetermined_imports,
        type_root_undetermined_imports,
        version_undetermined_imports,
        projects_undetermined_imports,
        jsconfig_undetermined_imports,
        types_versions_undetermined_imports,
        typescript_ranges,
        typescript_not_installed,
        typescript_installed_unread,
        ts_owner_undetermined,
    }
}

/// What a reference resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    /// The files it names: none where it names nothing in the project, or
    /// nothing unambiguous.
    Files(Vec<usize>),
    /// A Rust path whose file the crates' module trees do not determine.
    NeedsOwnership,
    /// A specifier the project's own configuration declares that names no
    /// file pack reads.
    Undetermined(String),
    /// A specifier `classic` resolution names a file in the project for,
    /// where a declaration in `node_modules/@types` or a `typeRoots`
    /// directory, which it takes first, may stand for it: one
    /// outside the project, which pack checks on disk without reading it,
    /// or one a `typesVersions` pack does not follow maps.
    DeclarationUndetermined(String),
    /// A specifier a declaration in a `typeRoots` directory may stand for
    /// outside `classic`, where pack cannot tell whether TypeScript takes
    /// it: a `node_modules` package whose `exports` or `typesVersions` pack
    /// does not follow may take the specifier first, or the declaration lies
    /// outside the project, where pack checks the disk alone, while the
    /// rest of the lookup names a file in it.
    TypeRootUndetermined(String),
    /// A specifier the TypeScript majors that may compile the importer read
    /// as different files under its one configuration, no installed or
    /// declared version fixing one.
    VersionUndetermined(String),
    /// A specifier the projects of a tsconfig's `references` that compile
    /// the importer, saying different things, read as different files.
    ProjectsUndetermined(String),
    /// A specifier the TypeScript majors that may read the `jsconfig.json`
    /// of the importer's JavaScript project read as different files.
    JsConfigUndetermined(String),
    /// A specifier naming a directory, a workspace package, or under
    /// `node10` a `node_modules` package taking it before a `paths` or
    /// `baseUrl` file, whose `package.json` holds `typesVersions`, which
    /// pack does not follow.
    TypesVersionsUndetermined(String),
}

/// What resolving a file's references needs to know about where the file
/// sits, worked out once for the file rather than once per reference.
struct Importer<'a> {
    node: &'a Node,
    /// The TypeScript configuration a JavaScript or TypeScript file is
    /// compiled under (`Manifests::ts_owner`).
    ts_config: Option<&'a TsConfig>,
    /// The configurations of the projects compiling the file where they say
    /// different things about specifiers, so that its specifiers are read
    /// without a configuration and its conditions as they agree; empty
    /// otherwise.
    ts_owners: Vec<&'a TsConfig>,
    /// The project whose configuration, with those its references reach,
    /// could have been meant for a file no project those `references` reach
    /// compiles, which is read without one.
    ts_uncompiled: Option<usize>,
    /// How each TypeScript version that may compile the file reads its
    /// non-relative imports, each reading once: an import forms an edge only
    /// where they agree.
    readings: Vec<Reading>,
    /// What TypeScript is installed or declared for the file where its
    /// readings can differ only by major: one configuration, or the
    /// defaults, read under each major that may compile it. `None` where
    /// several projects' configurations read it, where Node or a bundler
    /// does, and in a `jsconfig.json`'s JavaScript project, which is never
    /// one to tell to install TypeScript.
    typescript: Option<&'a TypeScript>,
}

/// How one TypeScript version that may compile a file reads a non-relative
/// specifier: `major` is that version's major, which decides whether it
/// reads `baseUrl` (`reads_base_url`) but in a `jsconfig.json`'s JavaScript
/// project, which its bundler builds; `None` where Node or a bundler reads
/// it; and `mode` how it reads a package's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    major: Option<u8>,
    mode: ImportMode,
}

/// How a module resolution reads a package's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportMode {
    /// `bundler`, `node16` or `nodenext`, which read a package's `exports`
    /// under these conditions.
    Exports(ExportConditions),
    /// `node10`, which reads no `exports`.
    Node10,
    /// `classic`, which reads no `package.json`.
    Classic,
}

/// The conditions a module resolution that reads `exports` matches for a
/// file, each `None` where its configuration leaves it open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExportConditions {
    /// Whether `node` matches: under `node16` and `nodenext`, not under
    /// `bundler`.
    node: Option<bool>,
    /// Whether an import declaration in the file matches `import` rather
    /// than `require` (`declares_es_imports`).
    esm: Option<bool>,
}

/// How one reading of an import looks up a package's `exports` target
/// (`export_target`): whether TypeScript reads it, where Node and the
/// bundlers otherwise do (`Importer::matches`), the conditions it matches,
/// how the import is written, and what a pattern key's `*` matched.
#[derive(Debug, Clone, Copy)]
struct Lookup<'s> {
    typescript: bool,
    conditions: ExportConditions,
    form: ImportForm,
    star: Option<&'s str>,
}

/// How an import is written, which TypeScript resolves under `bundler`,
/// `node16` and `nodenext` as an ES import or a CommonJS `require`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportForm {
    /// An `import` or `export … from` declaration, read by the format the
    /// file is emitted in.
    Declaration,
    /// A TypeScript `import x = require("…")`, a `require` in any file.
    Require,
}

impl<'a> Importer<'a> {
    fn of(node: &'a Node, manifests: &'a Manifests) -> Self {
        let owner = if is_javascript(node.language) {
            manifests.ts_owner(node)
        } else {
            TsOwner::None
        };
        let (ts_config, ts_uncompiled) = match &owner {
            TsOwner::Config(config) => (Some(*config), None),
            TsOwner::Uncompiled(project) => (None, Some(*project)),
            TsOwner::None | TsOwner::Undetermined(_) => (None, None),
        };
        // A TypeScript file no configuration governs is read under the
        // defaults of the TypeScript versions its directory installs or
        // declares; one the projects compiling it disagree on, under each
        // of theirs. A JavaScript file no configuration governs, which
        // TypeScript compiles only where a configuration's `allowJs` takes
        // it, and any file where no TypeScript is installed or declared, is
        // Node's or a bundler's to resolve.
        let unset = ModuleOptions::default();
        let compiled = manifests
            .typescript
            .get(&node.directory)
            .filter(|typescript| {
                node.language == Language::TypeScript && **typescript != TypeScript::Absent
            });
        let (readings, typescript) = match (&owner, compiled) {
            _ if !is_javascript(node.language) => (Vec::new(), None),
            (TsOwner::Config(config), _) => (
                vec![(&config.module_options, config.majors())],
                (!config.javascript).then_some(&config.typescript),
            ),
            (TsOwner::Undetermined(owners), _) => (
                owners
                    .iter()
                    .map(|config| (&config.module_options, config.majors()))
                    .collect(),
                None,
            ),
            (TsOwner::None | TsOwner::Uncompiled(_), Some(typescript)) => (
                vec![(&unset, typescript.majors().to_vec())],
                Some(typescript),
            ),
            (TsOwner::None | TsOwner::Uncompiled(_), None) => {
                return Self {
                    node,
                    ts_config,
                    ts_owners: Vec::new(),
                    ts_uncompiled,
                    typescript: None,
                    readings: vec![Reading {
                        major: None,
                        mode: ImportMode::Exports(ExportConditions {
                            node: None,
                            esm: Some(true),
                        }),
                    }],
                };
            }
        };
        let mut read = Vec::new();
        for (options, majors) in readings {
            for major in &majors {
                let reading = Reading {
                    major: Some(*major),
                    mode: import_mode(options, *major, node, manifests),
                };
                if !read.contains(&reading) {
                    read.push(reading);
                }
            }
        }
        Self {
            node,
            ts_config,
            ts_owners: match owner {
                TsOwner::Undetermined(owners) => owners,
                _ => Vec::new(),
            },
            ts_uncompiled,
            readings: read,
            typescript,
        }
    }

    /// Whether TypeScript matches an `exports` condition for an import this
    /// file writes as `lookup` reads it, `None` where its
    /// configuration leaves that open: `types`, `default` and the
    /// `customConditions` always; `import` for a declaration in a file
    /// emitted (`bundler`) or read by Node (`node16`) as an ES module;
    /// `require` for any other declaration and for every `import x =
    /// require()`; `node` under `node16`; nothing else, as TypeScript's
    /// `getModeForUsageLocation` selects them. A versioned `types@<range>`
    /// key depends on the compiler's own version, and any other key on a
    /// configuration whose `customConditions` its chain leaves undecided. A
    /// file several projects compile takes a custom condition all of their
    /// configurations list, none that none lists, and leaves one some list
    /// open.
    /// Where TypeScript does not resolve the file, Node and the bundlers
    /// agree on `default`, on `import` for a declaration and `require` for
    /// a `require`, and on never matching `types`; every other condition —
    /// `node`, `browser`, `module`, a custom one — some of them match and
    /// others do not, so it stays open.
    fn matches(&self, lookup: Lookup, condition: &str) -> Option<bool> {
        let conditions = lookup.conditions;
        let esm = match lookup.form {
            ImportForm::Declaration => conditions.esm,
            ImportForm::Require => Some(false),
        };
        if !lookup.typescript {
            return match condition {
                "default" => Some(true),
                "import" => esm,
                "require" => esm.map(|esm| !esm),
                "node" => conditions.node,
                _ if condition == "types" || condition.starts_with("types@") => Some(false),
                _ => None,
            };
        }
        if matches!(condition, "types" | "default") {
            return Some(true);
        }
        let listed = |config: &TsConfig| {
            config
                .custom_conditions
                .as_ref()
                .map(|custom| custom.iter().any(|c| c == condition))
        };
        let custom = match (self.ts_config, self.ts_owners.as_slice()) {
            (Some(config), _) => listed(config),
            (None, []) => Some(false),
            (None, owners) => {
                let first = listed(owners[0]);
                owners[1..]
                    .iter()
                    .all(|owner| listed(owner) == first)
                    .then_some(first)
                    .flatten()
            }
        };
        if custom == Some(true) {
            return Some(true);
        }
        match condition {
            "import" | "require" | "node" => match condition {
                "node" => conditions.node,
                "import" => esm,
                _ => esm.map(|esm| !esm),
            },
            _ if condition.starts_with("types@") || custom.is_none() => None,
            _ => Some(false),
        }
    }
}

/// How TypeScript of `major` reads a package's name for a file compiled
/// under `options`: the module resolution they select
/// (`ModuleOptions::resolution`), and where it reads `exports`, the
/// conditions the file's imports match. Options a configuration leaves
/// unknown leave those conditions open.
fn import_mode(
    options: &ModuleOptions,
    major: u8,
    node: &Node,
    manifests: &Manifests,
) -> ImportMode {
    let kind = options.module_kind(major);
    let resolution = options.resolution(major, kind);
    match resolution {
        Some(ModuleResolution::Node10) => ImportMode::Node10,
        Some(ModuleResolution::Classic) => ImportMode::Classic,
        _ => ImportMode::Exports(ExportConditions {
            node: resolution.map(|resolution| resolution == ModuleResolution::Node16),
            esm: resolution
                .and_then(|resolution| declares_es_imports(resolution, kind, node, manifests)),
        }),
    }
}

/// Whether an import declaration in a file matches `import` rather than
/// `require`, as TypeScript's `getEmitSyntaxForUsageLocation` decides: by
/// the format it emits the file in — the one its extension implies (`.mts`
/// and `.mjs` ES modules, `.cts` and `.cjs` CommonJS) or, under `node16`
/// and `nodenext`, the nearest `package.json`'s `"type"`, which a `module`
/// of `node16` to `nodenext` emits whatever it is and any other `module`
/// only where that `"type"` or the extension states it; else the `module`
/// kind's, CommonJS for `commonjs` and ES for `es2015` to `esnext` and
/// `preserve` — and where none is set, `import` under `bundler` and
/// `require` under `node16`. `None` where the module kind it needs is
/// unknown.
fn declares_es_imports(
    resolution: ModuleResolution,
    kind: Option<ModuleKind>,
    node: &Node,
    manifests: &Manifests,
) -> Option<bool> {
    let extension = Path::new(&node.rel_path)
        .extension()
        .and_then(|extension| extension.to_str());
    let package = nearest(&manifests.module_types, &node.directory)
        .map_or(PackageType::Unset, |(_, package)| *package);
    let by_extension = match extension {
        Some("mjs" | "mts") => Some(true),
        Some("cjs" | "cts") => Some(false),
        _ => None,
    };
    let by_package = match package {
        PackageType::Module => Some(true),
        PackageType::CommonJs => Some(false),
        PackageType::Unset => None,
    };
    let implied = by_extension
        .or_else(|| (resolution == ModuleResolution::Node16).then_some(by_package == Some(true)));
    let stated = implied.filter(|esm| {
        kind == Some(ModuleKind::Node) || by_extension.is_some() || by_package == Some(*esm)
    });
    let emitted = match stated {
        Some(esm) => Some(esm),
        None => match kind? {
            ModuleKind::CommonJs => Some(false),
            ModuleKind::Es | ModuleKind::Preserve => Some(true),
            ModuleKind::Node | ModuleKind::Other => None,
        },
    };
    Some(emitted.unwrap_or(resolution == ModuleResolution::Bundler))
}

/// How a module resolution TypeScript reads an import under looks up the
/// path a relative specifier, or a `paths` substitution or `baseUrl`, spells
/// (`ProjectIndex::typescript_path`), as tsc 5.9.3, 6.0.3 and 7.0.2 trace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PathLookup {
    /// `classic`: the file with TypeScript's extensions, then JavaScript's;
    /// no directory.
    Classic,
    /// `node10` under 5 and 6: the file, then the directory, with
    /// TypeScript's extensions, then both again with JavaScript's.
    Node10,
    /// `bundler`, and `node16` for a CommonJS import: the file, then the
    /// directory, with every extension at once.
    Bundler,
    /// `node16` for an ES import: only the file the path names with an
    /// extension in place of the one it writes, none added and no directory.
    Esm,
}

impl PathLookup {
    /// The passes the lookup makes, each with its extensions: `classic` and
    /// `node10` TypeScript's, then JavaScript's, over every path a
    /// specifier may name before the next pass; `bundler` and `node16` one
    /// with both.
    fn passes(self) -> &'static [Extensions] {
        match self {
            Self::Classic | Self::Node10 => &[Extensions::TypeScript, Extensions::JavaScript],
            Self::Bundler | Self::Esm => &[Extensions::All],
        }
    }
}

/// How TypeScript of one reading looks a path an import spells up
/// (`PathLookup`): `node16` as CommonJS for a `require` and for a
/// declaration in a file emitted as CommonJS, as an ES import for one in a
/// file emitted as an ES module. `None` where the reading leaves the
/// resolution or, under `node16`, the format the file is emitted in open,
/// as the reading where no TypeScript reads it does.
fn path_lookup(reading: Reading, form: ImportForm) -> Option<PathLookup> {
    match reading.mode {
        ImportMode::Classic => Some(PathLookup::Classic),
        ImportMode::Node10 => Some(PathLookup::Node10),
        ImportMode::Exports(conditions) => {
            let esm = match form {
                ImportForm::Declaration => conditions.esm,
                ImportForm::Require => Some(false),
            };
            match (conditions.node?, esm) {
                (false, _) | (true, Some(false)) => Some(PathLookup::Bundler),
                (true, Some(true)) => Some(PathLookup::Esm),
                (true, None) => None,
            }
        }
    }
}

/// What TypeScript finds for a non-relative specifier after a pass with
/// declarations looked at its paths (`ProjectIndex::type_root_specifier`,
/// `ProjectIndex::classic_between`).
enum TypeRooted {
    /// What the specifier names: the declaration, or a disclosure.
    Resolved(Resolution),
    /// A `node_modules` package takes it before the type root.
    Packaged,
    /// A declaration pack checks on disk without reading, or one a
    /// `typesVersions` pack does not follow decides, stands for it if it is
    /// there.
    Outside,
}

/// The declaration a `node_modules/@types` or `typeRoots` directory holds
/// for a non-relative specifier, which TypeScript takes before a JavaScript
/// file.
#[derive(Debug, Clone)]
enum Declared {
    /// One in the project: the file where pack reads it, none where it is
    /// only on disk.
    Files(Vec<usize>),
    /// One in the project that a `typesVersions` pack does not follow
    /// decides.
    Open,
    /// One outside the project, where pack checks the disk alone, may be
    /// there.
    Outside,
}

impl Declared {
    fn of(hit: Hit) -> Self {
        match hit {
            Hit::Files(files) => Self::Files(files),
            Hit::Open => Self::Open,
        }
    }
}

/// How a specifier the readings of its importer name different files for
/// is disclosed: as one the projects compiling it read apart where several
/// saying different things take it, as one the TypeScript version decides
/// where they read one configuration under several majors, and in a
/// `jsconfig.json`'s JavaScript project, which is never one to tell to
/// install TypeScript, as one the majors read its configuration apart.
fn read_apart(specifier: &str, importer: &Importer) -> Resolution {
    if !importer.ts_owners.is_empty() {
        return Resolution::ProjectsUndetermined(specifier.to_string());
    }
    match importer.typescript {
        Some(_) => Resolution::VersionUndetermined(specifier.to_string()),
        None => Resolution::JsConfigUndetermined(specifier.to_string()),
    }
}

/// What a package's `exports` target names for one importer.
#[derive(Debug, PartialEq, Eq)]
enum Exported {
    /// The files it resolves to: none where the file is only on disk.
    Files(Vec<usize>),
    /// No target: no condition the importer matches.
    Nothing,
    /// A path naming no file.
    Missing,
    /// Which file depends on a condition the importer's configuration leaves
    /// open.
    Undetermined,
}

impl Exported {
    /// Whether a lookup stops at this target rather than reading the next
    /// entry or key: TypeScript passes over a path naming no file, which
    /// Node and the bundlers resolve and stop at.
    fn ends_lookup(&self, lookup: Lookup) -> bool {
        match self {
            Self::Nothing => false,
            Self::Missing => !lookup.typescript,
            Self::Files(_) | Self::Undetermined => true,
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
    /// A JavaScript or TypeScript specifier that is a relative path, with
    /// the parent hops it starts with and how it ends (`PathEnd`).
    Script {
        up: usize,
        components: Vec<String>,
        end: PathEnd,
    },
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
        if is_javascript(language) {
            return Self::script(raw);
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
        let components = split_components(rest);
        if relative {
            Self::Relative { up, components }
        } else {
            Self::Absolute(components)
        }
    }

    /// A JavaScript or TypeScript specifier: a relative path where
    /// TypeScript's `pathIsRelative` reads one (`.`, `..`, `./…`, `../…`),
    /// its `.` and `..` segments folded as `normalizePath` folds them, so
    /// `./a/../b` is `./b`; an absolute path names nothing; any other is
    /// bare.
    fn script(raw: &str) -> Self {
        let relative = matches!(raw, "." | "..") || raw.starts_with("./") || raw.starts_with("../");
        if !relative {
            return if raw.starts_with('/') {
                Self::Unresolved
            } else {
                Self::Bare(raw.to_string())
            };
        }
        let mut up = 0;
        let mut components: Vec<String> = Vec::new();
        for segment in raw.split('/') {
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
        let end = match raw.rsplit('/').next() {
            Some("") => PathEnd::Slash,
            Some("." | "..") => PathEnd::Dot,
            _ => PathEnd::Name,
        };
        Self::Script {
            up,
            components,
            end,
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

/// How a relative JavaScript or TypeScript specifier ends, which decides
/// whether TypeScript looks it up as a file or only as a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathEnd {
    /// A name (`./a`): a file, then a directory.
    Name,
    /// A `/` (`./`, `./a/`): a directory alone, which `classic`, reading
    /// no directory, takes for none.
    Slash,
    /// A `.` or `..` segment (`.`, `..`, `./a/..`): a directory alone, as
    /// TypeScript's `normalizePathForCJSResolution` reads it, but under
    /// `classic`, which looks for the file the folded path names, the
    /// directory's name with an extension beside it (`.` in `src/app` as
    /// `src/app.ts`).
    Dot,
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
    /// What `classic` resolution named for each non-relative specifier,
    /// once per configuration, importer's directory and specifier, all its
    /// lookup depends on (`read_bare_specifier`): the files of a directory
    /// import the same names, and each lookup checks the disk in every
    /// directory up to the filesystem's root.
    classic: RefCell<HashMap<ClassicKey, Resolution>>,
    /// What the `typeRoots` of a configuration hold for each non-relative
    /// specifier, with an extension added to a name or not, once per
    /// importer's directory (`type_root_declaration`), which decides which
    /// of them the `node_modules` lookup covers.
    type_roots: RefCell<HashMap<TypeRootKey, Option<Declared>>>,
    /// Whether a `node_modules` package from a directory up takes each
    /// non-relative specifier (`node_modules_finds`), once per directory:
    /// the walks up from the importers' directories pass the same ones.
    node_modules: RefCell<HashMap<NodeModulesKey, Option<bool>>>,
}

/// What a `typeRoots` lookup depends on (`ProjectIndex::type_roots`): the
/// configuration, the importer's directory, the specifier and whether a
/// name is given an extension.
type TypeRootKey = (Option<*const TsConfig>, Vec<String>, String, bool);

/// What a `classic` lookup depends on (`ProjectIndex::classic`): the
/// configuration, by its place in the manifests, which outlive the index;
/// the importer's directory; the specifier.
type ClassicKey = (Option<*const TsConfig>, Vec<String>, String);

/// What a `node_modules` lookup depends on (`ProjectIndex::node_modules`):
/// the directory it starts in (`None` above the project's root), the
/// specifier, the pass's extensions, and the lookup.
type NodeModulesKey = (Option<Vec<String>>, String, Extensions, PathLookup);

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
    /// names whole, else the longest of its prefixes down to `base` that is
    /// a module of the crate, which holds the item the next segment names —
    /// an item the path then goes into, as an enum's variants or a type's
    /// associated items (`kind::Kind::A`), lies in that item's module.
    /// `None` where no such prefix is a module of the crate.
    fn lookup(&self, krate: usize, base: &[String], path: &[String]) -> Option<ModuleFile> {
        let modules = self.modules.get(&krate)?;
        let whole = [base, path].concat();
        (base.len()..=whole.len())
            .rev()
            .find_map(|length| modules.get(&whole[..length]).copied())
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
/// (`SourceSet`). The names an import can reach through, which hold what a
/// longer import or a wildcard names (`by_class`), are kept apart from the
/// functions and properties an import names only whole (`by_member`).
struct JvmIndex<'a> {
    by_class: HashMap<Vec<String>, Vec<usize>>,
    by_member: HashMap<Vec<String>, Vec<usize>>,
    by_package: HashMap<Vec<String>, Vec<usize>>,
    sets: HashMap<usize, SourceSet<'a>>,
}

impl<'a> JvmIndex<'a> {
    fn of(nodes: &'a [Node]) -> Self {
        let mut index = Self {
            by_class: HashMap::new(),
            by_member: HashMap::new(),
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
            for name in &node.imports.members {
                let member = [package.as_slice(), std::slice::from_ref(name)].concat();
                index.by_member.entry(member).or_default().push(node.id);
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
    /// sets: Maven's and Gradle's `test`, a name ending in `Test` — Kotlin
    /// Multiplatform's `<target>Test` (`commonTest`, `jvmTest`), Android's
    /// `androidTest`, `androidUnitTest` and `androidInstrumentedTest`, a
    /// Gradle test suite's `integrationTest` — and an Android variant's test
    /// set (`variant_test_base`). Gradle's `testFixtures` is no test set: it
    /// is what tests build on.
    fn is_test(self) -> bool {
        match self {
            Self::Named { name, .. } => {
                name == "test"
                    || (name.len() > "Test".len() && name.ends_with("Test"))
                    || variant_test_base(name).is_some()
            }
            Self::Root(_) => false,
        }
    }

    /// Whether code in this set sees the classes of `other` by the build
    /// tools' defaults: its own; in one module, a `test` set those of `main`
    /// and of `testFixtures`, and `testFixtures` those of `main`, which
    /// Maven, Gradle and its `java-test-fixtures` plugin put on their
    /// classpaths, and an Android variant's test set those of the test set
    /// it is built with (`variant_test_base`).
    fn sees(self, other: SourceSet<'_>) -> bool {
        match (self, other) {
            (Self::Named { module, name }, SourceSet::Named { module: a, name: b }) => {
                module == a
                    && (name == b
                        || variant_test_base(name) == Some(b)
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

/// The test set an Android variant's test set is built together with: its
/// `test` for `test<Variant>` (`testDebug`), its `androidTest` for
/// `androidTest<Variant>` (`androidTestFree`), a variant's name starting in
/// upper case after the prefix. Android documents that "when building a
/// test APK for the 'MyFlavor' variant of your app, Gradle combines the
/// `src/androidTest/` and `src/androidTestMyFlavor/` source sets"
/// (developer.android.com, Advanced test setup); the Android Gradle plugin
/// names and builds a variant's local unit-test set alike. Kotlin
/// Multiplatform names those sets `androidUnitTest<Variant>` and
/// `androidInstrumentedTest<Variant>` and compiles each with
/// `androidUnitTest` or `androidInstrumentedTest` (the Kotlin Gradle
/// plugin's `MultiplatformLayoutV2KotlinAndroidSourceSetNaming`, and its
/// `LanguageSettingsTests`, which compile `androidUnitTest` and
/// `androidUnitTestDebug` in one task). Gradle's `testFixtures` is none.
fn variant_test_base(name: &str) -> Option<&'static str> {
    if name.starts_with("testFixtures") {
        return None;
    }
    [
        "androidInstrumentedTest",
        "androidUnitTest",
        "androidTest",
        "test",
    ]
    .into_iter()
    .find(|base| {
        name.strip_prefix(base)
            .is_some_and(|variant| variant.starts_with(|c: char| c.is_ascii_uppercase()))
    })
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
    /// Where pytest roots a run no configuration file configures.
    rootdirs: &'a HashMap<Vec<String>, Rootdir>,
    /// How pytest runs where no configuration file says.
    default_pytest: Pytest,
    /// The directories holding a `conftest.py`.
    conftests: HashSet<&'a [String]>,
}

impl<'a> PythonPath<'a> {
    fn of(nodes: &'a [Node], manifests: &'a Manifests) -> Self {
        let mut projects = manifests.python_projects.clone();
        projects
            .entry(Vec::new())
            .or_insert_with(|| python_project(&[], None));
        let conftests = nodes
            .iter()
            .filter(|node| {
                node.language == Language::Python
                    && Path::new(&node.rel_path)
                        .file_name()
                        .is_some_and(|name| name == "conftest.py")
            })
            .map(|node| node.directory.as_slice())
            .collect();
        Self {
            packages: python_packages(nodes),
            projects,
            pytest: &manifests.pytest_configurations,
            rootdirs: &manifests.pytest_rootdirs,
            default_pytest: Pytest::default(),
            conftests,
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
    /// nearest above it, as pytest looks for one upward from where it runs,
    /// with the depth of the directory conftests load from — that
    /// configuration's, else the root pytest falls back to (`Rootdir`), else
    /// the project root, where pack stands for the directory pytest is run
    /// from.
    fn pytest_of(&self, directory: &[String]) -> (&Pytest, usize) {
        if let Some((at, pytest)) = nearest(self.pytest, directory) {
            return (pytest, at.len());
        }
        let fallback = |kind| {
            (0..=directory.len())
                .rev()
                .find(|depth| self.rootdirs.get(&directory[..*depth]) == Some(&kind))
        };
        let cut = fallback(Rootdir::Pyproject)
            .or_else(|| fallback(Rootdir::SetupPy))
            .unwrap_or_default();
        (&self.default_pytest, cut)
    }

    /// The directory pytest's import modes put on `sys.path` for a test
    /// module or `conftest.py` in `directory`: the first upward without
    /// `__init__.py`.
    fn basedir_of<'d>(&self, directory: &'d [String]) -> &'d [String] {
        self.package_root_of(directory).unwrap_or(directory)
    }

    /// The directories on a Python file's `sys.path` that its own project
    /// and pytest establish: the one above its top-level package; the roots
    /// of its own project and of the projects in the tree that installing it
    /// installs, its dependencies' dependencies included; the `pythonpath`
    /// of the pytest configuration it runs under; and, for a test module or
    /// `conftest.py` that configuration imports by inserting its basedir,
    /// that basedir — the first directory upward without `__init__.py` —
    /// and the basedir of each `conftest.py` pytest loads before it, in its
    /// directory and those above it down from where conftests load. They
    /// come in the tiers a lookup tries in turn, in the order the import
    /// mode leaves them on `sys.path` (pytest 9.1.1, measured): where pytest
    /// prepends, as it does by default, the file's own basedir, then the
    /// conftests' deepest first, then the rest; where it appends, the rest,
    /// then the conftests' in the order they load, outermost first, then
    /// the file's own.
    fn of_file<'s>(&'s self, from: &'s Node) -> Vec<Vec<&'s [String]>> {
        let package_root = self.package_root_of(&from.directory);
        let (pytest, cut) = self.pytest_of(&from.directory);
        let basedir = pytest
            .imports_as_test(Path::new(&from.rel_path))
            .then(|| package_root.unwrap_or(&from.directory));
        let conftests: Vec<&[String]> = (cut..=from.directory.len())
            .map(|depth| &from.directory[..depth])
            .filter(|directory| self.conftests.contains(directory))
            .map(|directory| self.basedir_of(directory))
            .collect();
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
        let rest: Vec<&[String]> = package_root
            .into_iter()
            .chain(
                installed
                    .into_iter()
                    .flat_map(|project| project.roots.iter().map(Vec::as_slice)),
            )
            .chain(pytest.pythonpath.iter().map(Vec::as_slice))
            .collect();
        let conftests = conftests.into_iter().map(|conftest| vec![conftest]);
        match (basedir, pytest.basedir) {
            (Some(basedir), Basedir::First) => std::iter::once(vec![basedir])
                .chain(conftests.rev())
                .chain([rest])
                .collect(),
            (Some(basedir), Basedir::Last) => std::iter::once(rest)
                .chain(conftests)
                .chain([vec![basedir]])
                .collect(),
            (None, _) | (_, Basedir::Nowhere) => vec![rest],
        }
    }
}

impl ProjectIndex<'_> {
    /// What a reference names, nothing where it names something outside the
    /// project or nothing unambiguous.
    fn resolve(
        &self,
        written: &str,
        reference: &Reference,
        importer: &Importer,
        form: ImportForm,
    ) -> Resolution {
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
                } else {
                    self.module(&path, from.language)
                }
            }
            Reference::Script {
                up,
                components,
                end,
            } => {
                let base = from.directory.len().checked_sub(*up);
                let Some(base) = base else {
                    return Resolution::Files(Vec::new());
                };
                let mut path = from.directory[..base].to_vec();
                path.extend_from_slice(components);
                return self.relative_script(written, &path, *end, importer, form);
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
            Reference::Bare(specifier) => return self.bare_specifier(specifier, importer, form),
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
    /// matches it, which maps every package name too. Each TypeScript
    /// version that may compile the importer reads it (`Reading`), 7 without
    /// `baseUrl`; where they name different files, it is disclosed, as one
    /// the TypeScript version decides where they read one configuration.
    fn bare_specifier(&self, specifier: &str, importer: &Importer, form: ImportForm) -> Resolution {
        let mut read = importer
            .readings
            .iter()
            .map(|reading| self.read_bare_specifier(specifier, importer, *reading, form));
        let first = read.next().unwrap_or(Resolution::Files(Vec::new()));
        if read.all(|other| other == first) {
            return first;
        }
        read_apart(specifier, importer)
    }

    /// What a non-relative specifier names under one reading of it
    /// (`bare_specifier`), looked up under `classic` once for what that
    /// lookup depends on (`ProjectIndex::classic`).
    fn read_bare_specifier(
        &self,
        specifier: &str,
        importer: &Importer,
        reading: Reading,
        form: ImportForm,
    ) -> Resolution {
        if reading.mode != ImportMode::Classic {
            return self.lookup_bare_specifier(specifier, importer, reading, form);
        }
        let key = (
            importer.ts_config.map(std::ptr::from_ref),
            importer.node.directory.clone(),
            specifier.to_string(),
        );
        if let Some(known) = self.classic.borrow().get(&key) {
            return known.clone();
        }
        let found = self.lookup_bare_specifier(specifier, importer, reading, form);
        self.classic.borrow_mut().insert(key, found.clone());
        found
    }

    /// The lookup `read_bare_specifier` makes, as tsc 5.9.3, 6.0.3 and 7.0.2
    /// trace the reading's module resolution. Each pass
    /// (`PathLookup::passes`) looks at each path a `paths` substitution names
    /// in turn, the file one writing an extension names first, then at the
    /// one below `baseUrl`, and `classic` then in the importer's directories
    /// (`classic_walk`). After a pass with declarations `classic` looks past
    /// the root and for a declaration (`classic_between`), the others in
    /// `node_modules` and the `typeRoots` (`type_root_specifier`). Where no
    /// pass names a file, one of any kind a substitution or `baseUrl` spells
    /// exactly is the one, then a workspace package's file, and a specifier
    /// a `paths` pattern other than `*` declares is disclosed.
    fn lookup_bare_specifier(
        &self,
        specifier: &str,
        importer: &Importer,
        reading: Reading,
        form: ImportForm,
    ) -> Resolution {
        let language = importer.node.language;
        let config = importer.ts_config;
        // A JavaScript project's bundler reads its `jsconfig.json`'s `baseUrl`
        // whatever TypeScript is installed: Next.js 16.3.3 and esbuild 0.28.2
        // do with 7.0.2, which reads none.
        let base_url = reading.major.is_some_and(reads_base_url)
            || config.is_some_and(|config| config.javascript);
        let mapped = config.and_then(|config| config.mapped(specifier, base_url));
        let substitutions = || mapped.iter().flat_map(|mapping| &mapping.paths);
        let below_base = config
            .and_then(|config| config.base_url.as_ref())
            .filter(|_| base_url)
            .and_then(|base| project_path(base, specifier));
        // TypeScript looks the paths `paths` and `baseUrl` name up as it
        // does a relative one, each pass over all of them before the next,
        // a substitution writing an extension naming that file first; then
        // a file of any kind one spells exactly is the one.
        let lookup = path_lookup(reading, form);
        let types_versions = || Resolution::TypesVersionsUndetermined(specifier.to_string());
        let configured = |extensions| {
            let lookup = lookup?;
            let hit = substitutions()
                .find_map(|substituted| {
                    substituted
                        .extension
                        .then(|| self.existing(&substituted.path))
                        .flatten()
                        .map(Hit::Files)
                        .or_else(|| {
                            self.path_in_pass(&substituted.path, PathEnd::Name, lookup, extensions)
                        })
                })
                .or_else(|| {
                    let path = below_base.as_ref()?;
                    self.path_in_pass(path, PathEnd::Name, lookup, extensions)
                })?;
            Some(match hit {
                Hit::Files(files) => Resolution::Files(files),
                Hit::Open => types_versions(),
            })
        };
        // A declaration pack does not read may stand for it, which tsc
        // takes over whatever the rest of the lookup names.
        let mut outside = false;
        let directory = Some(importer.node.directory.as_slice());
        // A `node_modules` package forms an edge only as a workspace's does.
        let packaged = || {
            self.workspace_package(specifier, importer, reading, form)
                .unwrap_or(Resolution::Files(Vec::new()))
        };
        let found = 'found: {
            if let Some(lookup) = lookup {
                for extensions in lookup.passes() {
                    let walked = || match lookup {
                        PathLookup::Classic => {
                            self.classic_walk(specifier, importer.node, *extensions)
                        }
                        _ => None,
                    };
                    if let Some(found) = configured(*extensions).or_else(walked) {
                        // `node10` looked in `node_modules` in its TypeScript
                        // pass, before this one reads `paths` and `baseUrl`
                        // again (`p3*`).
                        if lookup == PathLookup::Node10 && *extensions == Extensions::JavaScript {
                            let typescript = Extensions::TypeScript;
                            match self.node_modules_finds(directory, specifier, lookup, typescript)
                            {
                                Some(true) => break 'found packaged(),
                                None => break 'found types_versions(),
                                Some(false) => {}
                            }
                        }
                        break 'found found;
                    }
                    if *extensions == Extensions::JavaScript {
                        continue;
                    }
                    let between = match lookup {
                        PathLookup::Classic => self.classic_between(specifier, importer),
                        _ => self.type_root_specifier(specifier, importer, lookup, *extensions),
                    };
                    match between {
                        Some(TypeRooted::Resolved(found)) => break 'found found,
                        Some(TypeRooted::Packaged) => break 'found packaged(),
                        Some(TypeRooted::Outside) => outside = true,
                        None => {}
                    }
                }
            }
            let spelled = |path: &[String]| match lookup {
                None => self.script_target(path, language),
                Some(_) => self.existing(path),
            };
            let found = substitutions()
                .find_map(|substituted| spelled(&substituted.path))
                .or_else(|| below_base.as_ref().and_then(|path| spelled(path)));
            if let Some(found) = found {
                break 'found Resolution::Files(found);
            }
            let unmapped = || match mapped.as_ref().is_some_and(|mapping| !mapping.catch_all) {
                true => Resolution::Undetermined(specifier.to_string()),
                false => Resolution::Files(Vec::new()),
            };
            self.workspace_package(specifier, importer, reading, form)
                .unwrap_or_else(unmapped)
        };
        let disclosed: fn(String) -> Resolution = match lookup {
            Some(PathLookup::Classic) => Resolution::DeclarationUndetermined,
            _ => Resolution::TypeRootUndetermined,
        };
        match found {
            Resolution::Files(files) if outside && !files.is_empty() => {
                disclosed(specifier.to_string())
            }
            found => found,
        }
    }

    /// What a `typeRoots` directory gives a non-relative specifier outside
    /// `classic`, which TypeScript looks in after `node_modules` in a pass
    /// whose extensions hold declarations, as tsc 5.9.3, 6.0.3 and 7.0.2
    /// trace it (`tr_*`, `tq_*`): `None` where no type root holds a
    /// declaration for it (`type_root_declaration`). Where one does, a
    /// `node_modules` package or `@types` declaration the pass finds comes
    /// first (`node_modules_finds`), which pack does not read but for a
    /// workspace's package; where none does, the declaration is the one; and
    /// where pack cannot tell, which an `exports` or `typesVersions` it does
    /// not follow decides, it is disclosed. One a root outside the project
    /// may hold is disclosed only where the rest of the lookup names a file
    /// in it (`TypeRooted::Outside`).
    fn type_root_specifier(
        &self,
        specifier: &str,
        importer: &Importer,
        lookup: PathLookup,
        extensions: Extensions,
    ) -> Option<TypeRooted> {
        let add = lookup != PathLookup::Esm;
        let declared = self.type_root_declaration(specifier, importer, add)?;
        let directory = Some(importer.node.directory.as_slice());
        let packaged = self.node_modules_finds(directory, specifier, lookup, extensions);
        let undetermined = || Resolution::TypeRootUndetermined(specifier.to_string());
        Some(match (declared, packaged) {
            (_, Some(true)) => TypeRooted::Packaged,
            (Declared::Files(files), Some(false)) => TypeRooted::Resolved(Resolution::Files(files)),
            // Neither is a file pack reads.
            (Declared::Files(files), None) if files.is_empty() => {
                TypeRooted::Resolved(Resolution::Files(files))
            }
            (Declared::Outside, _) => TypeRooted::Outside,
            _ => TypeRooted::Resolved(undetermined()),
        })
    }

    /// Whether TypeScript's lookup in `node_modules` finds a file for a
    /// non-relative specifier with a pass's extensions in each directory from
    /// `directory`, `None` standing for those above the project's root, up to
    /// the filesystem's (`manifests::node_modules_in`): `node10` reads no
    /// `exports`, and `node16` adds no extension for an ES import.
    fn node_modules_finds(
        &self,
        directory: Option<&[String]>,
        specifier: &str,
        lookup: PathLookup,
        extensions: Extensions,
    ) -> Option<bool> {
        let from = directory.map(<[String]>::to_vec);
        let key = (from, specifier.to_string(), extensions, lookup);
        if let Some(known) = self.node_modules.borrow().get(&key) {
            return *known;
        }
        let (exports, add) = (lookup != PathLookup::Node10, lookup != PathLookup::Esm);
        let finds =
            |directory: &Path| node_modules_in(directory, specifier, extensions, exports, add);
        let found = match (directory, &self.manifests.root) {
            (_, None) => Some(false),
            (Some(directory), Some(root)) => {
                let parent = directory.split_last().map(|(_, parent)| parent);
                let up = self.node_modules_finds(parent, specifier, lookup, extensions);
                either(&[finds(&project_file(root, directory)), up])
            }
            (None, Some(root)) => either(&root.ancestors().skip(1).map(finds).collect::<Vec<_>>()),
        };
        self.node_modules.borrow_mut().insert(key, found);
        found
    }

    /// The declaration the `typeRoots` of an importer's configuration hold
    /// for a non-relative specifier, as TypeScript's `resolveFromTypeRoot`
    /// looks in each in turn, by the name `type_root_name` gives: in one in
    /// the project, the declaration there (`declaration_in`), an edge where
    /// pack reads it; outside it, where pack checks the disk alone, one that
    /// may be there (`type_root_holds`). `add` is whether the lookup adds an
    /// extension to a name, which an ES import under `node16` does not. A
    /// root that is the `node_modules/@types` of a directory from the
    /// importer's up is passed over: TypeScript looks in it before, as it
    /// walks `node_modules`, so it holds nothing the walk did not find.
    fn type_root_declaration(
        &self,
        specifier: &str,
        importer: &Importer,
        add: bool,
    ) -> Option<Declared> {
        let config = importer.ts_config;
        let roots = config.map_or(&[][..], |config| config.type_roots.as_slice());
        if roots.is_empty() {
            return None;
        }
        let directory = &importer.node.directory;
        let key = (
            config.map(std::ptr::from_ref),
            directory.clone(),
            specifier.to_string(),
            add,
        );
        if let Some(known) = self.type_roots.borrow().get(&key) {
            return known.clone();
        }
        let found = roots
            .iter()
            .filter(|type_root| !self.manifests.is_types_of_ancestor(type_root, directory))
            .find_map(
                |type_root| match self.manifests.project_path_of(type_root) {
                    Some(base) => {
                        let name = type_root_name(type_root, specifier);
                        self.declaration_in(&base, &name, add, false)
                            .map(Declared::of)
                    }
                    None => type_root_holds(type_root, specifier).then_some(Declared::Outside),
                },
            );
        self.type_roots.borrow_mut().insert(key, found.clone());
        found
    }

    /// The declaration `classic` takes for a non-relative specifier where its
    /// TypeScript pass finds no file, as tsc 5.9.3 and 6.0.3 look one up: in
    /// each `node_modules/@types` from the importer's directory up, the
    /// specifier's whole path, its scope mangled (`declaration_in`), then
    /// in each type root (`type_root_declaration`). Below the project's root
    /// it is the declaration there, an edge where pack reads it; above it,
    /// where pack checks the disk alone, one may be there
    /// (`Manifests::types_above_root`).
    fn classic_declaration(&self, specifier: &str, importer: &Importer) -> Option<Declared> {
        let directory = &importer.node.directory;
        let mangled = mangle_scope(specifier);
        (0..=directory.len())
            .rev()
            .find_map(|depth| {
                let types = ["node_modules".to_string(), "@types".to_string()];
                self.declaration_in(
                    &[&directory[..depth], &types].concat(),
                    &mangled,
                    true,
                    true,
                )
                .map(Declared::of)
            })
            .or_else(|| {
                self.manifests
                    .types_above_root(specifier)
                    .then_some(Declared::Outside)
            })
            .or_else(|| self.type_root_declaration(specifier, importer, true))
    }

    /// The declaration a project directory TypeScript looks in for one — a
    /// `node_modules/@types`, a type root — holds for a name's path below
    /// it, as tsc 5.9.3, 6.0.3 and 7.0.2 trace it: the file the path names
    /// with a declaration's extensions, then the directory
    /// (`directory_module`). Where `versions`, as in a `node_modules/@types`,
    /// a subpath of a package whose `package.json` holds `typesVersions` is
    /// left open.
    fn declaration_in(
        &self,
        base: &[String],
        name: &str,
        add: bool,
        versions: bool,
    ) -> Option<Hit> {
        let (package, subpath) = split_package_name(name);
        if versions && !subpath.is_empty() {
            let package = [base, &split_components(package)].concat();
            let manifest = self.manifests.package_json_at(&package);
            if manifest.is_some_and(|manifest| manifest.get("typesVersions").is_some()) {
                return Some(Hit::Open);
            }
        }
        let path = [base, &split_components(name)].concat();
        self.module_file(&path, Extensions::Declaration, add)
            .map(Hit::Files)
            .or_else(|| self.directory_module(&path, Extensions::Declaration, add))
    }

    /// The file `classic` takes for a non-relative specifier in the
    /// importer's directory or one above it, up to the project's root, with
    /// a pass's extensions, as tsc 5.9.3 and 6.0.3 walk them: the whole path
    /// below each, by the names `module_names` gives.
    fn classic_walk(
        &self,
        specifier: &str,
        from: &Node,
        extensions: Extensions,
    ) -> Option<Resolution> {
        let path = split_components(specifier);
        if path.is_empty() {
            return None;
        }
        let found = (0..=from.directory.len()).rev().find_map(|depth| {
            let below = [&from.directory[..depth], &path].concat();
            self.module_file(&below, extensions, true)
        });
        found.map(Resolution::Files)
    }

    /// What `classic` takes for a non-relative specifier between its passes,
    /// as tsc 5.9.3 and 6.0.3 go on: past the project's root, where pack
    /// checks the disk alone, a TypeScript file ends the lookup with no edge,
    /// as one in the project that pack does not read does; then a declaration
    /// in `node_modules/@types` or a type root (`classic_declaration`) is the
    /// one, before every JavaScript file, or may be (`TypeRooted::Outside`).
    fn classic_between(&self, specifier: &str, importer: &Importer) -> Option<TypeRooted> {
        let path = split_components(specifier);
        let above_root = path.split_last().is_some_and(|(name, directory)| {
            module_names(name, Extensions::TypeScript, true)
                .into_iter()
                .any(|candidate| {
                    let path = [directory, &[candidate]].concat();
                    self.manifests.is_file_above_root(&path)
                })
        });
        if above_root {
            return Some(TypeRooted::Resolved(Resolution::Files(Vec::new())));
        }
        Some(match self.classic_declaration(specifier, importer)? {
            Declared::Files(files) => TypeRooted::Resolved(Resolution::Files(files)),
            Declared::Open | Declared::Outside => TypeRooted::Outside,
        })
    }

    /// The file TypeScript takes for a project path one of its module
    /// resolutions looks up (`PathLookup`), as tsc 5.9.3, 6.0.3 and 7.0.2
    /// trace it: the file the path names with the extensions `module_names`
    /// gives, then, but under `classic` and for an ES import under `node16`,
    /// the directory it names (`directory_module`); only the directory where
    /// the path ends in `/`, `.` or `..` (`PathEnd`). `classic` and `node10`
    /// make a pass with TypeScript's extensions, then one with JavaScript's,
    /// so `node10` takes `x/index.ts` over `x.js`; `bundler` and `node16`
    /// make one pass with both, taking `x.js` over it. `None` where no file
    /// is found.
    fn typescript_path(&self, path: &[String], end: PathEnd, lookup: PathLookup) -> Option<Hit> {
        lookup
            .passes()
            .iter()
            .find_map(|extensions| self.path_in_pass(path, end, lookup, *extensions))
    }

    /// What one pass of a lookup (`PathLookup::passes`) takes for a path:
    /// the file, then the directory, as `typescript_path` reads them.
    fn path_in_pass(
        &self,
        path: &[String],
        end: PathEnd,
        lookup: PathLookup,
        extensions: Extensions,
    ) -> Option<Hit> {
        let file = |add| {
            (end == PathEnd::Name)
                .then(|| self.module_file(path, extensions, add))
                .flatten()
                .map(Hit::Files)
        };
        match lookup {
            PathLookup::Classic => match end {
                PathEnd::Name | PathEnd::Dot => {
                    self.module_file(path, extensions, true).map(Hit::Files)
                }
                PathEnd::Slash => None,
            },
            PathLookup::Node10 | PathLookup::Bundler => {
                file(true).or_else(|| self.directory_module(path, extensions, true))
            }
            PathLookup::Esm => file(false),
        }
    }

    /// The file TypeScript takes for a project directory a path names, by
    /// the `package.json` it holds on disk (`manifests::directory_module`).
    fn directory_module(
        &self,
        directory: &[String],
        extensions: Extensions,
        add: bool,
    ) -> Option<Hit> {
        let manifest = self.manifests.package_json_at(directory);
        let existing = |file: &[String]| self.existing(file);
        manifests::directory_module(directory, manifest.as_ref(), extensions, add, &existing)
    }

    /// The file TypeScript takes for a project path with a set of
    /// extensions (`manifests::module_file`).
    fn module_file(
        &self,
        path: &[String],
        extensions: Extensions,
        add: bool,
    ) -> Option<Vec<usize>> {
        manifests::module_file(path, extensions, add, &|file| self.existing(file))
    }

    /// The `index` script module of a directory a relative specifier spells
    /// as one, where Node and the bundlers rather than TypeScript read it:
    /// the one file named `index` there, none where several are. A file in
    /// the directory whose module path is the directory's is its `index`,
    /// which the module path folds.
    fn directory_index(&self, directory: &[String], language: Language) -> Vec<usize> {
        let found = self
            .exact
            .by_module
            .get(directory)
            .into_iter()
            .flatten()
            .copied()
            .filter(|id| {
                let node = &self.nodes[*id];
                imports_across(language, node.language) && node.directory.as_slice() == directory
            });
        Found::first([Found::of(found)])
    }

    /// The file a project path names whole, whatever its kind: the
    /// project's where pack reads it, none where it is only on disk. `None`
    /// where it names no file.
    fn existing(&self, file: &[String]) -> Option<Vec<usize>> {
        match self.exact.by_file.get(file) {
            Some(found) => Some(vec![*found]),
            None => self.manifests.is_file_on_disk(file).then(Vec::new),
        }
    }

    /// What a relative JavaScript or TypeScript specifier names, `path` being
    /// the project path it spells, under each reading of the importer: the
    /// file the module resolution TypeScript reads it under takes
    /// (`typescript_path`), else a file of any kind the path spells exactly;
    /// where no TypeScript reads it, or what it is read under is left open,
    /// the script module it spells (`script_target`), or for a directory
    /// the `index` it holds (`directory_index`). Where the readings name
    /// different files, it is disclosed, as a non-relative one is
    /// (`bare_specifier`).
    fn relative_script(
        &self,
        written: &str,
        path: &[String],
        end: PathEnd,
        importer: &Importer,
        form: ImportForm,
    ) -> Resolution {
        let mut read: Vec<(Option<PathLookup>, Resolution)> = Vec::new();
        for reading in &importer.readings {
            let lookup = path_lookup(*reading, form);
            if read.iter().any(|(known, _)| *known == lookup) {
                continue;
            }
            let language = importer.node.language;
            let found = match (lookup, end) {
                (None, PathEnd::Name) => {
                    Resolution::Files(self.script_target(path, language).unwrap_or_default())
                }
                (None, PathEnd::Slash | PathEnd::Dot) => {
                    Resolution::Files(self.directory_index(path, language))
                }
                (Some(lookup), _) => match self.typescript_path(path, end, lookup) {
                    Some(Hit::Files(files)) => Resolution::Files(files),
                    Some(Hit::Open) => Resolution::TypesVersionsUndetermined(written.to_string()),
                    None if end == PathEnd::Name => {
                        Resolution::Files(self.existing(path).unwrap_or_default())
                    }
                    None => Resolution::Files(Vec::new()),
                },
            };
            read.push((lookup, found));
        }
        let mut found = read.into_iter().map(|(_, found)| found);
        let first = found.next().unwrap_or(Resolution::Files(Vec::new()));
        if found.all(|other| other == first) {
            return first;
        }
        read_apart(written, importer)
    }

    /// Whether a TypeScript configuration maps a non-relative specifier:
    /// a `paths` pattern other than the catch-all `*` matches it, or its
    /// `paths` or `baseUrl` name a file for it.
    fn config_maps(&self, config: &TsConfig, specifier: &str, language: Language) -> bool {
        let mapped = config.mapped(specifier, true);
        mapped.as_ref().is_some_and(|mapping| !mapping.catch_all)
            || mapped
                .iter()
                .flat_map(|mapping| &mapping.paths)
                .any(|substituted| self.script_target(&substituted.path, language).is_some())
            || config
                .base_url
                .as_ref()
                .and_then(|base| project_path(base, specifier))
                .is_some_and(|path| self.script_target(&path, language).is_some())
    }

    /// What a workspace package's name, or a subpath below it, stands for,
    /// or `None` where the specifier names no package of a workspace the
    /// importer is in. A package with `exports` exports only what it maps:
    /// the name stands for the file `.` maps to, a subpath for the one
    /// `./<subpath>` or the pattern key matching it maps to
    /// (`export_subpath`), read through its conditions (`export_target`).
    /// Without `exports`, or under `node10`, which does not read it, the name
    /// or a subpath stands for the file TypeScript's `node_modules` lookup
    /// takes below the package's directory (`manifests::package_module`), as
    /// tsc 5.9.3, 6.0.3 and 7.0.2 trace it: the name by the first of
    /// `typings`, `types` and `main` it sets, else its `index`, a subpath as
    /// the file it names, then as a directory, an ES import adding no
    /// extension, and a `typesVersions` leaving either open. It looks with
    /// TypeScript's extensions, then with JavaScript's, unless a
    /// `node_modules` from the importer's directory up, which tsc looks in
    /// between, holds a declaration or TypeScript file for the specifier,
    /// which forms no edge, or may hold one, which is disclosed
    /// (`node_modules_finds`); else a subpath stands for a file of any kind it
    /// spells exactly. Where the module resolution is left open, a subpath
    /// stands for the file below the package it spells, as for Node and the
    /// bundlers, for which the name stands for its `main`, else its `index`,
    /// where no `module` or `browser` field names another
    /// (`bundler_entry_differs`). Under `classic`, which reads no
    /// `package.json`, no name is a package's. A package whose file pack
    /// cannot determine is disclosed: a condition the importer's
    /// configuration leaves open, an entry naming no file, or a name two
    /// packages of the workspace give.
    fn workspace_package(
        &self,
        specifier: &str,
        importer: &Importer,
        reading: Reading,
        form: ImportForm,
    ) -> Option<Resolution> {
        let from = importer.node;
        // The conditions `exports` is read under; `node10` reads no `exports`.
        let conditions = match reading.mode {
            ImportMode::Classic => return None,
            ImportMode::Node10 => None,
            ImportMode::Exports(conditions) => Some(conditions),
        };
        let (name, subpath) = split_package_name(specifier);
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
        let typescript = reading.major.is_some();
        let exports = package.exports.as_ref().zip(conditions);
        let Some((exports, conditions)) = exports else {
            let lookup = path_lookup(reading, form);
            if typescript && (subpath.is_empty() || lookup.is_some()) {
                let root = self.manifests.root.as_ref()?;
                let manifest = self.manifests.package_json_at(directory);
                let existing = |file: &[String]| self.existing(file);
                let add = lookup != Some(PathLookup::Esm);
                let read = |extensions| {
                    let manifest = manifest.as_ref();
                    package_module(
                        root, directory, manifest, subpath, extensions, add, &existing,
                    )
                };
                // tsc looks in every `node_modules` from the importer's
                // directory up, `@types` included, for TypeScript's files
                // before it looks for JavaScript's.
                let elsewhere = |lookup| {
                    let at = Some(from.directory.as_slice());
                    self.node_modules_finds(at, specifier, lookup, Extensions::TypeScript)
                };
                let found = match read(Extensions::TypeScript) {
                    Some(hit) => Some(hit),
                    None => match lookup.map_or(Some(false), elsewhere) {
                        Some(true) => return Some(Resolution::Files(Vec::new())),
                        None => return Some(undetermined()),
                        Some(false) => read(Extensions::JavaScript),
                    },
                };
                let spelled = || {
                    let path = project_path(directory, subpath).filter(|_| !subpath.is_empty())?;
                    self.existing(&path)
                };
                return Some(match found {
                    Some(Hit::Files(files)) => Resolution::Files(files),
                    Some(Hit::Open) => Resolution::TypesVersionsUndetermined(specifier.to_string()),
                    None => spelled().map_or_else(undetermined, Resolution::Files),
                });
            }
            let entries: Vec<&str> = if !subpath.is_empty() {
                vec![subpath]
            } else {
                package
                    .main
                    .as_deref()
                    .into_iter()
                    .chain(["index"])
                    .collect()
            };
            let found = entries
                .into_iter()
                .filter_map(|entry| project_path(directory, entry))
                .find_map(|path| self.script_target(&path, from.language));
            return Some(match found {
                Some(files)
                    if !typescript
                        && subpath.is_empty()
                        && self.bundler_entry_differs(
                            package,
                            directory,
                            &files,
                            from.language,
                        ) =>
                {
                    undetermined()
                }
                Some(files) => Resolution::Files(files),
                None => undetermined(),
            });
        };
        let exported = export_subpath(exports, subpath).map(|(target, star)| {
            let lookup = Lookup {
                typescript,
                conditions,
                form,
                star: star.as_deref(),
            };
            self.export_target(target, directory, importer, lookup)
        });
        Some(match exported {
            Some(Exported::Files(files)) => Resolution::Files(files),
            _ => undetermined(),
        })
    }

    /// Whether a bundler can take a package without `exports` for another
    /// file than the `main` (or `index`) Node takes, `files`: its `module`
    /// field, or its `browser` field — a path, or an object replacing that
    /// file's path with another or with `false` — naming one.
    fn bundler_entry_differs(
        &self,
        package: &JsPackage,
        directory: &[String],
        files: &[usize],
        language: Language,
    ) -> bool {
        let names = |path: &str| {
            project_path(directory, path).and_then(|path| self.script_target(&path, language))
        };
        let names_other = |path: &str| names(path).is_some_and(|named| named != files);
        package.module.as_deref().is_some_and(names_other)
            || match &package.browser {
                Some(serde_json::Value::String(path)) => names_other(path),
                Some(serde_json::Value::Object(replaced)) => {
                    replaced.iter().any(|(path, replacement)| {
                        path.starts_with('.')
                            && names(path).is_some_and(|named| named == files)
                            && replacement.as_str().is_none_or(names_other)
                    })
                }
                _ => false,
            }
    }

    /// What an `exports` target names for an importer: a path, which has to
    /// start `./`, the file it spells, a pattern key's `*` match in place of
    /// each `*` it holds; an array, what its first entry naming
    /// a file names; a condition object, what the target of the first of its
    /// keys, in their written order, that the importer matches names — nested
    /// objects read alike. TypeScript passes over a target naming no file to
    /// the next entry or key; Node and the bundlers stop at it, so where they
    /// resolve the file, the lookup ends there. Where a key the importer's
    /// configuration leaves open names a file that the keys after it do not,
    /// the target is undetermined.
    fn export_target(
        &self,
        target: &serde_json::Value,
        directory: &[String],
        importer: &Importer,
        lookup: Lookup,
    ) -> Exported {
        match target {
            serde_json::Value::String(path) if path.starts_with("./") => {
                export_path(path, lookup.star)
                    .and_then(|path| project_path(directory, &path))
                    .and_then(|path| self.script_target(&path, importer.node.language))
                    .map_or(Exported::Missing, Exported::Files)
            }
            serde_json::Value::Array(entries) => entries
                .iter()
                .map(|entry| self.export_target(entry, directory, importer, lookup))
                .find(|exported| exported.ends_lookup(lookup))
                .unwrap_or(Exported::Nothing),
            serde_json::Value::Object(conditions) => {
                let conditions: Vec<_> = conditions.iter().collect();
                self.conditional_target(&conditions, directory, importer, lookup)
            }
            _ => Exported::Nothing,
        }
    }

    fn conditional_target(
        &self,
        conditions: &[(&String, &serde_json::Value)],
        directory: &[String],
        importer: &Importer,
        lookup: Lookup,
    ) -> Exported {
        for (at, (condition, target)) in conditions.iter().enumerate() {
            let matched = importer.matches(lookup, condition);
            if matched == Some(false) {
                continue;
            }
            let exported = self.export_target(target, directory, importer, lookup);
            if !exported.ends_lookup(lookup) {
                continue;
            }
            if matched.is_none() {
                let rest =
                    self.conditional_target(&conditions[at + 1..], directory, importer, lookup);
                return if rest == exported {
                    rest
                } else {
                    Exported::Undetermined
                };
            }
            return exported;
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
    /// a function or property among those, then without each trailing
    /// segment in turn, which nested classes and members add to what a
    /// class-like name holds (a function or property holds nothing an
    /// import names). Scala imports a package by its name as well, which
    /// names the files the package holds.
    fn jvm_class(&self, name: &[String], from: &Node) -> Vec<usize> {
        let whole: Vec<usize> = [&self.jvm.by_class, &self.jvm.by_member]
            .into_iter()
            .filter_map(|index| index.get(name))
            .flatten()
            .copied()
            .collect();
        if !whole.is_empty() {
            return self.jvm_one(&whole, from).into_iter().collect();
        }
        for length in (1..name.len()).rev() {
            if let Some(files) = self.jvm.by_class.get(&name[..length]) {
                return self.jvm_one(files, from).into_iter().collect();
            }
        }
        if from.language == Language::Scala {
            return self.jvm_package(name, from);
        }
        Vec::new()
    }

    /// The files `import a.b.*` names: the class-like name `a.b`, whose
    /// members it imports, or else everything the package `a.b` holds.
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
    /// it in a source set the importer's sees — its own, and those its module
    /// builds it with (`SourceSet::sees`). Where no set it sees holds the package,
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
    /// does not read, so they name nothing. A test module's basedir is looked
    /// in before or after the rest, where pytest's import mode puts it at
    /// the start or the end of `sys.path`; two files on the same footing name
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
        Found::first(self.python.of_file(from).into_iter().map(|bases| {
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
        let script = |up, c: &[&str], end| Reference::Script {
            up,
            components: c.iter().map(|s| s.to_string()).collect(),
            end,
        };
        assert_eq!(
            Reference::parse("./services/pack", Language::TypeScript),
            script(0, &["services", "pack"], PathEnd::Name)
        );
        assert_eq!(
            Reference::parse("../../shared/util", Language::TypeScript),
            script(2, &["shared", "util"], PathEnd::Name)
        );
        assert_eq!(
            Reference::parse("../", Language::TypeScript),
            script(1, &[], PathEnd::Slash)
        );
        assert_eq!(
            Reference::parse(".", Language::TypeScript),
            script(0, &[], PathEnd::Dot)
        );
        assert_eq!(
            Reference::parse("..", Language::JavaScript),
            script(1, &[], PathEnd::Dot)
        );
        assert_eq!(
            Reference::parse("./a/../../b/./c", Language::TypeScript),
            script(1, &["b", "c"], PathEnd::Name)
        );
        assert_eq!(
            Reference::parse("./a/..", Language::TypeScript),
            script(0, &[], PathEnd::Dot)
        );
        assert_eq!(
            Reference::parse(".prettierrc", Language::TypeScript),
            Reference::Bare(".prettierrc".to_string())
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

    /// An Android variant's test set — `test` or `androidTest` and the
    /// variant's name, `testDebug`, `androidTestFree` — is a test set, built
    /// with its module's `test` or `androidTest` (`android_vt`), so it sees
    /// what that set declares and another module never sees it; Gradle's
    /// `testFixtures` stays visible to other modules.
    #[test]
    fn an_android_variant_test_set_is_built_with_its_modules_test_set() {
        let java = |id, rel, source| node_in(id, rel, source, Language::Java);
        let nodes = vec![
            java(
                0,
                "app/src/test/java/com/acme/fake/FakeApi.java",
                "package com.acme.fake; public class FakeApi {}\n",
            ),
            java(
                1,
                "app/src/testDebug/java/com/acme/t/A.java",
                "package com.acme.t;\nimport com.acme.fake.FakeApi;\nimport com.acme.app.App;\n\
                 import com.acme.dup.Dup;\nclass A {}\n",
            ),
            java(
                2,
                "app/src/main/java/com/acme/app/App.java",
                "package com.acme.app; public class App {}\n",
            ),
            java(
                3,
                "app/src/androidTest/java/com/acme/it/Helper.java",
                "package com.acme.it; public class Helper {}\n",
            ),
            java(
                4,
                "app/src/androidTestFree/java/com/acme/it2/B.java",
                "package com.acme.it2;\nimport com.acme.it.Helper;\nimport com.acme.dup.Dup;\nclass B {}\n",
            ),
            java(
                5,
                "lib/src/main/java/com/acme/lib/L.java",
                "package com.acme.lib;\nimport com.acme.t.A;\nimport com.acme.fx.Fx;\nclass L {}\n",
            ),
            java(
                6,
                "app/src/testFixtures/java/com/acme/fx/Fx.java",
                "package com.acme.fx; public class Fx {}\n",
            ),
            // Declared by both test sets: each variant set sees its own base's.
            java(
                7,
                "app/src/test/java/com/acme/dup/Dup.java",
                "package com.acme.dup; public class Dup {}\n",
            ),
            java(
                8,
                "app/src/androidTest/java/com/acme/dup/Dup.java",
                "package com.acme.dup; public class Dup {}\n",
            ),
        ];
        assert_eq!(
            edges_of(&nodes, 1),
            vec![
                "app/src/test/java/com/acme/fake/FakeApi.java",
                "app/src/main/java/com/acme/app/App.java",
                "app/src/test/java/com/acme/dup/Dup.java"
            ]
        );
        assert_eq!(
            edges_of(&nodes, 4),
            vec![
                "app/src/androidTest/java/com/acme/it/Helper.java",
                "app/src/androidTest/java/com/acme/dup/Dup.java"
            ]
        );
        assert_eq!(
            edges_of(&nodes, 5),
            vec!["app/src/testFixtures/java/com/acme/fx/Fx.java"]
        );
    }

    /// Kotlin Multiplatform's Android variant test sets,
    /// `androidUnitTest<Variant>` and `androidInstrumentedTest<Variant>`,
    /// are test sets built with `androidUnitTest` and
    /// `androidInstrumentedTest` (`kmp_variant`).
    #[test]
    fn a_kotlin_multiplatform_android_variant_test_set_is_built_with_its_test_set() {
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let nodes = vec![
            kotlin(
                0,
                "shared/src/androidUnitTest/kotlin/com/acme/fake/FakeApi.kt",
                "package com.acme.fake\nclass FakeApi\n",
            ),
            kotlin(
                1,
                "shared/src/androidUnitTestDebug/kotlin/com/acme/t/A.kt",
                "package com.acme.t\nimport com.acme.fake.FakeApi\nclass A\n",
            ),
            kotlin(
                2,
                "shared/src/androidUnitTestDebug/kotlin/com/acme/t/DebugOnly.kt",
                "package com.acme.t\nclass DebugOnly\n",
            ),
            kotlin(
                3,
                "shared/src/androidInstrumentedTest/kotlin/com/acme/it/Probe.kt",
                "package com.acme.it\nclass Probe\n",
            ),
            kotlin(
                4,
                "shared/src/androidInstrumentedTestFree/kotlin/com/acme/it2/B.kt",
                "package com.acme.it2\nimport com.acme.it.Probe\nclass B\n",
            ),
            kotlin(
                5,
                "app/src/main/kotlin/com/acme/app/App.kt",
                "package com.acme.app\nimport com.acme.t.DebugOnly\nclass App\n",
            ),
        ];
        assert_eq!(
            edges_of(&nodes, 1),
            vec!["shared/src/androidUnitTest/kotlin/com/acme/fake/FakeApi.kt"]
        );
        assert_eq!(
            edges_of(&nodes, 4),
            vec!["shared/src/androidInstrumentedTest/kotlin/com/acme/it/Probe.kt"]
        );
        assert!(edges_of(&nodes, 5).is_empty());
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

    /// A name the sets a JVM file sees declare is theirs, whatever other set
    /// it may see declares it too: `testFixtures` sees its module's `main`,
    /// and `test` its module's `testFixtures`, as Gradle's
    /// `java-test-fixtures` plugin wires them, before another module's sets.
    #[test]
    fn a_name_the_sets_a_jvm_file_sees_declare_is_theirs_before_another_modules() {
        let java = |id, rel, source| node_in(id, rel, source, Language::Java);
        let base = "package com.acme.base; public class Base {}\n";
        let fixture = "package com.acme.fix; public class Fixture {}\n";
        let nodes = vec![
            java(0, "lib/src/main/java/com/acme/base/Base.java", base),
            java(1, "other/src/main/java/com/acme/base/Base.java", base),
            java(
                2,
                "lib/src/testFixtures/java/com/acme/fix/Fixture.java",
                "package com.acme.fix;\nimport com.acme.base.Base;\npublic class Fixture {}\n",
            ),
            java(
                3,
                "other/src/testFixtures/java/com/acme/fix/Fixture.java",
                fixture,
            ),
            java(
                4,
                "lib/src/test/java/com/acme/t/T.java",
                "package com.acme.t;\nimport com.acme.fix.Fixture;\nclass T {}\n",
            ),
        ];
        assert_eq!(
            edges_of(&nodes, 2),
            vec!["lib/src/main/java/com/acme/base/Base.java"]
        );
        assert_eq!(
            edges_of(&nodes, 4),
            vec!["lib/src/testFixtures/java/com/acme/fix/Fixture.java"]
        );
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

    /// A top-level function or property is imported only by its whole name:
    /// Kotlin reads an import's qualifier as a package or a class, and Scala
    /// as a stable path (a package, an object, a `val`), so a longer import
    /// or a wildcard through a function names nothing — it is some class
    /// pack does not read. Java reaches them through the file class.
    #[test]
    fn a_jvm_function_or_property_is_imported_only_by_its_whole_name() {
        let kotlin = |id, rel, source| node_in(id, rel, source, Language::Kotlin);
        let scala = |id, rel, source| node_in(id, rel, source, Language::Scala);
        let nodes = vec![
            kotlin(
                0,
                "src/main/kotlin/com/acme/Routes.kt",
                "package com.acme\nfun api(): String = \"/api\"\nval base = 1\n",
            ),
            kotlin(
                1,
                "src/main/kotlin/com/acme/app/A.kt",
                "package com.acme.app\nimport com.acme.api.UserDto\nimport com.acme.api.*\n\
                 import com.acme.base.Inner\n",
            ),
            kotlin(
                2,
                "src/main/kotlin/com/acme/app/B.kt",
                "package com.acme.app\nimport com.acme.api\n",
            ),
            node_in(
                3,
                "src/main/java/com/acme/j/J.java",
                "package com.acme.j;\nimport static com.acme.RoutesKt.api;\nclass J {}\n",
                Language::Java,
            ),
            scala(
                4,
                "src/main/scala/com/acme/s/Defs.scala",
                "package com.acme.s\ndef helper = 1\nval config = Config()\n",
            ),
            scala(
                5,
                "src/main/scala/com/acme/t/Use.scala",
                "package com.acme.t\nimport com.acme.s.helper.x\nimport com.acme.s.helper._\n",
            ),
            scala(
                6,
                "src/main/scala/com/acme/t/Port.scala",
                "package com.acme.t\nimport com.acme.s.config.port\n",
            ),
        ];
        let routes = "src/main/kotlin/com/acme/Routes.kt";
        assert!(edges_of(&nodes, 1).is_empty());
        assert_eq!(edges_of(&nodes, 2), vec![routes]);
        assert_eq!(edges_of(&nodes, 3), vec![routes]);
        assert!(edges_of(&nodes, 5).is_empty());
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
    /// `setup.py`, a `pyproject.toml` or `setup.cfg` other than one holding
    /// pytest's table alone — and that is no package: a package's
    /// `setup.py` is one of its modules. A file that only configures pytest
    /// (`pytest.ini`, `tox.ini`, a `pyproject.toml` or `setup.cfg` holding
    /// only pytest's table) sets how pytest runs below it, wherever it is,
    /// and the files there stay the enclosing project's.
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
            python(13, "lone/run.py", "import core_pkg.m\nimport lonely\n"),
            python(14, "lone/lonely.py", ""),
            python(15, "blank/run.py", "import core_pkg.m\nimport blanket\n"),
            python(16, "blank/blanket.py", ""),
            python(17, "bare/run.py", "import core_pkg.m\nimport barely\n"),
            python(18, "bare/barely.py", ""),
            python(19, "bad/run.py", "import core_pkg.m\nimport badge\n"),
            python(20, "bad/badge.py", ""),
            python(21, "src/app/tests/test_v.py", "import vendored\n"),
            python(22, "src/app/vendor/vendored.py", ""),
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
                (
                    "lone/pyproject.toml",
                    "[project]\nname = \"lone\"\n\n[tool.pytest.ini_options]\naddopts = \"-q\"\n",
                ),
                ("blank/setup.cfg", ""),
                ("bare/pyproject.toml", "[tool]\n"),
                ("bad/pyproject.toml", "[project\nname = \"bad\"\n"),
                ("src/app/pytest.ini", "[pytest]\npythonpath = vendor\n"),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(edges(2), vec![core]);
        assert_eq!(edges(5), vec![core, "tests/helpers_dir/helper.py"]);
        assert_eq!(edges(7), vec![core, "checks/support/aid.py"]);
        assert_eq!(edges(9), vec![core, "more/extra/bit.py"]);
        // A `setup.cfg` with build metadata makes a project of its own, which
        // declares no dependency, and so does a `pyproject.toml` with
        // `[project]` besides pytest's table, and a file pytest does not read
        // as its configuration: an empty `setup.cfg` or `[tool]` table, a
        // `pyproject.toml` that cannot be parsed.
        assert_eq!(edges(11), vec!["side/sidekick.py"]);
        assert_eq!(edges(13), vec!["lone/lonely.py"]);
        assert_eq!(edges(15), vec!["blank/blanket.py"]);
        assert_eq!(edges(17), vec!["bare/barely.py"]);
        assert_eq!(edges(19), vec!["bad/badge.py"]);
        // pytest reads a configuration in a package directory, run from the
        // project root as `pytest src/app/tests`, as pytest 9.1.1 does.
        assert_eq!(edges(21), vec!["src/app/vendor/vendored.py"]);
    }

    /// A directory below a root an enclosing project puts on `sys.path` —
    /// its `src/`, a package location it declares — holds that root's
    /// packages, a namespace package needing no `__init__.py` (PEP 420), so
    /// a `setup.py` module there makes no project of it (`pyns`). Below the
    /// project's own directory, a flat layout's root, a directory whose files
    /// configure a build is a project of its own, as a workspace member is,
    /// and so is a root itself, which is not below it.
    #[test]
    fn a_directory_below_a_root_its_project_puts_on_sys_path_starts_no_project() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "src/acme/setup.py", "def configure():\n    pass\n"),
            python(1, "src/acme/a.py", "from acme.core.model import M\n"),
            python(2, "src/acme/core/__init__.py", ""),
            python(3, "src/acme/core/model.py", ""),
            python(4, "dl/lib/ns/setup.py", ""),
            python(5, "dl/lib/ns/a.py", "from ns.b import B\n"),
            python(6, "dl/lib/ns/b.py", ""),
            python(7, "ws/packages/lib/tool.py", "import helper\n"),
            python(8, "ws/packages/lib/helper.py", ""),
            python(9, "dr/lib/tool.py", "import x\n"),
            python(10, "dr/lib/inner/x.py", ""),
            python(11, "dr/lib/x.py", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"acme\"\n[build-system]\nrequires = [\"setuptools\"]\n\
                     build-backend = \"setuptools.build_meta\"\n",
                ),
                ("src/acme/setup.py", "def configure():\n    pass\n"),
                (
                    "dl/pyproject.toml",
                    "[project]\nname = \"dl\"\n[tool.setuptools.packages.find]\nwhere = [\"lib\"]\n",
                ),
                ("dl/lib/ns/setup.py", ""),
                ("ws/pyproject.toml", "[project]\nname = \"ws\"\n"),
                (
                    "ws/packages/lib/pyproject.toml",
                    "[project]\nname = \"lib\"\n",
                ),
                (
                    "dr/pyproject.toml",
                    "[project]\nname = \"dr\"\n[tool.setuptools.packages.find]\nwhere = [\"lib\"]\n",
                ),
                (
                    "dr/lib/pyproject.toml",
                    "[project]\nname = \"inner\"\n[tool.setuptools.packages.find]\nwhere = [\"inner\"]\n",
                ),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(edges(1), vec!["src/acme/core/model.py"]);
        assert_eq!(edges(5), vec!["dl/lib/ns/b.py"]);
        assert_eq!(edges(7), vec!["ws/packages/lib/helper.py"]);
        assert_eq!(edges(9), vec!["dr/lib/inner/x.py"]);
    }

    /// Only a root a project puts on `sys.path` covers the directories below
    /// it: one it declares, and setuptools' discovered `src/` only where a
    /// build installs the project. A uv workspace's virtual root installs
    /// nothing (`uvsrc`, which uv 0.13.5 installs and imports as each member's
    /// own), and a workspace's members are never covered by its root. Below a
    /// root, a directory starts a project only where its own files declare
    /// one: a `pyproject.toml` that configures a tool there leaves it the
    /// enclosing project's (`nobuild_nested`). A `[project]` uv manages
    /// without `[build-system]` installs nothing (`uvapp_pathdep`, whose `path`
    /// dependency below `src/` uv 0.13.5 installs and imports as its own), nor
    /// one whose `tool.uv.package` is `false`, but it is still a project,
    /// whose dependencies uv 0.13.5 installs: a workspace member below `src/`
    /// (`uvsrc_app`), or an application below a root configuring only ruff
    /// (`ruffroot_uvapp`), imports its uv dependency. Each file that declares
    /// a project starts one there: a `setup.cfg` beside a `pyproject.toml`
    /// configuring a tool, a `setup.py`, and a Poetry 1.x `[build-system]`
    /// without `[project]`, each of which `uv pip install -e` (uv 0.13.5)
    /// installs.
    #[test]
    fn only_a_root_a_project_puts_on_sys_path_covers_the_directories_below_it() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let import = "from svc_a.core import c\n";
        let mut nodes = vec![
            python(0, "uvsrc/src/svc_a/svc_a/__init__.py", ""),
            python(1, "uvsrc/src/svc_a/svc_a/core.py", ""),
            python(2, "uvsrc/src/svc_a/tests/test_a.py", import),
            python(3, "uvsrc/src/svc_b/svc_b/__init__.py", ""),
            python(4, "uvsrc/src/svc_b/svc_b/main.py", import),
            python(5, "vr/src/tool/tool/__init__.py", ""),
            python(6, "vr/src/tool/tool/x.py", ""),
            python(7, "vr/src/tool/tests/test_t.py", "import tool.x\n"),
            python(8, "bw/src/m/m/__init__.py", ""),
            python(9, "bw/src/m/m/x.py", ""),
            python(10, "bw/src/m/tests/test_m.py", "import m.x\n"),
            python(11, "nb/src/app/core.py", ""),
            python(12, "nb/src/app/main.py", "from app.core import x\n"),
            python(13, "nb/tests/test_core.py", "from app.core import x\n"),
        ];
        let member = |name: &str, dependencies: &str| {
            format!(
                "[project]\nname = \"{name}\"\ndependencies = [{dependencies}]\n\
                 [build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n"
            )
        };
        let svc_b = format!(
            "{}[tool.uv.sources]\nsvc-a = {{ workspace = true }}\n",
            member("svc-b", "\"svc-a\"")
        );
        let configs = [
            (
                "uvsrc/pyproject.toml",
                "[tool.uv.workspace]\nmembers = [\"src/*\"]\n".to_string(),
            ),
            ("uvsrc/src/svc_a/pyproject.toml", member("svc-a", "")),
            ("uvsrc/src/svc_b/pyproject.toml", svc_b),
            (
                "vr/pyproject.toml",
                "[tool.uv.workspace]\nmembers = [\"packages/*\"]\n".to_string(),
            ),
            ("vr/src/tool/pyproject.toml", member("tool", "")),
            (
                "bw/pyproject.toml",
                "[project]\nname = \"bw\"\n[build-system]\nrequires = [\"setuptools\"]\n\
                 build-backend = \"setuptools.build_meta\"\n\
                 [tool.uv.workspace]\nmembers = [\"src/*\"]\n"
                    .to_string(),
            ),
            ("bw/src/m/pyproject.toml", member("m", "")),
            (
                "nb/pyproject.toml",
                "[tool.ruff]\nline-length = 100\n".to_string(),
            ),
            (
                "nb/src/app/pyproject.toml",
                "[tool.ruff]\nextend = \"../../pyproject.toml\"\n".to_string(),
            ),
        ];
        let mut configs: Vec<(String, String)> = configs
            .into_iter()
            .map(|(path, text)| (path.to_string(), text))
            .collect();
        let mut plugins = Vec::new();
        for (app, build) in [
            ("ua", ""),
            (
                "uf",
                "[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n\
                 [tool.uv]\npackage = false\n",
            ),
        ] {
            let core = format!("{app}/src/plugin/plugin/core.py");
            for (rel, source) in [
                (format!("{app}/src/plugin/plugin/__init__.py"), ""),
                (core.clone(), ""),
                (format!("{app}/main.py"), "from plugin.core import y\n"),
                (
                    format!("{app}/src/plugin/plugin/main.py"),
                    "from plugin.core import y\n",
                ),
                (
                    format!("{app}/src/plugin/tests/test_core.py"),
                    "from plugin.core import y\n",
                ),
            ] {
                if !source.is_empty() {
                    plugins.push((nodes.len(), core.clone()));
                }
                nodes.push(node_in(nodes.len(), &rel, source, Language::Python));
            }
            configs.push((
                format!("{app}/pyproject.toml"),
                format!(
                    "[project]\nname = \"app\"\ndependencies = [\"plugin\"]\n{build}\
                     [tool.uv.sources]\nplugin = {{ path = \"src/plugin\", editable = true }}\n"
                ),
            ));
            configs.push((
                format!("{app}/src/plugin/pyproject.toml"),
                member("plugin", ""),
            ));
        }
        let mut dependents = Vec::new();
        for (root, root_config, app, source) in [
            (
                "us",
                "[tool.uv.workspace]\nmembers = [\"src/*\"]\n",
                "api",
                "lib = { workspace = true }",
            ),
            (
                "rr",
                "[tool.ruff]\nline-length = 100\n",
                "service",
                "lib = { path = \"../lib\", editable = true }",
            ),
        ] {
            let core = format!("{root}/src/lib/lib/core.py");
            for (rel, source) in [
                (format!("{root}/src/lib/lib/__init__.py"), ""),
                (core.clone(), ""),
                (
                    format!("{root}/src/{app}/main.py"),
                    "from lib.core import x\n",
                ),
                (
                    format!("{root}/src/{app}/tests/test_main.py"),
                    "from lib.core import x\n",
                ),
            ] {
                if !source.is_empty() {
                    dependents.push((nodes.len(), core.clone()));
                }
                nodes.push(node_in(nodes.len(), &rel, source, Language::Python));
            }
            configs.push((format!("{root}/pyproject.toml"), root_config.to_string()));
            configs.push((format!("{root}/src/lib/pyproject.toml"), member("lib", "")));
            configs.push((
                format!("{root}/src/{app}/pyproject.toml"),
                format!(
                    "[project]\nname = \"{app}\"\ndependencies = [\"lib\"]\n\
                     [tool.uv.sources]\n{source}\n"
                ),
            ));
        }
        for (root, package, files) in [
            (
                "sc",
                "a",
                vec![
                    ("pyproject.toml", "[tool.black]\nline-length = 100\n"),
                    (
                        "setup.cfg",
                        "[metadata]\nname = a\n\n[options]\npackages = find:\n",
                    ),
                ],
            ),
            (
                "sp",
                "b",
                vec![(
                    "setup.py",
                    "from setuptools import setup\nsetup(name=\"b\")\n",
                )],
            ),
            (
                "pt",
                "c",
                vec![(
                    "pyproject.toml",
                    "[tool.poetry]\nname = \"c\"\nversion = \"0.1.0\"\n\
                     [build-system]\nrequires = [\"poetry-core>=1.0.0\"]\n\
                     build-backend = \"poetry.core.masonry.api\"\n",
                )],
            ),
        ] {
            let core = format!("{root}/src/{package}/{package}/core.py");
            for (rel, source) in [
                (
                    format!("{root}/src/{package}/{package}/__init__.py"),
                    String::new(),
                ),
                (core.clone(), String::new()),
                (
                    format!("{root}/src/{package}/tests/test_{package}.py"),
                    format!("from {package}.core import x\n"),
                ),
            ] {
                if !source.is_empty() {
                    dependents.push((nodes.len(), core.clone()));
                }
                nodes.push(node_in(nodes.len(), &rel, &source, Language::Python));
            }
            configs.push((
                format!("{root}/pyproject.toml"),
                "[tool.ruff]\nline-length = 100\n".to_string(),
            ));
            for (file, text) in files {
                configs.push((format!("{root}/src/{package}/{file}"), text.to_string()));
            }
        }
        // A Poetry 1.x root, which `[build-system]` alone installs, covers
        // the directories below its `src/`, a nested project's among them:
        // installed editable (`uv pip install -e`, uv 0.13.5), it loads
        // `pq/src/pq/core.py` from `pq/src/tools/tests`.
        let core = "pq/src/pq/core.py".to_string();
        for (rel, source) in [
            ("pq/src/pq/__init__.py", ""),
            ("pq/src/pq/core.py", ""),
            ("pq/src/tools/tests/test_t.py", "from pq.core import x\n"),
        ] {
            if !source.is_empty() {
                dependents.push((nodes.len(), core.clone()));
            }
            nodes.push(node_in(nodes.len(), rel, source, Language::Python));
        }
        configs.push((
            "pq/pyproject.toml".to_string(),
            "[tool.poetry]\nname = \"pq\"\nversion = \"0.1.0\"\n\
             packages = [{ include = \"pq\", from = \"src\" }]\n\
             [build-system]\nrequires = [\"poetry-core>=1.0.0\"]\n\
             build-backend = \"poetry.core.masonry.api\"\n"
                .to_string(),
        ));
        configs.push((
            "pq/src/tools/pyproject.toml".to_string(),
            member("tools", ""),
        ));
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let edges = |id| edges_under(&nodes, id, &manifests);
        let core = "uvsrc/src/svc_a/svc_a/core.py";
        assert_eq!(edges(2), vec![core]);
        assert_eq!(edges(4), vec![core]);
        assert_eq!(edges(7), vec!["vr/src/tool/tool/x.py"]);
        assert_eq!(edges(10), vec!["bw/src/m/m/x.py"]);
        assert_eq!(edges(12), vec!["nb/src/app/core.py"]);
        assert_eq!(edges(13), vec!["nb/src/app/core.py"]);
        for (importer, core) in plugins.iter().chain(&dependents) {
            assert_eq!(
                edges(*importer),
                vec![core.as_str()],
                "{}",
                nodes[*importer].rel_path
            );
        }
    }

    /// pytest 9 reads, in each directory, the first of `pytest.toml`,
    /// `.pytest.toml`, `pytest.ini`, `.pytest.ini`, `pyproject.toml`,
    /// `tox.ini` and `setup.cfg` that configures it — the first four
    /// whatever they hold, `pyproject.toml` through its native `[tool.pytest]`
    /// or its `[tool.pytest.ini_options]` but not both — and a native table's
    /// `pythonpath` only as an array (`py9_native`, `py9_pytesttoml`, and each
    /// case run under pytest 9.1.1).
    #[test]
    fn pytest_reads_the_first_configuration_file_pytest_9_reads() {
        let cases = [
            (
                "n",
                &[(
                    "pyproject.toml",
                    "[project]\nname = \"n\"\n\n[tool.pytest]\npythonpath = [\"lib\"]\n",
                )][..],
                true,
            ),
            (
                "t",
                &[("pytest.toml", "[pytest]\npythonpath = [\"lib\"]\n")],
                true,
            ),
            (
                "d",
                &[(".pytest.ini", "[pytest]\npythonpath = lib\n")],
                true,
            ),
            (
                "dt",
                &[(".pytest.toml", "[pytest]\npythonpath = [\"lib\"]\n")],
                true,
            ),
            // An empty `pytest.toml` is pytest's configuration all the same.
            (
                "o",
                &[
                    ("pytest.toml", ""),
                    ("pytest.ini", "[pytest]\npythonpath = lib\n"),
                ],
                false,
            ),
            (
                "p",
                &[
                    ("pytest.ini", "[pytest]\n"),
                    (
                        "pyproject.toml",
                        "[tool.pytest.ini_options]\npythonpath = [\"lib\"]\n",
                    ),
                ],
                false,
            ),
            (
                "b",
                &[(
                    "pyproject.toml",
                    "[tool.pytest]\npythonpath = [\"lib\"]\n\n[tool.pytest.ini_options]\naddopts = \"-q\"\n",
                )],
                false,
            ),
            (
                "s",
                &[("pyproject.toml", "[tool.pytest]\npythonpath = \"lib\"\n")],
                false,
            ),
            (
                "x",
                &[
                    ("tox.ini", "[pytest]\npythonpath = lib\n"),
                    ("setup.cfg", "[tool:pytest]\npythonpath = other\n"),
                ],
                true,
            ),
        ];
        let mut nodes = Vec::new();
        let mut files = Vec::new();
        for (directory, configuration, _) in cases {
            for (rel, source) in [
                ("tests/test_a.py", "import helpers\n"),
                ("lib/helpers.py", ""),
                ("other/helpers.py", ""),
            ] {
                let path = format!("{directory}/{rel}");
                nodes.push(node_in(nodes.len(), &path, source, Language::Python));
            }
            for (file, contents) in configuration {
                files.push((format!("{directory}/{file}"), *contents));
            }
        }
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, contents)| (path.as_str(), *contents))
            .collect();
        let manifests = manifests_of(&files, &nodes);
        for (at, (directory, _, found)) in cases.iter().enumerate() {
            let expected: Vec<String> = found
                .then(|| format!("{directory}/lib/helpers.py"))
                .into_iter()
                .collect();
            assert_eq!(
                edges_under(&nodes, at * 3, &manifests),
                expected,
                "{directory}"
            );
        }
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

    /// pytest's default `prepend` import mode puts a test module's basedir at
    /// `sys.path[0]`, so a module there wins over every other directory's;
    /// `append` puts it after them, and `importlib` nowhere.
    #[test]
    fn a_test_modules_basedir_comes_first_or_last_by_pytests_import_mode() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            python(0, "utils.py", ""),
            python(1, "tests/utils.py", ""),
            python(2, "tests/test_a.py", "from utils import u\n"),
            python(3, "tests/helper.py", "from utils import u\n"),
            python(4, "app/utils.py", ""),
            python(5, "app/tests/utils.py", ""),
            python(6, "app/tests/test_b.py", "import utils\nimport only_here\n"),
            python(7, "app/tests/only_here.py", ""),
            python(8, "iso/utils.py", ""),
            python(9, "iso/tests/utils.py", ""),
            python(10, "iso/tests/test_c.py", "import utils\n"),
            python(11, "suites/unit/__init__.py", ""),
            python(12, "suites/unit/test_d.py", "import shared_util\n"),
            python(13, "suites/shared_util.py", ""),
            python(14, "shared_util.py", ""),
        ];
        let manifests = manifests_of(
            &[
                ("app/pyproject.toml", "[project]\nname = \"app\"\n"),
                (
                    "app/pytest.ini",
                    "[pytest]\naddopts = --import-mode=append\n",
                ),
                ("iso/pyproject.toml", "[project]\nname = \"iso\"\n"),
                (
                    "iso/pytest.ini",
                    "[pytest]\naddopts = --import-mode importlib\n",
                ),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(edges(2), vec!["tests/utils.py"]);
        // A module pytest does not import as a test has no basedir inserted.
        assert_eq!(edges(3), vec!["utils.py"]);
        assert_eq!(edges(6), vec!["app/utils.py", "app/tests/only_here.py"]);
        assert_eq!(edges(10), vec!["iso/utils.py"]);
        // Inside a package, the basedir is the first directory upward
        // without `__init__.py`.
        assert_eq!(edges(12), vec!["suites/shared_util.py"]);
    }

    /// pytest imports each `conftest.py` it loads for a test module — in the
    /// module's directory and those above it, down from the directory of the
    /// configuration it runs under, else of the `pyproject.toml` it falls
    /// back to, else the run's root — with the run's import mode, so the
    /// conftests' basedirs are on the module's `sys.path` too: under
    /// `prepend` after its own basedir, deepest first, under `append` before
    /// it, outermost first, under `importlib` nowhere (`pyconf`,
    /// `pyconf_ini`, and each case run under pytest 9.1.1).
    #[test]
    fn a_test_module_imports_from_the_basedirs_of_the_conftests_pytest_loads_for_it() {
        let python = |id, rel, source| node_in(id, rel, source, Language::Python);
        let nodes = vec![
            // `pyconf`: a service whose pytest-only pyproject makes no project.
            python(0, "svc/conftest.py", ""),
            python(1, "svc/app/__init__.py", ""),
            python(2, "svc/app/core.py", ""),
            python(3, "svc/tests/test_a.py", "from app.core import c\n"),
            // `pyconf_ini`: a service run by its `pytest.ini`.
            python(4, "ini/conftest.py", ""),
            python(5, "ini/app/__init__.py", ""),
            python(6, "ini/app/core.py", ""),
            python(7, "ini/tests/test_b.py", "from app.core import c\n"),
            // `prepend`: the deepest conftest's basedir before an outer one's.
            python(8, "pre/conftest.py", ""),
            python(9, "pre/helpers.py", ""),
            python(10, "pre/only_root.py", ""),
            python(11, "pre/sub/conftest.py", ""),
            python(12, "pre/sub/helpers.py", ""),
            python(
                13,
                "pre/sub/tests/test_x.py",
                "import helpers\nimport only_root\n",
            ),
            // `append`: a conftest's basedir before the module's own.
            python(14, "app/sub/conftest.py", ""),
            python(15, "app/sub/helpers.py", ""),
            python(16, "app/sub/tests/helpers.py", ""),
            python(17, "app/sub/tests/test_y.py", "import helpers\n"),
            python(18, "imp/sub/conftest.py", ""),
            python(19, "imp/sub/helpers.py", ""),
            python(20, "imp/sub/tests/test_z.py", "import helpers\n"),
            // A conftest above where conftests load is not loaded: those of
            // a configuration's directory, of a `pyproject.toml` pytest falls
            // back to, and otherwise of the project root.
            python(21, "proj/x/conftest.py", ""),
            python(22, "proj/x/mod_x.py", ""),
            python(23, "proj/x/inner/tests/test_q.py", "import mod_x\n"),
            python(24, "proj/x/fb/tests/test_f.py", "import mod_x\n"),
            python(25, "proj/x/plain/tests/test_p.py", "import mod_x\n"),
            // A module pytest does not import as a test has no basedir, and
            // no conftest's.
            python(26, "svc/tests/helper.py", "from app.core import c\n"),
            // Without a configuration file, a `setup.py` roots the run.
            python(27, "up/conftest.py", ""),
            python(28, "up/mod_up.py", ""),
            python(29, "up/sp/tests/test_s.py", "import mod_up\n"),
            python(30, "up/plain/tests/test_p.py", "import mod_up\n"),
            // A conftest inside a package: its basedir is the directory
            // above the package (`pyconf_pkg`).
            python(31, "cp/pkg/svc/__init__.py", ""),
            python(32, "cp/pkg/svc/conftest.py", ""),
            python(33, "cp/pkg/svc/core.py", ""),
            python(34, "cp/pkg/svc/tests/test_a.py", "from svc.core import c\n"),
        ];
        let manifests = manifests_of(
            &[
                (
                    "svc/pyproject.toml",
                    "[tool.pytest.ini_options]\naddopts = \"-q\"\n",
                ),
                ("ini/pytest.ini", "[pytest]\naddopts = -q\n"),
                ("pre/pytest.ini", "[pytest]\naddopts = -q\n"),
                (
                    "app/pytest.ini",
                    "[pytest]\naddopts = --import-mode=append\n",
                ),
                (
                    "imp/pytest.ini",
                    "[pytest]\naddopts = --import-mode=importlib\n",
                ),
                ("proj/pyproject.toml", "[project]\nname = \"proj\"\n"),
                ("proj/x/inner/pytest.ini", "[pytest]\n"),
                (
                    "proj/x/fb/pyproject.toml",
                    "[tool.ruff]\nline-length = 100\n",
                ),
                (
                    "up/sp/setup.py",
                    "from setuptools import setup\nsetup(name='sp')\n",
                ),
                ("cp/pytest.ini", "[pytest]\n"),
            ],
            &nodes,
        );
        let edges = |id| edges_under(&nodes, id, &manifests);
        assert_eq!(edges(3), vec!["svc/app/core.py"]);
        assert_eq!(edges(7), vec!["ini/app/core.py"]);
        assert_eq!(edges(13), vec!["pre/sub/helpers.py", "pre/only_root.py"]);
        assert_eq!(edges(17), vec!["app/sub/helpers.py"]);
        assert!(edges(20).is_empty());
        assert!(edges(23).is_empty() && edges(24).is_empty());
        assert_eq!(edges(25), vec!["proj/x/mod_x.py"]);
        assert!(edges(26).is_empty());
        assert!(edges(29).is_empty());
        assert_eq!(edges(30), vec!["up/mod_up.py"]);
        assert_eq!(edges(34), vec!["cp/pkg/svc/core.py"]);
    }

    /// Installing a project installs the projects in the tree it depends on
    /// through its tooling — a uv workspace member its sources mark
    /// `{ workspace = true }`, a uv or Poetry `path` dependency on a directory
    /// any build configuration makes a project — and theirs, so their
    /// packages are on its `sys.path`; a member nothing it installs depends
    /// on is not, nor a directory that is no project, nor a directory the
    /// workspace's `exclude` globs leave out of it.
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
                     exclude = [\"packages/other\"]\n\
                     [tool.uv.sources]\nfixture-core = { workspace = true }\n\
                     other_pkg = { workspace = true }\n",
                ),
                (
                    "packages/app/pyproject.toml",
                    "[project]\nname = \"fixture-app\"\n\
                     dependencies = [\"Fixture_Core>=1.0\", \"legacy\", \"loose\", \"other-pkg\"]\n\
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
                // The majors before 7, which read `baseUrl`.
                (
                    "web/package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
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

    /// TypeScript 7 reads no `baseUrl`: tsc 7.0.2 reports TS5102, resolves
    /// nothing below it and `paths` against the configuration's own
    /// directory, where 5.9.3 and 6.0.3 resolve both against `baseUrl`
    /// (`baseurl7`). So where no TypeScript major is installed or declared —
    /// none, or a range fixing none (`latest`) — a configuration setting
    /// `baseUrl` is read under 5 and 6 alone, as it is where the version
    /// installed is no number (`nonumber`). Under a major above 7, installed
    /// (`inst8`) or declared (`caret8`), and a range admitting only 7 and
    /// above (`>=7`, `7.x || 8.x`), it keeps 7, and a specifier `baseUrl`
    /// decides is disclosed, with the range where one is declared. A set of
    /// comparators within one major fixes it (`ts_range`).
    #[test]
    fn typescript_7_reads_no_base_url() {
        let source = "import a from \"lib/x\";\nimport b from \"@/lib/x\";\n";
        let mut nodes = Vec::new();
        let mut configs = Vec::new();
        for (app, range, installed) in [
            ("six", Some("^6.0.3"), None),
            ("seven", Some("^7.0.2"), None),
            ("none", None, None),
            ("latest", Some("latest"), None),
            ("range", Some(">=5.0.0 <6.0.0"), None),
            ("ge7", Some(">=7"), None),
            ("ge7or8", Some("7.x || 8.x"), None),
            ("caret8", Some("^8.0.0"), None),
            ("inst8", None, Some("8.0.0")),
            ("nonumber", None, Some("next")),
        ] {
            nodes.push(node_in(
                nodes.len(),
                &format!("{app}/app/a.ts"),
                source,
                Language::TypeScript,
            ));
            for file in ["src/lib/x.ts", "lib/x.ts"] {
                nodes.push(node_in(
                    nodes.len(),
                    &format!("{app}/{file}"),
                    "",
                    Language::TypeScript,
                ));
            }
            configs.push((
                format!("{app}/tsconfig.json"),
                "{\"compilerOptions\": {\"baseUrl\": \"./src\", \"paths\": {\"@/*\": [\"./*\"]}, \
                 \"module\": \"esnext\", \"moduleResolution\": \"bundler\"}}"
                    .to_string(),
            ));
            if let Some(range) = range {
                configs.push((
                    format!("{app}/package.json"),
                    format!("{{\"devDependencies\": {{\"typescript\": \"{range}\"}}}}"),
                ));
            }
            if let Some(version) = installed {
                configs.push((
                    format!("{app}/node_modules/typescript/package.json"),
                    format!("{{\"name\": \"typescript\", \"version\": \"{version}\"}}"),
                ));
            }
        }
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(edges(0), vec!["six/src/lib/x.ts"]);
        assert_eq!(edges(3), vec!["seven/lib/x.ts"]);
        assert_eq!(edges(6), vec!["none/src/lib/x.ts"]);
        assert_eq!(edges(9), vec!["latest/src/lib/x.ts"]);
        assert_eq!(edges(12), vec!["range/src/lib/x.ts"]);
        for kept in [15, 18, 21, 24] {
            assert!(edges(kept).is_empty(), "{}", nodes[kept].rel_path);
        }
        assert_eq!(edges(27), vec!["nonumber/src/lib/x.ts"]);
        assert!(graph.undetermined_imports.is_empty());
        assert_eq!(
            graph.version_undetermined_imports,
            BTreeSet::from(["@/lib/x".to_string(), "lib/x".to_string()])
        );
        assert_eq!(
            graph.typescript_ranges,
            BTreeSet::from([
                ">=7".to_string(),
                "7.x || 8.x".to_string(),
                "^8.0.0".to_string()
            ])
        );
        // `inst8` has one installed, which no declared range settles.
        assert!(graph.typescript_not_installed && graph.typescript_installed_unread);
    }

    /// A relative specifier the projects compiling a file read apart is
    /// disclosed as one they read apart, not as a `paths` alias or a
    /// workspace package naming no file, as tsc 5.9.3 and 6.0.3 trace it
    /// (`owners`): `tsconfig.a.json`, whose `module` `amd` reads `classic`,
    /// finds no file for `../components`, which `tsconfig.b.json`'s
    /// `bundler` reads as `components/index.ts`; `./util`, which both read
    /// as `util.ts`, keeps its edge.
    #[test]
    fn a_relative_specifier_the_projects_read_apart_names_its_cause() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let nodes = vec![
            ts(0, "src/a/a.ts", ""),
            ts(1, "src/b/b.ts", ""),
            ts(
                2,
                "src/shared/s.ts",
                "import * as m0 from \"../components\";\nimport * as m1 from \"./util\";\n",
            ),
            ts(3, "src/shared/util.ts", ""),
            ts(4, "src/components/index.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"files\": [], \"references\": [{\"path\": \"./tsconfig.a.json\"}, \
                     {\"path\": \"./tsconfig.b.json\"}]}",
                ),
                (
                    "tsconfig.a.json",
                    "{\"compilerOptions\": {\"composite\": true, \"module\": \"amd\", \"outDir\": \"out/a\"}, \
                     \"include\": [\"src/shared/**/*.ts\", \"src/a/**/*.ts\"]}",
                ),
                (
                    "tsconfig.b.json",
                    "{\"compilerOptions\": {\"composite\": true, \"module\": \"esnext\", \
                     \"moduleResolution\": \"bundler\", \"outDir\": \"out/b\"}, \
                     \"include\": [\"src/shared/**/*.ts\", \"src/b/**/*.ts\"]}",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert_eq!(edges_under(&nodes, 2, &manifests), ["src/shared/util.ts"]);
        assert_eq!(
            graph.projects_undetermined_imports,
            BTreeSet::from(["../components".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
        assert!(graph.version_undetermined_imports.is_empty());
    }

    /// A file several projects compile under different majors and module
    /// options is disclosed as their configurations disagreeing, which no
    /// installed or declared version settles, not as one the TypeScript
    /// version decides (`owners_majors`): the specifier as one the projects
    /// read apart.
    #[test]
    fn a_file_projects_of_different_majors_read_apart_is_no_version_disagreement() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let nodes = vec![
            ts(0, "apps/web/a/ax.ts", ""),
            ts(1, "apps/web/b/bx.ts", ""),
            ts(2, "apps/web/shared/s.ts", "import { p } from 'p'\n"),
            ts(3, "packages/p/src/index.ts", ""),
            ts(4, "packages/p/src/other.ts", ""),
        ];
        let project = |major: &str, options: &str| {
            [
                format!("{{\"devDependencies\": {{\"typescript\": \"{major}\"}}}}"),
                format!(
                    "{{\"compilerOptions\": {{\"composite\": true, {options}}}, \
                     \"include\": [\"../shared/**/*.ts\", \"./*.ts\"]}}"
                ),
            ]
        };
        let [a_package, a_config] = project("^5.9.0", "\"module\": \"commonjs\"");
        let [b_package, b_config] = project(
            "^7.0.2",
            "\"module\": \"esnext\", \"moduleResolution\": \"bundler\"",
        );
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"workspaces\": [\"packages/*\", \"apps/*\"]}",
                ),
                ("apps/web/package.json", "{\"name\": \"web\"}"),
                (
                    "apps/web/tsconfig.json",
                    "{\"files\": [], \"references\": [{\"path\": \"./a\"}, {\"path\": \"./b\"}]}",
                ),
                ("apps/web/a/package.json", a_package.as_str()),
                ("apps/web/a/tsconfig.json", a_config.as_str()),
                ("apps/web/b/package.json", b_package.as_str()),
                ("apps/web/b/tsconfig.json", b_config.as_str()),
                (
                    "packages/p/package.json",
                    "{\"name\": \"p\", \"types\": \"./src/index.ts\", \"exports\": \
                     {\".\": {\"import\": \"./src/other.ts\", \"default\": \"./src/index.ts\"}}}",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert!(graph.edges[&2].is_empty());
        assert_eq!(
            graph.projects_undetermined_imports,
            BTreeSet::from(["p".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
        assert!(graph.version_undetermined_imports.is_empty());
        assert_eq!(
            graph.ts_owner_undetermined,
            BTreeSet::from(["apps/web/shared/s.ts".to_string()])
        );
    }

    /// `classic` resolution looks a non-relative specifier up in the
    /// importer's directory and each one above it, as tsc 5.9.3 and 6.0.3
    /// trace it (`classic_walk`): `.ts`, `.tsx` and `.d.ts` first, the
    /// nearest directory first (`lib/near`), a written `.js` stripped
    /// (`lib/other.js`), never a directory's `index` or `.mts`; then, past
    /// `node_modules/@types`, `.js` and `.jsx`, so a JavaScript file gives
    /// way to an `@types` package of its name installed in the project,
    /// which pack does not read: no edge, nothing disclosed (`jq`). A
    /// written `.ts` is stripped alike (`lib/util.ts`). A file on disk that
    /// pack does not read ends the walk with no edge (`lib/gen`, for which
    /// tsc takes `src/app/lib/gen.d.ts`).
    #[test]
    fn classic_resolution_walks_the_importers_directories() {
        let ts = |id, rel: &str| node_in(id, rel, "", Language::TypeScript);
        let js = |id, rel: &str| node_in(id, rel, "", Language::JavaScript);
        let imports = "import a from \"lib/util\";\nimport b from \"lib/near\";\n\
                       import c from \"x/both\";\nimport d from \"lib/other.js\";\n\
                       import e from \"js/thing\";\nimport f from \"jq\";\n\
                       import g from \"dir\";\nimport h from \"m/n\";\n\
                       import i from \"lib/util.ts\";\nimport j from \"lib/gen\";\n";
        let nodes = vec![
            node_in(0, "cw/src/app/main.ts", imports, Language::TypeScript),
            ts(1, "cw/src/lib/util.ts"),
            ts(2, "cw/src/app/lib/near.ts"),
            ts(3, "cw/src/lib/near.ts"),
            ts(4, "cw/src/x/both.ts"),
            ts(5, "cw/src/x/both.d.ts"),
            js(6, "cw/src/js/thing.js"),
            js(7, "cw/src/jq.js"),
            ts(8, "cw/src/dir/index.ts"),
            ts(9, "cw/src/m/n.mts"),
            ts(10, "cw/src/lib/other.ts"),
            ts(11, "cw/src/lib/gen.ts"),
        ];
        let manifests = manifests_of(
            &[
                (
                    "cw/package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "cw/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"amd\", \"outFile\": \"out.js\"}}",
                ),
                (
                    "cw/node_modules/@types/jq/index.d.ts",
                    "export declare const jq: string;",
                ),
                (
                    "cw/src/app/lib/gen.d.ts",
                    "export declare const gen: string;",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(
            edges,
            vec![
                "cw/src/lib/util.ts",
                "cw/src/app/lib/near.ts",
                "cw/src/x/both.ts",
                "cw/src/lib/other.ts",
                "cw/src/js/thing.js"
            ]
        );
        assert!(graph.undetermined_imports.is_empty());
        assert!(graph.declaration_undetermined_imports.is_empty());
        assert!(graph.version_undetermined_imports.is_empty());
    }

    /// Between its passes `classic` looks the specifier's whole path up
    /// below `node_modules/@types`, as tsc 5.9.3 and 6.0.3 trace it
    /// (`types_sub`). Above the project's root, where pack checks the disk
    /// alone, a JavaScript file the walk finds is disclosed where one may
    /// stand for it: a declaration it names (`fd/sub` as `fd/sub.d.ts`,
    /// `flat` as `flat.d.ts`, `@acme/e/sub` as `acme__e/sub.d.ts`), a
    /// directory holding an `index.d.ts`
    /// (`dd/sub`) or a `package.json` naming its types (`pj/sub`), or a
    /// subpath of a package with `typesVersions` (`tv/sub`). A package
    /// holding no such subpath (`lib/util`, `@acme/d/sub` below `acme__d`),
    /// a directory holding no declaration (`empty`) and a `package.json`
    /// naming none (`pkgonly`) leave the JavaScript file the one, as tsc
    /// takes it.
    #[test]
    fn classic_resolution_looks_the_specifiers_whole_path_up_below_types() {
        let specifiers = [
            "lib/util",
            "@acme/d/sub",
            "@acme/e/sub",
            "fd/sub",
            "flat",
            "dd/sub",
            "pj/sub",
            "tv/sub",
            "empty",
            "pkgonly",
        ];
        let source: String = specifiers
            .iter()
            .map(|specifier| format!("import \"{specifier}\";\n"))
            .collect();
        let mut nodes = vec![node_in(0, "src/app/main.ts", &source, Language::TypeScript)];
        for rel in [
            "src/lib/util.js",
            "src/@acme/d/sub.js",
            "src/@acme/e/sub.js",
            "src/fd/sub.js",
            "src/flat.js",
            "src/dd/sub.js",
            "src/pj/sub.js",
            "src/tv/sub.js",
            "src/empty.js",
            "src/pkgonly.js",
        ] {
            nodes.push(node_in(nodes.len(), rel, "", Language::JavaScript));
        }
        let declaration = "export declare const v: number;";
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in [
            (
                "outer/ts/package.json",
                "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
            ),
            (
                "outer/ts/tsconfig.json",
                "{\"compilerOptions\": {\"moduleResolution\": \"classic\", \"module\": \"esnext\"}}",
            ),
            ("outer/node_modules/@types/lib/index.d.ts", declaration),
            ("outer/node_modules/@types/acme__d/index.d.ts", declaration),
            ("outer/node_modules/@types/acme__e/sub.d.ts", declaration),
            ("outer/node_modules/@types/fd/sub.d.ts", declaration),
            ("outer/node_modules/@types/flat.d.ts", declaration),
            ("outer/node_modules/@types/dd/sub/index.d.ts", declaration),
            (
                "outer/node_modules/@types/pj/sub/package.json",
                "{\"types\": \"x.d.ts\"}",
            ),
            ("outer/node_modules/@types/pj/sub/x.d.ts", declaration),
            (
                "outer/node_modules/@types/tv/package.json",
                "{\"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
            ),
            ("outer/node_modules/@types/tv/ts/sub.d.ts", declaration),
            ("outer/node_modules/@types/empty/README.md", ""),
            (
                "outer/node_modules/@types/pkgonly/package.json",
                "{\"name\": \"@types/pkgonly\"}",
            ),
        ] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        let manifests = Manifests::read(&dir.path().join("outer/ts"), &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(
            edges,
            vec![
                "src/lib/util.js",
                "src/@acme/d/sub.js",
                "src/empty.js",
                "src/pkgonly.js"
            ]
        );
        assert!(graph.undetermined_imports.is_empty());
        assert_eq!(
            graph.declaration_undetermined_imports,
            [
                "@acme/e/sub",
                "fd/sub",
                "flat",
                "dd/sub",
                "pj/sub",
                "tv/sub"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
        );
    }

    /// After `node_modules/@types`, `classic` looks in each directory the
    /// chain's `typeRoots` names, against the configuration declaring it,
    /// as tsc 5.9.3 and 6.0.3 trace it (`typeroots`, `typeroots_nm`,
    /// `tru`): the specifier's whole path below it (`lib2/util` as
    /// `lib2/util.d.ts`), a scope kept as written (`@sc/x`, where
    /// `sc__y` names nothing for `@sc/y`) but where the root is itself a
    /// `node_modules/@types` (`@sc/z` as `sc__z`), and a root above the
    /// project's (`up`). A declaration one in the project holds is the one
    /// tsc takes over the walk's JavaScript file: an edge where pack reads
    /// it (`rp7/classic_typeroots`'s `jq`), none and nothing disclosed where
    /// it is only on disk (`@sc/z`). A JavaScript file the walk finds is
    /// disclosed where a root outside the project may hold a declaration
    /// for it (`up`), and is the one where none does (`emp`, `@sc/y`). A
    /// `node_modules/@types` above the project's root comes before every
    /// type root: its declaration, which pack does not read, stands for
    /// `ot` over the one a root in the project holds, and forms no edge.
    #[test]
    fn classic_resolution_looks_in_the_type_roots_after_types() {
        let dir = tempfile::tempdir().expect("tempdir");
        let declaration = "export declare const v: number;";
        for (path, contents) in [
            (
                "outer/repo/package.json",
                "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
            ),
            (
                "outer/repo/tsconfig.json",
                "{\"extends\": \"./base/tsconfig.base.json\", \"compilerOptions\": \
                 {\"moduleResolution\": \"classic\", \"module\": \"esnext\"}}",
            ),
            (
                "outer/repo/base/tsconfig.base.json",
                "{\"compilerOptions\": {\"typeRoots\": [\"./typings\", \"../../types\", \
                 \"./nm/node_modules/@types\"]}}",
            ),
            ("outer/repo/base/typings/emp/README.md", ""),
            ("outer/types/up/index.d.ts", declaration),
            ("outer/node_modules/@types/ot/index.d.ts", declaration),
            (
                "outer/repo/base/nm/node_modules/@types/sc__z/index.d.ts",
                declaration,
            ),
        ] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        let specifiers = [
            "jq",
            "lib2/util",
            "@sc/x",
            "@sc/y",
            "up",
            "@sc/z",
            "emp",
            "ot",
        ];
        let source: String = specifiers
            .iter()
            .map(|specifier| format!("import \"{specifier}\";\n"))
            .collect();
        let mut nodes = vec![node_in(0, "src/app/main.ts", &source, Language::TypeScript)];
        for rel in [
            "src/jq.js",
            "src/lib2/util.js",
            "src/@sc/x.js",
            "src/@sc/y.js",
            "src/up.js",
            "src/@sc/z.js",
            "src/emp.js",
        ] {
            nodes.push(node_in(nodes.len(), rel, "", Language::JavaScript));
        }
        for rel in [
            "base/typings/jq/index.d.ts",
            "base/typings/lib2/util.d.ts",
            "base/typings/@sc/x/index.d.ts",
            "base/typings/sc__y/index.d.ts",
            "base/typings/ot/index.d.ts",
        ] {
            nodes.push(node_in(nodes.len(), rel, declaration, Language::TypeScript));
        }
        let manifests = Manifests::read(&dir.path().join("outer/repo"), &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(
            edges,
            vec![
                "base/typings/jq/index.d.ts",
                "base/typings/lib2/util.d.ts",
                "base/typings/@sc/x/index.d.ts",
                "src/@sc/y.js",
                "src/emp.js"
            ]
        );
        assert!(graph.undetermined_imports.is_empty());
        assert_eq!(
            graph.declaration_undetermined_imports,
            BTreeSet::from(["up".to_string()])
        );
    }

    /// `classic` takes a declaration the project holds in a
    /// `node_modules/@types` or a `typeRoots` directory over the walk's
    /// JavaScript file, as tsc 5.9.3 and 6.0.3 trace it (`tq2_classic`): the
    /// edge to it where pack reads it, a scope mangled (`@acme/d` as
    /// `acme__d`) and a `types` naming a TypeScript file naming it (`pj`,
    /// `tt2`); a subpath through `typesVersions`, which pack does not
    /// follow, is disclosed (`tv/sub`).
    #[test]
    fn classic_takes_a_declaration_the_project_holds() {
        let source = "import \"@acme/d\";\nimport \"pj\";\nimport \"tv/sub\";\nimport \"tt2\";\n";
        let mut nodes = vec![node_in(0, "src/app/main.ts", source, Language::TypeScript)];
        for rel in ["src/@acme/d.js", "src/pj.js", "src/tv/sub.js", "src/tt2.js"] {
            nodes.push(node_in(nodes.len(), rel, "", Language::JavaScript));
        }
        for rel in [
            "node_modules/@types/acme__d/index.d.ts",
            "node_modules/@types/pj/lib/t.ts",
            "typings/tt2/lib/t.ts",
        ] {
            nodes.push(node_in(nodes.len(), rel, "", Language::TypeScript));
        }
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"classic\", \
                     \"module\": \"esnext\", \"typeRoots\": [\"./typings\"]}}",
                ),
                (
                    "node_modules/@types/pj/package.json",
                    "{\"types\": \"lib/t.ts\"}",
                ),
                (
                    "node_modules/@types/tv/package.json",
                    "{\"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                ),
                ("node_modules/@types/tv/ts/sub.d.ts", ""),
                ("typings/tt2/package.json", "{\"types\": \"lib/t.ts\"}"),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(
            edges,
            [
                "node_modules/@types/acme__d/index.d.ts",
                "node_modules/@types/pj/lib/t.ts",
                "typings/tt2/lib/t.ts"
            ]
        );
        assert_eq!(
            graph.declaration_undetermined_imports,
            BTreeSet::from(["tv/sub".to_string()])
        );
    }

    /// Outside `classic`, TypeScript looks in the `typeRoots` after
    /// `node_modules` in a pass whose extensions hold declarations, as tsc
    /// 5.9.3, 6.0.3 and 7.0.2 trace it (`tr_*`): a declaration a root in the
    /// project holds is the edge where no package or `@types` declaration
    /// there takes the specifier first (`jq`, `lib2/util`, `@sc/x`, `pk`
    /// through its `package.json`, and `nm`, a package holding nothing).
    /// `node10` makes that pass before the JavaScript one, so a package of
    /// JavaScript alone gives way to it (`ul`, `ex`, whose `exports` it does
    /// not read), where `bundler` and `node16` take the package's file in
    /// their one pass (`ul`), an `exports` pack does not follow leaving it
    /// open (`ex`), as does a `typesVersions` (`tv`, which 5.9.3 and 6.0.3
    /// pass over and 7.0.2 does not). A package that is a JavaScript file
    /// (`tf`) gives way under `node10` alike (`tf_*`). A package with its own
    /// declaration (`tl`, `tt` through its `types`) or one in `@types`
    /// (`at`) comes first in every mode. A `types` naming a TypeScript file
    /// names it (`tn`). A package whose `types` names no file is read by its
    /// `main` in JavaScript's pass, which `bundler` and `node16` make before
    /// the type root and `node10` after it (`tm`, `q3c_*`). `node16` for
    /// an ES import adds no extension and reads no `index` there, nor in
    /// `node_modules` but for a package's name whose `package.json` exists,
    /// which tsc's `index.js` fallback reads (`ei`, `p1c_*`), not for one
    /// without (`en`, `q1b_*`) or for a subpath (`es/sub`, `q1e_*`), so
    /// only `pk`, `tn`, `en`, `es/sub` and `@sc/q` name one. A declaration
    /// a root holds where pack does not read it, beside a package whose
    /// `exports` pack does not follow, forms no edge and is not disclosed,
    /// as neither is a file pack reads (`ox` in `./node_modules/@org/types`);
    /// a root that is no `node_modules/@types` keeps a scope as written
    /// (`@sc/q` in `./src/@types`). A subpath below a type root is not
    /// read through the `typesVersions` of the `package.json` above it,
    /// which only a `node_modules` package's subpath goes through (`vs/sub`).
    #[test]
    fn a_type_root_declaration_outside_classic_comes_after_node_modules() {
        let specifiers = [
            "jq",
            "ul",
            "tl",
            "at",
            "lib2/util",
            "pk",
            "ex",
            "@sc/x",
            "nm",
            "tf",
            "tt",
            "tv",
            "tn",
            "tm",
            "ei",
            "en",
            "es/sub",
            "ox",
            "@sc/q",
            "vs/sub",
        ];
        let source: String = specifiers
            .iter()
            .map(|specifier| format!("import \"{specifier}\";\n"))
            .collect();
        let typings = [
            "jq/index.d.ts",
            "ul/index.d.ts",
            "tl/index.d.ts",
            "at/index.d.ts",
            "lib2/util.d.ts",
            "pk/main.d.ts",
            "ex/index.d.ts",
            "@sc/x/index.d.ts",
            "nm/index.d.ts",
            "tf/index.d.ts",
            "tt/index.d.ts",
            "tv/index.d.ts",
            "tn/lib/t.ts",
            "tm/index.d.ts",
            "ei/index.d.ts",
            "en/index.d.ts",
            "es/sub/index.d.ts",
            "vs/sub/index.d.ts",
        ];
        let modes = [
            (
                "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
                None,
            ),
            (
                "\"moduleResolution\": \"bundler\", \"module\": \"esnext\"",
                None,
            ),
            (
                "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
                None,
            ),
            (
                "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
                Some("module"),
            ),
        ];
        let mut read = Vec::new();
        for (options, kind) in modes {
            let mut nodes = vec![node_in(0, "src/app/main.ts", &source, Language::TypeScript)];
            for file in typings {
                let path = format!("typings/{file}");
                nodes.push(node_in(nodes.len(), &path, "", Language::TypeScript));
            }
            let scoped = "src/@types/@sc/q/index.d.ts";
            nodes.push(node_in(nodes.len(), scoped, "", Language::TypeScript));
            let package = match kind {
                Some(kind) => format!(
                    "{{\"type\": \"{kind}\", \"devDependencies\": {{\"typescript\": \"^5.9.0\"}}}}"
                ),
                None => "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}".to_string(),
            };
            let config = format!(
                "{{\"compilerOptions\": {{{options}, \"allowJs\": true, \
                 \"typeRoots\": [\"./typings\", \"./node_modules/@org/types\", \"./src/@types\"]}}}}"
            );
            let manifests = manifests_of(
                &[
                    ("package.json", &package),
                    ("tsconfig.json", &config),
                    ("typings/pk/package.json", "{\"types\": \"main.d.ts\"}"),
                    (
                        "node_modules/ul/package.json",
                        "{\"name\": \"ul\", \"main\": \"index.js\"}",
                    ),
                    ("node_modules/ul/index.js", ""),
                    (
                        "node_modules/tl/package.json",
                        "{\"name\": \"tl\", \"types\": \"index.d.ts\"}",
                    ),
                    ("node_modules/tl/index.d.ts", ""),
                    ("node_modules/@types/at/index.d.ts", ""),
                    (
                        "node_modules/ex/package.json",
                        "{\"name\": \"ex\", \"exports\": {\".\": \"./index.js\"}}",
                    ),
                    ("node_modules/ex/index.js", ""),
                    ("node_modules/nm/package.json", "{\"name\": \"nm\"}"),
                    ("node_modules/tf.js", ""),
                    (
                        "node_modules/tt/package.json",
                        "{\"name\": \"tt\", \"types\": \"lib/x.d.ts\"}",
                    ),
                    ("node_modules/tt/lib/x.d.ts", ""),
                    (
                        "node_modules/tv/package.json",
                        "{\"name\": \"tv\", \"main\": \"index.js\", \
                         \"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                    ),
                    ("node_modules/tv/index.js", ""),
                    ("typings/tn/package.json", "{\"types\": \"lib/t.ts\"}"),
                    ("typings/tm/package.json", "{\"types\": \"index.d.ts\"}"),
                    (
                        "node_modules/tm/package.json",
                        "{\"name\": \"tm\", \"types\": \"./dist/index.d.ts\", \"main\": \"./lib/index.js\"}",
                    ),
                    ("node_modules/tm/lib/index.js", ""),
                    ("typings/ei/package.json", "{\"types\": \"index.d.ts\"}"),
                    ("node_modules/ei/package.json", "{\"name\": \"ei\"}"),
                    ("node_modules/ei/index.js", ""),
                    ("typings/en/package.json", "{\"types\": \"index.d.ts\"}"),
                    ("node_modules/en/index.js", ""),
                    ("typings/es/sub/package.json", "{\"types\": \"index.d.ts\"}"),
                    ("node_modules/es/package.json", "{\"name\": \"es\"}"),
                    ("node_modules/es/sub/index.js", ""),
                    ("node_modules/@org/types/ox/index.d.ts", ""),
                    (
                        "node_modules/ox/package.json",
                        "{\"name\": \"ox\", \"exports\": {\".\": \"./index.js\"}}",
                    ),
                    ("node_modules/ox/index.js", ""),
                    (
                        "src/@types/@sc/q/package.json",
                        "{\"types\": \"index.d.ts\"}",
                    ),
                    (
                        "typings/vs/package.json",
                        "{\"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                    ),
                    ("typings/vs/ts/sub/index.d.ts", ""),
                ],
                &nodes,
            );
            let graph = build_import_graph(&nodes, &manifests);
            let edges: BTreeSet<&str> = graph
                .edges
                .get(&0)
                .into_iter()
                .flatten()
                .map(|dst| {
                    let path = nodes[*dst].rel_path.as_str();
                    path.strip_prefix("typings/").unwrap_or(path)
                })
                .collect();
            let edges: BTreeSet<String> = edges.into_iter().map(str::to_string).collect();
            read.push((edges, graph.type_root_undetermined_imports.clone()));
            assert!(graph.undetermined_imports.is_empty(), "{options}");
            assert!(
                graph.declaration_undetermined_imports.is_empty(),
                "{options}"
            );
        }
        let set = |items: &[&str]| -> BTreeSet<String> {
            items.iter().map(|item| item.to_string()).collect()
        };
        let common = [
            "jq/index.d.ts",
            "lib2/util.d.ts",
            "pk/main.d.ts",
            "@sc/x/index.d.ts",
            "nm/index.d.ts",
            "tn/lib/t.ts",
            "src/@types/@sc/q/index.d.ts",
            "vs/sub/index.d.ts",
        ];
        let node10 = [
            &common[..],
            &[
                "ul/index.d.ts",
                "ex/index.d.ts",
                "tf/index.d.ts",
                "tm/index.d.ts",
                "ei/index.d.ts",
                "en/index.d.ts",
                "es/sub/index.d.ts",
            ],
        ]
        .concat();
        assert_eq!(read[0], (set(&node10), set(&["tv"])));
        assert_eq!(read[1], (set(&common), set(&["ex", "tv"])));
        assert_eq!(read[2], (set(&common), set(&["ex", "tv"])));
        let esm = [
            "pk/main.d.ts",
            "tn/lib/t.ts",
            "en/index.d.ts",
            "es/sub/index.d.ts",
            "src/@types/@sc/q/index.d.ts",
        ];
        assert_eq!(read[3], (set(&esm), set(&[])));
    }

    /// The `node_modules` lookup before a type root is kept apart per
    /// directory and per module resolution, as tsc 5.9.3, 6.0.3 and 7.0.2
    /// trace it (`q6b_node16_per_directory`): under `node16` an ES import of
    /// `en`, whose package holds no `package.json`, reads no `index` and
    /// takes the type root's declaration, where a CommonJS one from the same
    /// directory takes the package's `index.js`; `nested`, which a
    /// `node_modules` in `src/app` declares, is that one's there and the
    /// type root's from `src`.
    #[test]
    fn node_modules_lookups_are_kept_apart_per_directory_and_resolution() {
        let source = |specifier| format!("import \"{specifier}\";\n");
        let nodes = vec![
            node_in(0, "src/app/a.mts", &source("en"), Language::TypeScript),
            node_in(1, "src/app/b.cts", &source("en"), Language::TypeScript),
            node_in(2, "src/app/c.cts", &source("nested"), Language::TypeScript),
            node_in(3, "src/d.cts", &source("nested"), Language::TypeScript),
            node_in(4, "typings/en/index.d.ts", "", Language::TypeScript),
            node_in(5, "typings/nested/index.d.ts", "", Language::TypeScript),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"node16\", \
                     \"moduleResolution\": \"node16\", \"typeRoots\": [\"./typings\"]}}",
                ),
                ("typings/en/package.json", "{\"types\": \"index.d.ts\"}"),
                ("typings/nested/package.json", "{\"types\": \"index.d.ts\"}"),
                ("node_modules/en/index.js", ""),
                ("src/app/node_modules/nested/index.d.ts", ""),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph
                .edges
                .get(&id)
                .into_iter()
                .flatten()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(edges(0), vec!["typings/en/index.d.ts"]);
        assert!(edges(1).is_empty());
        assert!(edges(2).is_empty());
        assert_eq!(edges(3), vec!["typings/nested/index.d.ts"]);
    }

    /// The `node_modules` lookup goes on past the project's root, as tsc
    /// 5.9.3, 6.0.3 and 7.0.2 trace it (`q7_nm_above_root_*`): a package of
    /// JavaScript alone installed above the root takes `ul` before the type
    /// root's declaration under `bundler`, and gives way to it under
    /// `node10`.
    #[test]
    fn a_node_modules_above_the_projects_root_comes_before_a_type_root() {
        let modes = [("bundler", "esnext", false), ("node10", "commonjs", true)];
        for (resolution, module, declared) in modes {
            let dir = tempfile::tempdir().expect("tempdir");
            let config = format!(
                "{{\"compilerOptions\": {{\"moduleResolution\": \"{resolution}\", \
                 \"module\": \"{module}\", \"typeRoots\": [\"./typings\"]}}}}"
            );
            for (path, contents) in [
                (
                    "outer/node_modules/ul/package.json",
                    "{\"name\": \"ul\", \"main\": \"index.js\"}",
                ),
                ("outer/node_modules/ul/index.js", ""),
                (
                    "outer/repo/package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                ("outer/repo/tsconfig.json", &config),
            ] {
                let path = dir.path().join(path);
                std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
                std::fs::write(path, contents).expect("write");
            }
            let nodes = vec![
                node_in(0, "src/main.ts", "import \"ul\";\n", Language::TypeScript),
                node_in(1, "typings/ul/index.d.ts", "", Language::TypeScript),
            ];
            let manifests = Manifests::read(&dir.path().join("outer/repo"), &nodes);
            let edges = edges_under(&nodes, 0, &manifests);
            let want = declared.then(|| "typings/ul/index.d.ts".to_string());
            assert_eq!(edges, want.into_iter().collect::<Vec<_>>(), "{resolution}");
        }
    }

    /// A `paths` substitution naming a directory whose `package.json` holds
    /// `typesVersions` is disclosed as such, as a relative path naming one
    /// is: tsc 5.9.3 and 6.0.3 resolve none for `@/pv` and 7.0.2 its `index`
    /// (`pvp_bundler`).
    #[test]
    fn a_paths_substitution_naming_a_types_versions_directory_names_its_cause() {
        let nodes = vec![
            node_in(0, "src/main.ts", "import \"@/pv\";\n", Language::TypeScript),
            node_in(1, "src/pv/index.ts", "", Language::TypeScript),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"bundler\", \
                     \"module\": \"esnext\", \"paths\": {\"@/*\": [\"./src/*\"]}}}",
                ),
                (
                    "src/pv/package.json",
                    "{\"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert!(graph.edges[&0].is_empty());
        assert_eq!(
            graph.types_versions_undetermined_imports,
            BTreeSet::from(["@/pv".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
    }

    /// A `package.json` field a declaration lookup reads names a TypeScript
    /// file too, as tsc 5.9.3, 6.0.3 and 7.0.2 trace it (`tm_bundler`): a
    /// type root's `types` of `lib/t` names `lib/t.ts`. One writing `.ts`
    /// is looked up as its declaration first (`f_field_decl_*`): `tw`'s
    /// `./index.ts` names `index.d.ts` beside it.
    #[test]
    fn a_declaration_lookup_takes_a_typescript_file_a_field_names() {
        let nodes = vec![
            node_in(
                0,
                "src/main.ts",
                "import \"tm\";\nimport \"tw\";\n",
                Language::TypeScript,
            ),
            node_in(1, "typings/tm/lib/t.ts", "", Language::TypeScript),
            node_in(2, "typings/tw/index.ts", "", Language::TypeScript),
            node_in(3, "typings/tw/index.d.ts", "", Language::TypeScript),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"bundler\", \
                     \"module\": \"esnext\", \"typeRoots\": [\"./typings\"]}}",
                ),
                ("typings/tm/package.json", "{\"types\": \"lib/t\"}"),
                ("typings/tw/package.json", "{\"types\": \"./index.ts\"}"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            ["typings/tm/lib/t.ts", "typings/tw/index.d.ts"]
        );
    }

    /// A declaration a `typeRoots` directory outside the project may hold,
    /// which pack checks on disk without reading, is disclosed outside
    /// `classic` only where the rest of the lookup names a file in the
    /// project, which tsc would pass over for it (`up`, whose `paths`
    /// JavaScript file `node10` looks for after the type roots); where it
    /// names none there is nothing to disclose (`none`). A root that is the
    /// `node_modules/@types` of the importer's directory or one above it is
    /// passed over, since TypeScript's `node_modules` walk looks there first:
    /// its `typesVersions` package leaves nothing open (`react`).
    #[test]
    fn a_type_root_outside_the_project_discloses_only_a_file_in_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let declaration = "export declare const v: number;";
        for (path, contents) in [
            (
                "outer/repo/package.json",
                "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
            ),
            (
                "outer/repo/tsconfig.json",
                "{\"compilerOptions\": {\"moduleResolution\": \"node10\", \"module\": \"commonjs\", \
                 \"allowJs\": true, \"typeRoots\": [\"./node_modules/@types\", \"../types\"], \
                 \"paths\": {\"up\": [\"./src/up\"]}}}",
            ),
            ("outer/types/up/index.d.ts", declaration),
            ("outer/types/none/index.d.ts", declaration),
            (
                "outer/repo/node_modules/react/package.json",
                "{\"name\": \"react\", \"main\": \"index.js\"}",
            ),
            ("outer/repo/node_modules/react/index.js", ""),
            (
                "outer/repo/node_modules/@types/react/package.json",
                "{\"types\": \"index.d.ts\", \"typesVersions\": {\"<=5.0\": {\"*\": [\"ts5.0/*\"]}}}",
            ),
            (
                "outer/repo/node_modules/@types/react/index.d.ts",
                declaration,
            ),
        ] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        let nodes = vec![
            node_in(
                0,
                "src/app/main.ts",
                "import \"up\";\nimport \"none\";\nimport \"react\";\n",
                Language::TypeScript,
            ),
            node_in(1, "src/up.js", "", Language::JavaScript),
        ];
        let manifests = Manifests::read(&dir.path().join("outer/repo"), &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        assert!(graph.edges[&0].is_empty());
        assert_eq!(
            graph.type_root_undetermined_imports,
            BTreeSet::from(["up".to_string()])
        );
        assert!(graph.declaration_undetermined_imports.is_empty());
    }

    /// A `classic` lookup is made once for each configuration, importer's
    /// directory and specifier (`ProjectIndex::classic`): two files of one
    /// directory that projects of different `paths` compile read one
    /// specifier each through its own, and a file of another directory
    /// walks from its own (`lib/n`).
    #[test]
    fn classic_lookups_are_kept_apart_per_configuration() {
        let imports = "import \"@x/m\";\nimport \"lib/n\";\n";
        let nodes = vec![
            node_in(0, "mk/src/a.ts", imports, Language::TypeScript),
            node_in(1, "mk/src/b.ts", "import \"@x/m\";\n", Language::TypeScript),
            node_in(2, "mk/one/m.ts", "", Language::TypeScript),
            node_in(3, "mk/two/m.ts", "", Language::TypeScript),
            node_in(4, "mk/src/deep/d.ts", imports, Language::TypeScript),
            node_in(5, "mk/src/lib/n.ts", "", Language::TypeScript),
            node_in(6, "mk/src/deep/lib/n.ts", "", Language::TypeScript),
        ];
        let project = |file: &str, target: &str| {
            format!(
                "{{\"compilerOptions\": {{\"composite\": true, \"module\": \"amd\", \
                 \"paths\": {{\"@x/*\": [\"./{target}/*\"]}}}}, \"files\": [\"{file}\"]}}"
            )
        };
        let (a, b) = (
            project("src/a.ts\", \"src/deep/d.ts", "one"),
            project("src/b.ts", "two"),
        );
        let manifests = manifests_of(
            &[
                (
                    "mk/package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "mk/tsconfig.json",
                    "{\"files\": [], \"references\": [{\"path\": \"./tsconfig.a.json\"}, \
                     {\"path\": \"./tsconfig.b.json\"}]}",
                ),
                ("mk/tsconfig.a.json", a.as_str()),
                ("mk/tsconfig.b.json", b.as_str()),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert_eq!(graph.edges[&0], vec![2, 5]);
        assert_eq!(graph.edges[&1], vec![3]);
        assert_eq!(graph.edges[&4], vec![2, 6]);
    }

    /// Under `classic`, a relative specifier and the paths `paths` and
    /// `baseUrl` name are looked up as the walk is, as tsc 5.9.3 and 6.0.3
    /// trace them (`rel_js`, `paths`, `baseurl`): TypeScript's files in every
    /// place first, `.ts`, `.tsx`, `.d.ts`, then `node_modules/@types`, then
    /// JavaScript's, never a directory's `index` or an `.mts` for no
    /// extension. A written extension gives way to those of the pass:
    /// `./w3.js` names `w3.js.ts` before `w3.js`, `./w4.ts` the `w4.js`
    /// alone there, `./w7.mjs` `w7.mts`. A `paths` substitution writing an
    /// extension names that file first (`@e/foo`), one naming no file is
    /// passed over for the next (`@y/thing`) or the walk (`@n/q`), and a
    /// pattern naming none at all is disclosed (`@x/x`). Below `baseUrl` a
    /// TypeScript file comes before the walk's (`bt`), the walk's before a
    /// JavaScript file there (`lib/x`), `@types` before it too, its
    /// declaration the edge where pack reads it (`jq2`), and a
    /// JavaScript file there before the walk's (`bj`). A file of another
    /// kind a specifier or a substitution spells exactly is still the one
    /// (`./x.vue`, `@x/v/c.vue`).
    #[test]
    fn classic_resolution_reads_relative_paths_and_base_url_specifiers_alike() {
        let file = |id, rel: &str| {
            let language = match rel.rsplit_once('.').map(|(_, extension)| extension) {
                Some("js" | "jsx" | "mjs" | "cjs") => Language::JavaScript,
                Some("vue") => Language::Vue,
                _ => Language::TypeScript,
            };
            node_in(id, rel, "", language)
        };
        let main = |id, rel: &str, specifiers: &[&str]| {
            let source: String = specifiers
                .iter()
                .map(|specifier| format!("import \"{specifier}\";\n"))
                .collect();
            node_in(id, rel, &source, Language::TypeScript)
        };
        let relative = [
            "../components",
            "../m/n",
            "./both",
            "./tsxjs",
            "./dts",
            "./jsxjs",
            "./w3.js",
            "./w4.ts",
            "./w7.mjs",
            "./w10.d.ts",
            "./x.vue",
        ];
        let mapped = [
            "@x/x",
            "@y/thing",
            "@e/foo",
            "@n/q",
            "@x/foo.js",
            "zed",
            "@t/tsx",
            "@x/v/c.vue",
        ];
        let based = ["lib/x", "jq2", "bt", "bj", "components", "m/n"];
        let mut nodes = vec![
            main(0, "rel/src/app/main.ts", &relative),
            main(1, "paths/src/app/main.ts", &mapped),
            main(2, "base/src/app/main.ts", &based),
        ];
        for rel in [
            "rel/src/components/index.ts",
            "rel/src/m/n.mts",
            "rel/src/app/both.ts",
            "rel/src/app/both.js",
            "rel/src/app/tsxjs.tsx",
            "rel/src/app/tsxjs.js",
            "rel/src/app/dts.d.ts",
            "rel/src/app/dts.js",
            "rel/src/app/jsxjs.jsx",
            "rel/src/app/jsxjs.js",
            "rel/src/app/w3.js.ts",
            "rel/src/app/w3.js",
            "rel/src/app/w4.js",
            "rel/src/app/w7.mts",
            "rel/src/app/w7.mjs",
            "rel/src/app/w10.ts",
            "rel/src/app/w10.d.ts",
            "rel/src/app/x.vue",
            "paths/src/x/index.ts",
            "paths/src/y/thing.js",
            "paths/src/app/y/thing.ts",
            "paths/src/e/foo.js",
            "paths/src/e/foo.ts",
            "paths/src/@n/q.ts",
            "paths/src/foo.ts",
            "paths/src/foo.js",
            "paths/types/zed.ts",
            "paths/src/zed.js",
            "paths/src/t/tsx.tsx",
            "paths/src/t/tsx.d.ts",
            "paths/src/v/c.vue",
            "base/src/lib/x.js",
            "base/src/app/lib/x.ts",
            "base/src/jq2.js",
            "base/src/bt.ts",
            "base/src/app/bt.ts",
            "base/src/bj.js",
            "base/src/app/bj.jsx",
            "base/src/components/index.ts",
            "base/src/m/n.mts",
            "base/node_modules/@types/jq2/index.d.ts",
        ] {
            nodes.push(file(nodes.len(), rel));
        }
        let package = "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}";
        let manifests = manifests_of(
            &[
                ("rel/package.json", package),
                (
                    "rel/tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"classic\", \"module\": \"esnext\"}}",
                ),
                ("paths/package.json", package),
                (
                    "paths/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"amd\", \"paths\": {\
                     \"@y/*\": [\"./src/y/*\", \"./src/app/y/*\"], \"@x/*\": [\"./src/*\"], \
                     \"@e/*\": [\"./src/e/*.js\"], \"@n/*\": [\"./src/nothing/*\"], \
                     \"*\": [\"./types/*\"], \"@t/*\": [\"./src/t/*\"]}}}",
                ),
                ("base/package.json", package),
                (
                    "base/tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"classic\", \"module\": \"esnext\", \
                     \"baseUrl\": \"src\"}}",
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
                "rel/src/app/both.ts",
                "rel/src/app/tsxjs.tsx",
                "rel/src/app/dts.d.ts",
                "rel/src/app/jsxjs.js",
                "rel/src/app/w3.js.ts",
                "rel/src/app/w4.js",
                "rel/src/app/w7.mts",
                "rel/src/app/w10.ts",
                "rel/src/app/x.vue",
            ]
        );
        assert_eq!(
            edges(1),
            vec![
                "paths/src/app/y/thing.ts",
                "paths/src/e/foo.js",
                "paths/src/@n/q.ts",
                "paths/src/foo.ts",
                "paths/types/zed.ts",
                "paths/src/t/tsx.tsx",
                "paths/src/v/c.vue",
            ]
        );
        assert_eq!(
            edges(2),
            vec![
                "base/src/app/lib/x.ts",
                "base/node_modules/@types/jq2/index.d.ts",
                "base/src/bt.ts",
                "base/src/bj.js"
            ]
        );
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["@x/x".to_string()])
        );
        assert!(graph.declaration_undetermined_imports.is_empty());
        assert!(graph.version_undetermined_imports.is_empty());
    }

    /// Outside `classic`, TypeScript looks the path a relative specifier or
    /// a `paths` substitution spells up as each module resolution does, as
    /// tsc 5.9.3, 6.0.3 and 7.0.2 trace it (`rel_*`, `dir_*`): a file before
    /// a directory, `.ts`, `.tsx`, `.d.ts`, then `.js`, `.jsx`, a written
    /// extension giving way to those (`./w2.js` as `w2.ts`), and in a
    /// directory the `types` or `main` of its `package.json`, with those
    /// extensions in place of the one it writes, else its `index`. `node10`
    /// under 5 makes the TypeScript pass over the file and the directory
    /// before the JavaScript one, so it takes `dirfile/index.ts` over
    /// `dirfile.js` and `pn/index.ts` over the `main`'s `pn/lib/n.js`, which
    /// `bundler` and `node16` take. `node16` for an ES import adds no
    /// extension and reads no directory: only `./both.js`, `./w2.js` and
    /// `@/both.js` name a file. A `package.json` holding `typesVersions`
    /// leaves the directory open, disclosed as such (`./pv`). A path ending in `/` names a
    /// directory alone: `./x/` is `x/index.ts`, not `x.ts`.
    #[test]
    fn typescript_reads_a_relative_path_by_each_resolutions_order() {
        let specifiers = [
            "./both",
            "./both.js",
            "./dirfile",
            "./y",
            "./z",
            "./w2.js",
            "./pm",
            "./pn",
            "./pkg",
            "@/both",
            "@/dirfile",
            "./pv",
            "./x/",
        ];
        let source: String = specifiers
            .iter()
            .map(|specifier| format!("import \"{specifier}\";\n"))
            .collect();
        let modes = [
            (
                "n10",
                "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
                None,
            ),
            (
                "bun",
                "\"moduleResolution\": \"bundler\", \"module\": \"esnext\"",
                None,
            ),
            (
                "esm",
                "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
                Some("module"),
            ),
            (
                "cjs",
                "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
                Some("commonjs"),
            ),
        ];
        let files = [
            "both.ts",
            "both.js",
            "dirfile.js",
            "dirfile/index.ts",
            "y.d.ts",
            "y.js",
            "z.tsx",
            "z.ts",
            "w2.ts",
            "w2.js",
            "pm/lib/m.ts",
            "pm/lib/m.js",
            "pm/index.ts",
            "pn/lib/n.js",
            "pn/index.ts",
            "pkg/lib/t.d.ts",
            "pkg/index.ts",
            "pv/index.ts",
            "x.ts",
            "x/index.ts",
        ];
        let mut nodes = Vec::new();
        let mut configs: Vec<(String, String)> = Vec::new();
        for (mode, options, package_type) in modes {
            nodes.push(node_in(
                nodes.len(),
                &format!("{mode}/src/main.ts"),
                &source,
                Language::TypeScript,
            ));
            for file in files {
                let language = match file.rsplit_once('.').map(|(_, extension)| extension) {
                    Some("js") => Language::JavaScript,
                    _ => Language::TypeScript,
                };
                nodes.push(node_in(
                    nodes.len(),
                    &format!("{mode}/src/{file}"),
                    "",
                    language,
                ));
            }
            let package = match package_type {
                Some(kind) => format!(
                    "{{\"type\": \"{kind}\", \"devDependencies\": {{\"typescript\": \"^5.9.0\"}}}}"
                ),
                None => "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}".to_string(),
            };
            configs.push((format!("{mode}/package.json"), package));
            configs.push((
                format!("{mode}/tsconfig.json"),
                format!(
                    "{{\"compilerOptions\": {{{options}, \"allowJs\": true, \
                     \"paths\": {{\"@/*\": [\"./src/*\"]}}}}}}"
                ),
            ));
            for (file, contents) in [
                ("pm/package.json", "{\"main\": \"./lib/m.js\"}"),
                ("pn/package.json", "{\"main\": \"./lib/n.js\"}"),
                (
                    "pkg/package.json",
                    "{\"types\": \"./lib/t.d.ts\", \"main\": \"./lib/m.js\"}",
                ),
                (
                    "pv/package.json",
                    "{\"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                ),
            ] {
                configs.push((format!("{mode}/src/{file}"), contents.to_string()));
            }
        }
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let stride = files.len() + 1;
        let edges = |mode: usize| -> Vec<&str> {
            graph.edges[&(mode * stride)]
                .iter()
                .map(|dst| nodes[*dst].rel_path.split_once("/src/").unwrap().1)
                .collect()
        };
        assert_eq!(
            edges(0),
            vec![
                "both.ts",
                "dirfile/index.ts",
                "y.d.ts",
                "z.ts",
                "w2.ts",
                "pm/lib/m.ts",
                "pn/index.ts",
                "pkg/lib/t.d.ts",
                "x/index.ts"
            ]
        );
        let single = vec![
            "both.ts",
            "dirfile.js",
            "y.d.ts",
            "z.ts",
            "w2.ts",
            "pm/lib/m.ts",
            "pn/lib/n.js",
            "pkg/lib/t.d.ts",
            "x/index.ts",
        ];
        assert_eq!(edges(1), single);
        assert_eq!(edges(3), single);
        assert_eq!(edges(2), vec!["both.ts", "w2.ts"]);
        // `@/both` and `@/dirfile` name no file under `node16` for an ES
        // import, which a declared `paths` pattern discloses.
        assert_eq!(
            graph.undetermined_imports,
            ["@/both", "@/dirfile"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(
            graph.types_versions_undetermined_imports,
            BTreeSet::from(["./pv".to_string()])
        );
        assert!(graph.version_undetermined_imports.is_empty());
    }

    /// `.` and `..` are relative specifiers, as TypeScript's `pathIsRelative`
    /// reads them, their segments folded where a path writes them, and each
    /// module resolution reads them as tsc 5.9.3, 6.0.3 and 7.0.2 trace it
    /// (`rel_*`): `classic` as the file the folded path names (`.` in
    /// `src/app` as `src/app.ts`, `../..` as a file beside the project's
    /// directory) and a path ending in `/` as none; `node10`, `bundler` and
    /// `node16` for a CommonJS import as the directory alone, its `index`;
    /// `node16` for an ES import as nothing, since it reads no directory.
    /// Where no TypeScript reads the import, Node and the bundlers read a
    /// directory too.
    #[test]
    fn dot_segments_are_relative_paths_each_resolution_reads() {
        let specifiers = [
            ".",
            "..",
            "./sub/..",
            "../..",
            "./",
            "../",
            "./sub/../both",
            "./sub/../both.js",
        ];
        let modes = [
            (
                "cls",
                Some("\"moduleResolution\": \"classic\", \"module\": \"esnext\""),
                None,
            ),
            (
                "n10",
                Some("\"moduleResolution\": \"node10\", \"module\": \"commonjs\""),
                None,
            ),
            (
                "bun",
                Some("\"moduleResolution\": \"bundler\", \"module\": \"esnext\""),
                None,
            ),
            (
                "esm",
                Some("\"moduleResolution\": \"node16\", \"module\": \"node16\""),
                Some("module"),
            ),
            (
                "cjs",
                Some("\"moduleResolution\": \"node16\", \"module\": \"node16\""),
                Some("commonjs"),
            ),
            ("js", None, None),
        ];
        let mut nodes = Vec::new();
        let mut configs: Vec<(String, String)> = Vec::new();
        let mut importers = Vec::new();
        for (mode, options, package_type) in modes {
            let (extension, language) = match options {
                Some(_) => ("ts", Language::TypeScript),
                None => ("js", Language::JavaScript),
            };
            let mut ids = Vec::new();
            for (index, specifier) in specifiers.iter().enumerate() {
                ids.push(nodes.len());
                nodes.push(node_in(
                    nodes.len(),
                    &format!("{mode}/src/app/m{index}.{extension}"),
                    &format!("import \"{specifier}\";\n"),
                    language,
                ));
            }
            importers.push(ids);
            for file in [
                "src/app/index",
                "src/app",
                "src/index",
                "src",
                "index",
                "src/app/both",
            ] {
                let path = format!("{mode}/{file}.{extension}");
                nodes.push(node_in(nodes.len(), &path, "", language));
            }
            let typescript = match options {
                Some(_) => ", \"devDependencies\": {\"typescript\": \"^5.9.0\"}",
                None => "",
            };
            let kind = package_type.map_or(String::new(), |kind| format!(", \"type\": \"{kind}\""));
            configs.push((
                format!("{mode}/package.json"),
                format!("{{\"name\": \"{mode}\"{kind}{typescript}}}"),
            ));
            if let Some(options) = options {
                configs.push((
                    format!("{mode}/tsconfig.json"),
                    format!("{{\"compilerOptions\": {{{options}, \"allowJs\": true}}}}"),
                ));
            }
        }
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let read = |mode: usize| -> Vec<String> {
            importers[mode]
                .iter()
                .map(|id| {
                    let found: Vec<&str> = graph
                        .edges
                        .get(id)
                        .into_iter()
                        .flatten()
                        .map(|dst| nodes[*dst].rel_path.split_once('/').unwrap().1)
                        .collect();
                    found.join(" ")
                })
                .collect()
        };
        assert_eq!(
            read(0),
            [
                "src/app.ts",
                "src.ts",
                "src/app.ts",
                "",
                "",
                "",
                "src/app/both.ts",
                "src/app/both.ts"
            ]
        );
        let directories = [
            "src/app/index.ts",
            "src/index.ts",
            "src/app/index.ts",
            "index.ts",
            "src/app/index.ts",
            "src/index.ts",
            "src/app/both.ts",
            "src/app/both.ts",
        ];
        for mode in [1, 2, 4] {
            assert_eq!(read(mode), directories, "{}", modes[mode].0);
        }
        assert_eq!(read(3), ["", "", "", "", "", "", "", "src/app/both.ts"]);
        assert_eq!(read(5), directories.map(|file| file.replace(".ts", ".js")));
        assert!(graph.undetermined_imports.is_empty());
        assert!(graph.version_undetermined_imports.is_empty());
    }

    /// `node16` looks the path of an `import x = require()` up as a CommonJS
    /// import's even in a file emitted as an ES module, as tsc 5.9.3, 6.0.3
    /// and 7.0.2 trace it (`req_esm`): the file the path names with an
    /// extension added, then the directory, where an import declaration in
    /// the same file names none of them.
    #[test]
    fn a_require_in_an_es_module_reads_its_path_as_a_commonjs_import() {
        let nodes = vec![
            node_in(
                0,
                "src/r.ts",
                "import a = require(\"./both\");\nimport b = require(\"./sub\");\n\
                 import c = require(\"./pn\");\n",
                Language::TypeScript,
            ),
            node_in(
                1,
                "src/d.ts",
                "import a from \"./both\";\nimport b from \"./sub\";\nimport c from \"./pn\";\n",
                Language::TypeScript,
            ),
            node_in(2, "src/both.ts", "", Language::TypeScript),
            node_in(3, "src/sub/index.ts", "", Language::TypeScript),
            node_in(4, "src/pn/lib/n.js", "", Language::JavaScript),
            node_in(5, "src/pn/index.ts", "", Language::TypeScript),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"type\": \"module\", \"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"node16\", \"moduleResolution\": \"node16\", \
                     \"allowJs\": true}}",
                ),
                ("src/pn/package.json", "{\"main\": \"./lib/n.js\"}"),
            ],
            &nodes,
        );
        assert_eq!(
            edges_under(&nodes, 0, &manifests),
            ["src/both.ts", "src/sub/index.ts", "src/pn/lib/n.js"]
        );
        assert!(edges_under(&nodes, 1, &manifests).is_empty());
    }

    /// A `paths` substitution writing an extension names the file it spells
    /// before the lookup replaces that extension, under every module
    /// resolution, as tsc 5.9.3, 6.0.3 and 7.0.2 trace it (`px_*`): `@x` and
    /// `@z/x` take `x.js` beside `x.ts`, which `@y`, writing none, takes; a
    /// substitution naming no file takes the lookup's (`@z/both`).
    #[test]
    fn a_paths_substitution_writing_an_extension_names_its_file_first() {
        let modes = [
            "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
            "\"moduleResolution\": \"bundler\", \"module\": \"esnext\"",
            "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
        ];
        for options in modes {
            let nodes = vec![
                node_in(
                    0,
                    "src/main.ts",
                    "import \"@x\";\nimport \"@y\";\nimport \"@z/x\";\nimport \"@z/both\";\n",
                    Language::TypeScript,
                ),
                node_in(1, "src/x.js", "", Language::JavaScript),
                node_in(2, "src/x.ts", "", Language::TypeScript),
                node_in(3, "src/both.ts", "", Language::TypeScript),
            ];
            let config = format!(
                "{{\"compilerOptions\": {{{options}, \"allowJs\": true, \"paths\": \
                 {{\"@x\": [\"./src/x.js\"], \"@y\": [\"./src/x\"], \"@z/*\": [\"./src/*.js\"]}}}}}}"
            );
            let manifests = manifests_of(
                &[
                    (
                        "package.json",
                        "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                    ),
                    ("tsconfig.json", &config),
                ],
                &nodes,
            );
            let graph = build_import_graph(&nodes, &manifests);
            let found: BTreeSet<&str> = graph.edges[&0]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect();
            assert_eq!(
                found,
                BTreeSet::from(["src/x.js", "src/x.ts", "src/both.ts"]),
                "{options}"
            );
            assert!(graph.undetermined_imports.is_empty(), "{options}");
        }
    }

    /// A directory is read by the first of its `package.json`'s `typings`,
    /// `types` and `main` that is set, as tsc 5.9.3, 6.0.3 and 7.0.2 trace
    /// it (`req_fields_*`): a `types` naming no file gives way to the
    /// directory's `index`, never to `main` (`./d1`); an empty `typings`
    /// counts as unset (`./d2`); and a `main` naming a directory names its
    /// `index` (`./d3`). A field naming a JavaScript file is read with the
    /// pass's extensions, so `node10`'s first pass, which holds TypeScript's
    /// alone, takes the directory's `index.ts` over it (`./d4`); one naming
    /// a declaration takes it over its TypeScript twin (`./d5`, `./d6`).
    #[test]
    fn a_directory_is_read_by_the_first_package_json_field_it_sets() {
        let modes = [
            (
                "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
                "src/d4/index.ts",
            ),
            (
                "\"moduleResolution\": \"bundler\", \"module\": \"esnext\"",
                "src/d4/lib/index.js",
            ),
        ];
        for (options, d4) in modes {
            let nodes = vec![
                node_in(
                    0,
                    "src/main.ts",
                    "import \"./d1\";\nimport \"./d2\";\nimport \"./d3\";\n\
                     import \"./d4\";\nimport \"./d5\";\nimport \"./d6\";\n",
                    Language::TypeScript,
                ),
                node_in(1, "src/d1/lib/m.ts", "", Language::TypeScript),
                node_in(2, "src/d1/index.ts", "", Language::TypeScript),
                node_in(3, "src/d2/t.d.ts", "", Language::TypeScript),
                node_in(4, "src/d2/index.ts", "", Language::TypeScript),
                node_in(5, "src/d3/lib/index.ts", "", Language::TypeScript),
                node_in(6, "src/d3/index.ts", "", Language::TypeScript),
                node_in(7, "src/d4/lib/index.js", "", Language::JavaScript),
                node_in(8, "src/d4/index.ts", "", Language::TypeScript),
                node_in(9, "src/d5/index.d.ts", "", Language::TypeScript),
                node_in(10, "src/d5/index.ts", "", Language::TypeScript),
                node_in(11, "src/d6/index.d.ts", "", Language::TypeScript),
                node_in(12, "src/d6/index.ts", "", Language::TypeScript),
            ];
            let config = format!("{{\"compilerOptions\": {{{options}, \"allowJs\": true}}}}");
            let manifests = manifests_of(
                &[
                    (
                        "package.json",
                        "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                    ),
                    ("tsconfig.json", &config),
                    (
                        "src/d1/package.json",
                        "{\"types\": \"./missing.d.ts\", \"main\": \"./lib/m.js\"}",
                    ),
                    (
                        "src/d2/package.json",
                        "{\"typings\": \"\", \"types\": \"./t.d.ts\"}",
                    ),
                    ("src/d3/package.json", "{\"main\": \"./lib\"}"),
                    ("src/d4/package.json", "{\"types\": \"./lib/index.js\"}"),
                    ("src/d5/package.json", "{\"types\": \"./index.d.ts\"}"),
                    ("src/d6/package.json", "{\"main\": \"./index.d.ts\"}"),
                ],
                &nodes,
            );
            assert_eq!(
                edges_under(&nodes, 0, &manifests),
                [
                    "src/d1/index.ts",
                    "src/d2/t.d.ts",
                    "src/d3/lib/index.ts",
                    d4,
                    "src/d5/index.d.ts",
                    "src/d6/index.d.ts"
                ],
                "{options}"
            );
        }
    }

    /// `node10` looks every path `paths` and `baseUrl` name up with
    /// TypeScript's extensions before any with JavaScript's, as tsc 5.9.3 and
    /// 6.0.3 trace it (`tq_node10`): `@/x` takes the second substitution's
    /// `b/x.ts` over the first's `a/x.js`, which `bundler` and `node16` for a
    /// CommonJS import take in their one pass, and `node16` for an ES import
    /// only where the specifier writes an extension.
    #[test]
    fn paths_substitutions_are_looked_up_in_each_pass_before_the_next() {
        let modes = [
            (
                "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
                None,
                ["b/x.ts", "b/x.ts"],
            ),
            (
                "\"moduleResolution\": \"bundler\", \"module\": \"esnext\"",
                None,
                ["a/x.js", "a/x.js"],
            ),
            (
                "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
                None,
                ["a/x.js", "a/x.js"],
            ),
            (
                "\"moduleResolution\": \"node16\", \"module\": \"node16\"",
                Some("module"),
                ["", "a/x.js"],
            ),
        ];
        for (options, kind, expected) in modes {
            let nodes = vec![
                node_in(0, "src/m0.ts", "import \"@/x\";\n", Language::TypeScript),
                node_in(1, "src/m1.ts", "import \"@/x.js\";\n", Language::TypeScript),
                node_in(2, "a/x.js", "", Language::JavaScript),
                node_in(3, "b/x.ts", "", Language::TypeScript),
            ];
            let package = match kind {
                Some(kind) => format!(
                    "{{\"type\": \"{kind}\", \"devDependencies\": {{\"typescript\": \"^5.9.0\"}}}}"
                ),
                None => "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}".to_string(),
            };
            let config = format!(
                "{{\"compilerOptions\": {{{options}, \"allowJs\": true, \
                 \"paths\": {{\"@/*\": [\"./a/*\", \"./b/*\"]}}}}}}"
            );
            let manifests = manifests_of(
                &[("package.json", &package), ("tsconfig.json", &config)],
                &nodes,
            );
            for (id, file) in expected.iter().enumerate() {
                let found = edges_under(&nodes, id, &manifests);
                let want: Vec<String> = (!file.is_empty())
                    .then(|| file.to_string())
                    .into_iter()
                    .collect();
                assert_eq!(found, want, "{options} {kind:?} {id}");
            }
        }
    }

    /// `node10` looks a non-relative specifier up in `node_modules` with
    /// TypeScript's extensions between its passes, as tsc 5.9.3 and 6.0.3
    /// trace it, so a declaration or TypeScript file there comes before the
    /// JavaScript file `baseUrl` names: none for `config` and `lib/x`
    /// (`p3_*`, `p3c_*`), the workspace package's for `w` (`q2a_*`), and a
    /// `typesVersions` pack does not follow leaves `tv/x` open (`q2b_*`). A
    /// package of JavaScript alone leaves `plain` the project's file
    /// (`q2c_*`). `bundler` reads `baseUrl` first in its one pass
    /// (`p3d_*`).
    #[test]
    fn node10_looks_in_node_modules_before_a_base_url_javascript_file() {
        let modes = [
            (
                "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
                true,
            ),
            (
                "\"moduleResolution\": \"bundler\", \"module\": \"esnext\"",
                false,
            ),
        ];
        for (options, node10) in modes {
            let specifiers = ["config", "lib/x", "w", "tv/x", "plain"];
            let source: String = specifiers
                .iter()
                .map(|specifier| format!("import \"{specifier}\";\n"))
                .collect();
            let nodes = vec![
                node_in(0, "src/app/main.ts", &source, Language::TypeScript),
                node_in(1, "src/config.js", "", Language::JavaScript),
                node_in(2, "src/lib/x.js", "", Language::JavaScript),
                node_in(3, "src/w.js", "", Language::JavaScript),
                node_in(4, "packages/w/src/index.ts", "", Language::TypeScript),
                node_in(5, "src/tv/x.js", "", Language::JavaScript),
                node_in(6, "src/plain.js", "", Language::JavaScript),
            ];
            let config = format!(
                "{{\"compilerOptions\": {{{options}, \"allowJs\": true, \"baseUrl\": \"./src\"}}}}"
            );
            let manifests = manifests_of(
                &[
                    (
                        "package.json",
                        "{\"workspaces\": [\"packages/*\"], \
                         \"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                    ),
                    ("tsconfig.json", &config),
                    ("node_modules/@types/config/index.d.ts", ""),
                    ("node_modules/lib/package.json", "{\"name\": \"lib\"}"),
                    ("node_modules/lib/x.d.ts", ""),
                    (
                        "packages/w/package.json",
                        "{\"name\": \"w\", \"main\": \"./src/index.ts\"}",
                    ),
                    (
                        "node_modules/w/package.json",
                        "{\"name\": \"w\", \"main\": \"./src/index.ts\"}",
                    ),
                    ("node_modules/w/src/index.ts", ""),
                    (
                        "node_modules/tv/package.json",
                        "{\"name\": \"tv\", \"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                    ),
                    ("node_modules/tv/ts/x.d.ts", ""),
                    (
                        "node_modules/plain/package.json",
                        "{\"name\": \"plain\", \"main\": \"index.js\"}",
                    ),
                    ("node_modules/plain/index.js", ""),
                ],
                &nodes,
            );
            let graph = build_import_graph(&nodes, &manifests);
            let edges: Vec<&str> = graph.edges[&0]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect();
            let disclosed = &graph.types_versions_undetermined_imports;
            if node10 {
                assert_eq!(edges, vec!["packages/w/src/index.ts", "src/plain.js"]);
                assert_eq!(*disclosed, BTreeSet::from(["tv/x".to_string()]));
            } else {
                let all = [
                    "src/config.js",
                    "src/lib/x.js",
                    "src/w.js",
                    "src/tv/x.js",
                    "src/plain.js",
                ];
                assert_eq!(edges, all);
                assert!(disclosed.is_empty());
            }
        }
    }

    /// Where a configuration that cannot be read leaves the module
    /// resolution open, or under `node16` the format the importer is
    /// emitted in, a relative path and a `paths` substitution name the one
    /// script module they spell: `pn/index.ts` for `./pn`, which `bundler`
    /// and `node16` for a CommonJS import read as the `package.json`'s
    /// `main`, `pn/lib/n.js`.
    #[test]
    fn an_open_module_resolution_keeps_the_script_module_a_path_spells() {
        let configs = [
            "{\"extends\": \"./missing.json\", \"compilerOptions\": \
             {\"allowJs\": true, \"paths\": {\"@/*\": [\"./src/*\"]}}}",
            "{\"extends\": \"./missing.json\", \"compilerOptions\": \
             {\"moduleResolution\": \"node16\", \"allowJs\": true, \"paths\": {\"@/*\": [\"./src/*\"]}}}",
        ];
        for config in configs {
            let nodes = vec![
                node_in(
                    0,
                    "src/main.ts",
                    "import \"./pn\";\nimport \"@/pn\";\n",
                    Language::TypeScript,
                ),
                node_in(1, "src/pn/lib/n.js", "", Language::JavaScript),
                node_in(2, "src/pn/index.ts", "", Language::TypeScript),
            ];
            let manifests = manifests_of(
                &[
                    (
                        "package.json",
                        "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                    ),
                    ("tsconfig.json", config),
                    ("src/pn/package.json", "{\"main\": \"./lib/n.js\"}"),
                ],
                &nodes,
            );
            assert_eq!(
                edges_under(&nodes, 0, &manifests),
                ["src/pn/index.ts"],
                "{config}"
            );
        }
    }

    /// A relative specifier is read under each module resolution the
    /// TypeScript majors that may compile its importer read it under: with
    /// `module` `esnext` and nothing installed or declared, 5 reads
    /// `classic`, which tries no directory's `index`, and 6 and 7 `bundler`,
    /// which do, so `../components` is disclosed as one the TypeScript
    /// version decides, while `./util`, a file each reads alike, keeps its
    /// edge, as does `./both`, whose `.ts` beside a `.js` each takes
    /// (`probe_mixed`).
    #[test]
    fn a_relative_specifier_the_majors_read_apart_is_disclosed() {
        let nodes = vec![
            node_in(
                0,
                "app/src/app/main.ts",
                "import \"../components\";\nimport \"./util\";\nimport \"./both\";\n",
                Language::TypeScript,
            ),
            node_in(1, "app/src/components/index.ts", "", Language::TypeScript),
            node_in(2, "app/src/app/util.ts", "", Language::TypeScript),
            node_in(3, "app/src/app/both.ts", "", Language::TypeScript),
            node_in(4, "app/src/app/both.js", "", Language::JavaScript),
        ];
        let manifests = manifests_of(
            &[(
                "app/tsconfig.json",
                "{\"compilerOptions\": {\"module\": \"esnext\"}}",
            )],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(edges, vec!["app/src/app/util.ts", "app/src/app/both.ts"]);
        assert_eq!(
            graph.version_undetermined_imports,
            BTreeSet::from(["../components".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
        assert!(graph.typescript_not_installed && !graph.typescript_installed_unread);
    }

    /// `classic` resolution walks on past the project's root with `.ts`,
    /// `.tsx` and `.d.ts` before it looks in `@types` or for JavaScript, and
    /// looks in every `node_modules/@types` up to the filesystem's root, as
    /// tsc 5.9.3 and 6.0.3 trace it (`classic3`), the project at
    /// `outer/repo`: `lib/x` resolves to the `lib/x.ts` two directories
    /// above the root and `lib/y` to the `outer/lib/y.d.ts` one above it,
    /// not to the `.js` files in it, and `jz` to the `@types/jz` two above
    /// it. So `lib/x` and `lib/y` form no edge and `jz` is disclosed. A `.js`
    /// file above the root comes after the project's: `lib/w` keeps its
    /// edge. The walk reaches past the root only after the project's
    /// directories: a `.ts` file in both, as `lib/v` names, is the
    /// project's, as tsc 5.9.3 and 6.0.3 resolve it (`tsboth`).
    #[test]
    fn classic_resolution_walks_past_the_projects_root_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in [
            ("lib/x.ts", "export const x = 1;"),
            ("outer/lib/y.d.ts", "export declare const y: number;"),
            ("outer/lib/v.ts", "export const v = 1;"),
            (
                "node_modules/@types/jz/index.d.ts",
                "export declare const jz: string;",
            ),
            ("lib/w.js", "export const w = 1;"),
            (
                "outer/repo/package.json",
                "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
            ),
            (
                "outer/repo/tsconfig.json",
                "{\"compilerOptions\": {\"moduleResolution\": \"classic\", \"module\": \"esnext\"}}",
            ),
        ] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        let js = |id, rel: &str| node_in(id, rel, "", Language::JavaScript);
        let imports = "import x from \"lib/x\";\nimport y from \"lib/y\";\n\
                       import jz from \"jz\";\nimport w from \"lib/w\";\n\
                       import v from \"lib/v\";\n";
        let nodes = vec![
            node_in(0, "src/app/main.ts", imports, Language::TypeScript),
            js(1, "src/lib/x.js"),
            js(2, "src/lib/y.js"),
            js(3, "src/jz.js"),
            js(4, "src/lib/w.js"),
            node_in(5, "src/lib/v.ts", "", Language::TypeScript),
        ];
        let manifests = Manifests::read(&dir.path().join("outer/repo"), &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(edges, vec!["src/lib/w.js", "src/lib/v.ts"]);
        assert_eq!(
            graph.declaration_undetermined_imports,
            BTreeSet::from(["jz".to_string()])
        );
    }

    /// A JavaScript project's `jsconfig.json`, as Create React App documents
    /// absolute imports (`baseUrl: "src"`) and Next.js its aliases, is read
    /// with its `baseUrl` (`cra_js`, `next_js`), as esbuild 0.28.2 and
    /// Next.js 16.3.3 read it, also where TypeScript 7 is declared
    /// (`cra_js_ts7`) and for a TypeScript file there, which esbuild 0.28.2
    /// resolves through it with TypeScript 7.0.2 installed, and an import
    /// the TypeScript majors read apart there is disclosed as one they read
    /// its configuration apart, without advice to install TypeScript.
    #[test]
    fn a_javascript_projects_jsconfig_is_read_with_its_base_url() {
        let js = |id, rel: &str, source: &str| node_in(id, rel, source, Language::JavaScript);
        let mut nodes = vec![
            js(
                0,
                "cra/src/pages/a.js",
                "import Button from \"components/Button\";\nimport p from \"p6\";\n",
            ),
            js(1, "cra/src/components/Button.js", ""),
            js(
                2,
                "next/pages/a.js",
                "import Nav from \"@/components/Nav\";\nimport Nav2 from \"components/Nav\";\n",
            ),
            js(3, "next/components/Nav.js", ""),
            js(
                4,
                "cra7/src/pages/a.js",
                "import Button from \"components/Button\";\n",
            ),
            js(5, "cra7/src/components/Button.js", ""),
            node_in(
                6,
                "cra7/src/pages/b.ts",
                "import Button from \"components/Button\";\n",
                Language::TypeScript,
            ),
        ];
        for file in ["types", "esm", "cjs"] {
            nodes.push(node_in(
                nodes.len(),
                &format!("packages/p6/src/{file}.ts"),
                "",
                Language::TypeScript,
            ));
        }
        let manifests = manifests_of(
            &[
                ("package.json", "{\"workspaces\": [\"packages/*\"]}"),
                (
                    "cra/package.json",
                    "{\"dependencies\": {\"react-scripts\": \"5.0.1\"}}",
                ),
                (
                    "cra/jsconfig.json",
                    "{\"compilerOptions\": {\"baseUrl\": \"src\"}, \"include\": [\"src\"]}",
                ),
                (
                    "cra7/package.json",
                    "{\"dependencies\": {\"react-scripts\": \"5.0.1\"}, \
                     \"devDependencies\": {\"typescript\": \"^7.0.2\"}}",
                ),
                (
                    "cra7/jsconfig.json",
                    "{\"compilerOptions\": {\"baseUrl\": \"src\"}, \"include\": [\"src\"]}",
                ),
                (
                    "next/jsconfig.json",
                    "{\"compilerOptions\": {\"baseUrl\": \".\", \"paths\": \
                     {\"@/components/*\": [\"components/*\"]}}}",
                ),
                (
                    "packages/p6/package.json",
                    "{\"name\": \"p6\", \"types\": \"./src/types.ts\", \"exports\": \
                     {\".\": {\"import\": \"./src/esm.ts\", \"require\": \"./src/cjs.ts\"}}}",
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
        assert_eq!(edges(0), vec!["cra/src/components/Button.js"]);
        assert_eq!(edges(2), vec!["next/components/Nav.js"]);
        assert_eq!(edges(4), vec!["cra7/src/components/Button.js"]);
        assert_eq!(edges(6), vec!["cra7/src/components/Button.js"]);
        assert!(graph.version_undetermined_imports.is_empty());
        assert_eq!(
            graph.jsconfig_undetermined_imports,
            BTreeSet::from(["p6".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
    }

    /// A relative specifier the TypeScript majors read apart in a
    /// `jsconfig.json`'s JavaScript project is disclosed as one they read its
    /// configuration apart, not as a `paths` alias naming no file, as tsc
    /// 5.9.3, 6.0.3 and 7.0.2 trace it (`js_apart`): `module` `esnext` reads
    /// `classic` under 5, which finds no file for `../components`, and
    /// `bundler` under 6 and 7, which take `components/index.js`.
    #[test]
    fn a_relative_specifier_a_jsconfigs_majors_read_apart_names_its_cause() {
        let nodes = vec![
            node_in(
                0,
                "src/app/main.js",
                "import x from \"../components\";\n",
                Language::JavaScript,
            ),
            node_in(1, "src/components/index.js", "", Language::JavaScript),
        ];
        let manifests = manifests_of(
            &[
                ("package.json", "{\"name\": \"js-apart\"}"),
                (
                    "jsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"esnext\"}}",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        assert!(graph.edges[&0].is_empty());
        assert_eq!(
            graph.jsconfig_undetermined_imports,
            BTreeSet::from(["../components".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
        assert!(graph.version_undetermined_imports.is_empty());
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
            // Compiled by no project, and naming only what no project maps.
            ts(
                11,
                "eslint.config.js",
                "import js from \"@eslint/js\";\nimport c from \"lodash\";\n",
            ),
            // Compiled by no project, naming what one's `baseUrl` maps.
            ts(12, "scripts/fx.ts", "import user from \"fixtures/user\";\n"),
            // Compiled by a project only a referenced project's own
            // `references` name, which inherits its `include`.
            ts(13, "e2e/spec.ts", "import h from \"#e2e/helper\";\n"),
            ts(14, "e2e/support/helper.ts", ""),
            // Compiled by no project, naming a file only the catch-all `*`
            // of one maps.
            ts(15, "scripts/t.ts", "import s from \"shared\";\n"),
            ts(16, "types/shared.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"baseUrl\": \".\", \"paths\": {\"@/*\": [\"./src/*\"]}}, \
                     \"files\": [], \"references\": [{\"path\": \"./tsconfig.app.json\"}, \
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
                    "{\"compilerOptions\": {\"baseUrl\": \".\", \"paths\": {\"@/*\": [\"./src/*\"], \
                     \"#fixtures/*\": [\"./fixtures/*\"], \"*\": [\"./types/*\"]}}, \
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
                     \"files\": [\"../vite.config.ts\"], \
                     \"references\": [{\"path\": \"../tsconfig.e2e.json\"}]}",
                ),
                (
                    "tsconfig.e2e.json",
                    "{\"extends\": \"./config/e2e.base.json\"}",
                ),
                (
                    "config/e2e.base.json",
                    "{\"compilerOptions\": {\"paths\": {\"#e2e/*\": [\"../e2e/support/*\"]}}, \
                     \"include\": [\"../e2e\"]}",
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
        assert!(edges(8).is_empty() && edges(11).is_empty());
        assert_eq!(edges(13), vec!["e2e/support/helper.ts"]);
        // A file no project compiles is read without a configuration — the
        // root's own `paths` and `baseUrl`, which shadcn's Vite setup puts
        // there, map nothing for it — and disclosed only where one of those
        // the references reach maps what it imports; projects compiling a
        // file alike but saying different things about specifiers leave it
        // undetermined.
        assert!(edges(6).is_empty() && edges(12).is_empty() && edges(15).is_empty());
        assert_eq!(
            graph.ts_owner_undetermined,
            BTreeSet::from([
                "scripts/fx.ts".to_string(),
                "scripts/gen.ts".to_string(),
                "scripts/t.ts".to_string(),
                "src/test-utils/render.tsx".to_string()
            ])
        );
    }

    /// A file a project takes that a project its references reach also takes
    /// is compiled in that referenced project alone, whose `paths` its
    /// imports resolve through: the referencing project's program reads the
    /// file as that project's output declaration, as tsc 5.9.3, 6.0.3 and
    /// 7.0.2 all build it. Projects neither of which reaches the other each
    /// compile it.
    #[test]
    fn a_file_a_project_and_one_it_references_both_take_is_compiled_in_the_referenced_one() {
        let shapes = ["chain", "own", "deep", "siblings"];
        let mut nodes = Vec::new();
        for shape in shapes {
            for (rel, source) in [
                ("shared/s.ts", "import { x } from \"@x\";\n"),
                ("a/ax.ts", ""),
                ("b/bx.ts", ""),
            ] {
                let rel = format!("{shape}/{rel}");
                nodes.push(node_in(
                    nodes.len(),
                    rel.as_str(),
                    source,
                    Language::TypeScript,
                ));
            }
        }
        let a = |include: &str, references: &str| {
            format!(
                "{{\"compilerOptions\": {{\"paths\": {{\"@x\": [\"./ax.ts\"]}}}}, \
                 \"include\": [{include}]{references}}}"
            )
        };
        let b = "{\"compilerOptions\": {\"paths\": {\"@x\": [\"./bx.ts\"]}}, \
                 \"include\": [\"../shared\", \".\"]}";
        let root = |include: &str, reference: &str| {
            format!(
                "{{\"compilerOptions\": {{\"paths\": {{\"@x\": [\"./a/ax.ts\"]}}}}, \
                 \"include\": [{include}], \"references\": [{{\"path\": \"{reference}\"}}]}}"
            )
        };
        let to_b = ", \"references\": [{\"path\": \"../b\"}]";
        let configs = [
            (
                "chain/tsconfig.json",
                "{\"files\": [], \"references\": [{\"path\": \"./a\"}]}".to_string(),
            ),
            ("chain/a/tsconfig.json", a("\"../shared\", \".\"", to_b)),
            ("chain/b/tsconfig.json", b.to_string()),
            ("own/tsconfig.json", root("\"shared\", \"a\"", "./b")),
            ("own/b/tsconfig.json", b.to_string()),
            ("deep/tsconfig.json", root("\"shared\"", "./a")),
            ("deep/a/tsconfig.json", a("\".\"", to_b)),
            ("deep/b/tsconfig.json", b.to_string()),
            (
                "siblings/tsconfig.json",
                "{\"files\": [], \"references\": [{\"path\": \"./a\"}, {\"path\": \"./b\"}]}"
                    .to_string(),
            ),
            ("siblings/a/tsconfig.json", a("\"../shared\", \".\"", "")),
            ("siblings/b/tsconfig.json", b.to_string()),
        ];
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (*path, text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(edges(0), vec!["chain/b/bx.ts"]);
        assert_eq!(edges(3), vec!["own/b/bx.ts"]);
        assert_eq!(edges(6), vec!["deep/b/bx.ts"]);
        assert!(edges(9).is_empty());
        assert_eq!(
            graph.ts_owner_undetermined,
            BTreeSet::from(["siblings/shared/s.ts".to_string()])
        );
    }

    /// A file several projects compile, saying different things about
    /// specifiers, matches a custom condition all of their configurations
    /// list, and none that none lists; one only some list leaves the target
    /// open (`undet_custom`).
    #[test]
    fn a_file_several_projects_compile_matches_the_custom_conditions_they_agree_on() {
        let shapes = [
            ("both", "[\"source\"]", "[\"source\"]"),
            ("some", "[\"source\"]", "[]"),
            ("none", "[]", "[]"),
        ];
        let mut nodes = Vec::new();
        let mut configs = vec![(
            "package.json".to_string(),
            "{\"workspaces\": [\"packages/*\"]}".to_string(),
        )];
        for (shape, a, b) in shapes {
            nodes.push(node_in(
                nodes.len(),
                &format!("{shape}/shared/s.ts"),
                "import p from 'p'\n",
                Language::TypeScript,
            ));
            configs.push((
                format!("{shape}/tsconfig.json"),
                "{\"files\": [], \"references\": [{\"path\": \"./a\"}, {\"path\": \"./b\"}]}"
                    .to_string(),
            ));
            for (project, custom) in [("a", a), ("b", b)] {
                configs.push((
                    format!("{shape}/{project}/tsconfig.json"),
                    format!(
                        "{{\"compilerOptions\": {{\"module\": \"esnext\", \
                         \"moduleResolution\": \"bundler\", \"customConditions\": {custom}, \
                         \"paths\": {{\"@x\": [\"./{project}x.ts\"]}}}}, \
                         \"include\": [\"../shared\", \".\"]}}"
                    ),
                ));
            }
        }
        for file in ["index", "other"] {
            nodes.push(node_in(
                nodes.len(),
                &format!("packages/p/src/{file}.ts"),
                "",
                Language::TypeScript,
            ));
        }
        configs.push((
            "packages/p/package.json".to_string(),
            "{\"name\": \"p\", \"exports\": {\".\": {\"source\": \"./src/index.ts\", \
             \"default\": \"./src/other.ts\"}}}"
                .to_string(),
        ));
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        assert_eq!(edges(0), vec!["packages/p/src/index.ts"]);
        assert!(edges(1).is_empty());
        assert_eq!(edges(2), vec!["packages/p/src/other.ts"]);
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["p".to_string()])
        );
        assert_eq!(graph.ts_owner_undetermined.len(), 3);
    }

    /// A specifier that each TypeScript major reads alike but the majors
    /// read differently is disclosed as one the unknown version decides;
    /// one that projects compiling the file read differently under one
    /// major is disclosed as one they read apart whatever the version. Where no
    /// TypeScript is installed or declared, a configuration setting a module
    /// option tsc 7.0.2 removed (TS5108) is read under 5 and 6 alone: with
    /// `moduleResolution: "node"` or `"node10"` tsc 5.9.3 and 6.0.3 take the
    /// package's `types` (`node10_absent`, `node10_spelled`), and with
    /// `moduleResolution: "classic"` or
    /// `module` `amd`, `umd`, `system` or `none` (TS6046) they resolve no
    /// file for it, where 7.0.2 takes its `import` file in each. A declared
    /// range admitting only 7 and above keeps 7, and what it reads apart from
    /// 5 and 6 is disclosed, as no major the range admits builds such a
    /// configuration (`ge7_node`).
    #[test]
    fn a_specifier_only_the_typescript_version_decides_is_disclosed_as_such() {
        let ts = |id, rel: &str, source: &str| node_in(id, rel, source, Language::TypeScript);
        let mut nodes = vec![
            ts(0, "ver/src/a.ts", "import p from 'p6'\n"),
            ts(1, "owners/shared/s.ts", "import q from 'q6'\n"),
            ts(2, "n10/src/a.ts", "import p from 'p6'\n"),
        ];
        // Each option tsc 7.0.2 rejects, and the file tsc 5.9.3 and 6.0.3
        // resolve `r6` to under it.
        let removed = [
            (
                "\"moduleResolution\": \"node10\", \"module\": \"commonjs\"",
                Some("packages/r6/src/types.ts"),
            ),
            (
                "\"moduleResolution\": \"classic\", \"module\": \"esnext\"",
                None,
            ),
            ("\"module\": \"amd\"", None),
            ("\"module\": \"umd\"", None),
            ("\"module\": \"system\"", None),
            ("\"module\": \"none\"", None),
        ];
        for (at, _) in removed.iter().enumerate() {
            nodes.push(ts(
                nodes.len(),
                &format!("rm{at}/src/a.ts"),
                "import r from 'r6'\n",
            ));
        }
        let ge7 = nodes.len();
        nodes.push(ts(ge7, "ge7/src/a.ts", "import s from 's6'\n"));
        let mut configs = vec![
            (
                "package.json".to_string(),
                "{\"workspaces\": [\"packages/*\"]}".to_string(),
            ),
            ("ver/tsconfig.json".to_string(), "{}".to_string()),
            (
                "ge7/package.json".to_string(),
                "{\"devDependencies\": {\"typescript\": \">=7\"}}".to_string(),
            ),
            (
                "ge7/tsconfig.json".to_string(),
                "{\"compilerOptions\": {\"moduleResolution\": \"node\"}}".to_string(),
            ),
            (
                "n10/tsconfig.json".to_string(),
                "{\"compilerOptions\": {\"moduleResolution\": \"node\", \"module\": \"esnext\"}}"
                    .to_string(),
            ),
            (
                "owners/package.json".to_string(),
                "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}".to_string(),
            ),
            (
                "owners/tsconfig.json".to_string(),
                "{\"files\": [], \"references\": [{\"path\": \"./a\"}, {\"path\": \"./b\"}]}"
                    .to_string(),
            ),
        ];
        for (at, (options, _)) in removed.iter().enumerate() {
            configs.push((
                format!("rm{at}/tsconfig.json"),
                format!("{{\"compilerOptions\": {{{options}}}}}"),
            ));
        }
        for (project, resolution) in [("a", "node10"), ("b", "bundler")] {
            configs.push((
                format!("owners/{project}/tsconfig.json"),
                format!(
                    "{{\"compilerOptions\": {{\"module\": \"esnext\", \
                     \"moduleResolution\": \"{resolution}\"}}, \"include\": [\"../shared\"]}}"
                ),
            ));
        }
        for package in ["p6", "q6", "r6", "s6"] {
            for file in ["types", "esm", "cjs"] {
                nodes.push(ts(
                    nodes.len(),
                    &format!("packages/{package}/src/{file}.ts"),
                    "",
                ));
            }
            configs.push((
                format!("packages/{package}/package.json"),
                format!(
                    "{{\"name\": \"{package}\", \"types\": \"./src/types.ts\", \"exports\": \
                     {{\".\": {{\"import\": \"./src/esm.ts\", \"require\": \"./src/cjs.ts\"}}}}}}"
                ),
            ));
        }
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        assert!(graph.edges[&0].is_empty() && graph.edges[&1].is_empty());
        let n10: Vec<&str> = graph.edges[&2]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(n10, vec!["packages/p6/src/types.ts"]);
        for (at, (options, expected)) in removed.iter().enumerate() {
            let found: Vec<&str> = graph.edges[&(3 + at)]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect();
            assert_eq!(
                found,
                expected.iter().copied().collect::<Vec<_>>(),
                "{options}"
            );
        }
        assert!(graph.edges[&ge7].is_empty());
        assert_eq!(
            graph.version_undetermined_imports,
            BTreeSet::from(["p6".to_string(), "s6".to_string()])
        );
        assert_eq!(graph.typescript_ranges, BTreeSet::from([">=7".to_string()]));
        assert_eq!(
            graph.projects_undetermined_imports,
            BTreeSet::from(["q6".to_string()])
        );
        assert!(graph.undetermined_imports.is_empty());
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
    /// `exports` it exports only what that maps, a subpath through its exact
    /// key or else the pattern key with the longest prefix before `*`, the
    /// longer key on a tie (`./*.js` over `./*` written after it, as tsc
    /// 5.9.3, 6.0.3 and 7.0.2 and Node 26.12.1 take it);
    /// without, the name stands for the file `types` or `main` names, else
    /// its `index`, and a subpath for the file below it. A listed name whose
    /// file pack cannot determine is disclosed; a package no workspace lists
    /// is no workspace's.
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
                 import icon from \"@acme/ui/icon.svg\";\nimport en from \"@acme/msg/messages/en\";\n\
                 import card from \"@acme/msg/ui/card\";\nimport sx from \"@acme/msg/ui/special/x\";\n\
                 import util from \"@acme/msg/util.js\";\n",
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
            ts(17, "packages/msg/src/ui/card.ts", ""),
            ts(18, "packages/msg/src/special/x.ts", ""),
            ts(19, "packages/msg/src/ui/special/x.ts", ""),
            ts(20, "packages/msg/lib/util.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"name\": \"root\", \"workspaces\": [\"packages/*\", \"apps/*\"], \
                     \"devDependencies\": {\"typescript\": \"^6.0.3\"}}",
                ),
                (
                    "packages/ui/package.json",
                    "{\"name\": \"@acme/ui\", \"exports\": {\".\": \"./src/index.ts\", \
                     \"./button\": \"./src/button.ts\", \"./styles.css\": \"./src/styles.css\", \
                     \"./icon.svg\": \"./assets/icon.svg\"}}",
                ),
                ("packages/ui/assets/icon.svg", "<svg/>"),
                (
                    "packages/msg/package.json",
                    "{\"name\": \"@acme/msg\", \"exports\": {\"./messages/*\": \"./messages/*.json\", \
                     \"./ui/*\": \"./src/ui/*.ts\", \"./ui/special/*\": \"./src/special/*.ts\", \
                     \"./*.js\": \"./lib/*.ts\", \"./*\": \"./src/*.ts\"}}",
                ),
                ("packages/msg/messages/en.json", "{}"),
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
                (
                    "pn/package.json",
                    "{\"devDependencies\": {\"typescript\": \"^6.0.3\"}}",
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
                "packages/ui/src/styles.css",
                "packages/msg/src/ui/card.ts",
                "packages/msg/src/special/x.ts",
                "packages/msg/lib/util.ts"
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

    /// TypeScript reads a workspace package without `exports` as its
    /// `node_modules` lookup reads the package's directory, as tsc 5.9.3,
    /// 6.0.3 and 7.0.2 trace it: with TypeScript's extensions by the first of
    /// `typings` and `types` it sets, else `main`, then its `index`, and
    /// only then with JavaScript's by `main`. A `types` naming no file falls
    /// to the `index`, never to `main` (`p4_*`), and a `typesVersions` leaves
    /// the name open.
    #[test]
    fn a_workspace_package_without_exports_is_read_by_the_first_field_it_sets() {
        let ts = |id, rel| node_in(id, rel, "", Language::TypeScript);
        let nodes = vec![
            node_in(
                0,
                "src/app.ts",
                "import a from \"typed\";\nimport b from \"unbuilt\";\n\
                 import c from \"script\";\nimport d from \"built\";\nimport e from \"versioned\";\n",
                Language::TypeScript,
            ),
            ts(1, "packages/typed/a.d.ts"),
            ts(2, "packages/typed/b.d.ts"),
            ts(3, "packages/unbuilt/src/main.ts"),
            node_in(4, "packages/script/lib/index.js", "", Language::JavaScript),
            node_in(5, "packages/built/dist/index.js", "", Language::JavaScript),
            ts(6, "packages/built/dist/index.d.ts"),
            ts(7, "packages/built/src/index.ts"),
            ts(8, "packages/versioned/index.d.ts"),
            ts(9, "packages/versioned/ts/index.d.ts"),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"workspaces\": [\"packages/*\"], \"devDependencies\": {\"typescript\": \"5.9.3\"}}",
                ),
                (
                    "tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"esnext\", \"moduleResolution\": \"bundler\"}}",
                ),
                (
                    "packages/typed/package.json",
                    "{\"name\": \"typed\", \"types\": \"./b.d.ts\", \"typings\": \"./a.d.ts\"}",
                ),
                (
                    "packages/unbuilt/package.json",
                    "{\"name\": \"unbuilt\", \"types\": \"./dist/index.d.ts\", \"main\": \"./src/main.ts\"}",
                ),
                (
                    "packages/script/package.json",
                    "{\"name\": \"script\", \"types\": \"./dist/index.d.ts\", \"main\": \"./lib/index.js\"}",
                ),
                (
                    "packages/built/package.json",
                    "{\"name\": \"built\", \"main\": \"./dist/index.js\"}",
                ),
                (
                    "packages/versioned/package.json",
                    "{\"name\": \"versioned\", \"types\": \"./index.d.ts\", \
                     \"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                ),
            ],
            &nodes,
        );
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<&str> = graph.edges[&0]
            .iter()
            .map(|dst| nodes[*dst].rel_path.as_str())
            .collect();
        assert_eq!(
            edges,
            vec![
                "packages/typed/a.d.ts",
                "packages/script/lib/index.js",
                "packages/built/dist/index.d.ts"
            ]
        );
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["unbuilt".to_string()])
        );
        assert_eq!(
            graph.types_versions_undetermined_imports,
            BTreeSet::from(["versioned".to_string()])
        );
    }

    /// TypeScript reads a subpath of a workspace package without `exports`
    /// as its `node_modules` lookup reads one below the package's link, as
    /// tsc 5.9.3, 6.0.3 and 7.0.2 trace it: the file it names and then the
    /// directory, by the `package.json` it holds, with TypeScript's
    /// extensions, and only then with JavaScript's. So a declaration or a
    /// TypeScript file beside a JavaScript one comes first, and so does a
    /// directory's `index.ts` beside a JavaScript file of its name. An ES
    /// import under `node16` adds no extension and reads no `index`: a
    /// subpath writing none names no file, and is disclosed. A
    /// `typesVersions` the package holds leaves a subpath open. A file of
    /// any kind a subpath spells exactly is the one where neither pass finds
    /// a file: a stylesheet pack does not read forms no edge and is not
    /// disclosed.
    #[test]
    fn a_workspace_package_subpath_is_read_as_typescript_reads_one_in_node_modules() {
        let ts = |id, rel| node_in(id, rel, "", Language::TypeScript);
        let js = |id, rel| node_in(id, rel, "", Language::JavaScript);
        let bundler = "\"module\": \"esnext\", \"moduleResolution\": \"bundler\"";
        let node10 = "\"module\": \"commonjs\", \"moduleResolution\": \"node10\"";
        let node16 = "\"module\": \"node16\", \"moduleResolution\": \"node16\"";
        let specifiers = [
            "w/src/a", "w/src/b", "w/src/c", "w/lib/x", "w/src/j", "w/lib/y",
        ];
        let read = [
            "packages/w/src/a.d.ts",
            "packages/w/src/b.d.ts",
            "packages/w/src/c.ts",
            "packages/w/lib/x/index.ts",
            "packages/w/src/j.js",
            "packages/w/lib/y/main.d.ts",
        ];
        let modes = [
            (bundler, "src/app.ts", &specifiers[..], &read[..]),
            (node10, "src/app.ts", &specifiers, &read),
            (node16, "src/app.cts", &specifiers, &read),
            (
                node16,
                "src/app.mts",
                &[
                    "w/src/a.js",
                    "w/src/c.js",
                    "w/lib/x/index.js",
                    "w/lib/y",
                    "w/src/b",
                ],
                &[read[0], read[2], read[3], read[5]],
            ),
        ];
        for (options, importer, specifiers, edges) in modes {
            let source: String = specifiers
                .iter()
                .chain(&["v/src/a", "w/theme.css"])
                .map(|specifier| format!("import \"{specifier}\";\n"))
                .collect();
            let nodes = vec![
                node_in(0, importer, &source, Language::TypeScript),
                js(1, "packages/w/src/a.js"),
                ts(2, "packages/w/src/a.d.ts"),
                ts(3, "packages/w/src/b.d.ts"),
                ts(4, "packages/w/src/c.ts"),
                js(5, "packages/w/src/c.js"),
                js(6, "packages/w/lib/x.js"),
                ts(7, "packages/w/lib/x/index.ts"),
                js(8, "packages/w/src/j.js"),
                ts(9, "packages/w/lib/y/main.d.ts"),
                ts(10, "packages/w/lib/y/index.ts"),
                js(11, "packages/v/src/a.js"),
                ts(12, "packages/v/ts/src/a.d.ts"),
            ];
            let config = format!("{{\"compilerOptions\": {{{options}}}}}");
            let manifests = manifests_of(
                &[
                    (
                        "package.json",
                        "{\"workspaces\": [\"packages/*\"], \"devDependencies\": {\"typescript\": \"5.9.3\"}}",
                    ),
                    ("tsconfig.json", &config),
                    ("packages/w/package.json", "{\"name\": \"w\"}"),
                    ("packages/w/theme.css", ""),
                    (
                        "packages/w/lib/y/package.json",
                        "{\"types\": \"./main.d.ts\"}",
                    ),
                    (
                        "packages/v/package.json",
                        "{\"name\": \"v\", \"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                    ),
                ],
                &nodes,
            );
            let graph = build_import_graph(&nodes, &manifests);
            let found: Vec<&str> = graph
                .edges
                .get(&0)
                .into_iter()
                .flatten()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect();
            assert_eq!(found, edges, "{importer} {options}");
            assert_eq!(
                graph.types_versions_undetermined_imports,
                BTreeSet::from(["v/src/a".to_string()]),
                "{importer} {options}"
            );
            let unnamed = match importer {
                "src/app.mts" => BTreeSet::from(["w/src/b".to_string()]),
                _ => BTreeSet::new(),
            };
            assert_eq!(graph.undetermined_imports, unnamed, "{importer} {options}");
        }
    }

    /// Where TypeScript's pass over a workspace package without `exports`
    /// finds no declaration or TypeScript file, it looks in every
    /// `node_modules` from the importer's directory up, `@types` included,
    /// before the package's JavaScript, as tsc 5.9.3, 6.0.3 and 7.0.2 trace
    /// it: a declaration there stands for the name or a subpath and forms no
    /// edge. An ES import under `node16` reads an `@types` directory's
    /// `index` only where its `package.json` exists, and a `typesVersions`
    /// there leaves the specifier open. `node10` reaches the package past a
    /// `paths` JavaScript file its TypeScript pass comes before. The
    /// package's own JavaScript, installed in `node_modules` as its link
    /// puts it, holds no declaration, and keeps its edge (`plain`).
    #[test]
    fn a_declaration_in_node_modules_comes_before_a_workspace_packages_javascript() {
        let js = |id, rel| node_in(id, rel, "", Language::JavaScript);
        let node16 = "\"module\": \"node16\", \"moduleResolution\": \"node16\"";
        let modes = [
            (
                "\"module\": \"esnext\", \"moduleResolution\": \"bundler\"",
                "src/app.ts",
                &[][..],
            ),
            (
                "\"module\": \"commonjs\", \"moduleResolution\": \"node10\", \
                 \"paths\": {\"*\": [\"./third/*\"]}",
                "src/app.ts",
                &[],
            ),
            (node16, "src/app.cts", &[]),
            (node16, "src/app.mts", &["packages/p/index.js"]),
        ];
        for (options, importer, esm) in modes {
            let nodes = vec![
                node_in(
                    0,
                    importer,
                    "import \"w\";\nimport \"w/sub.js\";\nimport \"p\";\n\
                     import \"v\";\nimport \"plain\";\n",
                    Language::TypeScript,
                ),
                js(1, "packages/w/index.js"),
                js(2, "packages/w/sub.js"),
                js(3, "packages/p/index.js"),
                js(4, "packages/v/index.js"),
                js(5, "packages/plain/index.js"),
                js(6, "third/w.js"),
            ];
            let config = format!("{{\"compilerOptions\": {{{options}}}}}");
            let package =
                |name: &str| format!("{{\"name\": \"{name}\", \"main\": \"./index.js\"}}");
            let manifests = manifests_of(
                &[
                    (
                        "package.json",
                        "{\"workspaces\": [\"packages/*\"], \"devDependencies\": {\"typescript\": \"5.9.3\"}}",
                    ),
                    ("tsconfig.json", &config),
                    ("packages/w/package.json", &package("w")),
                    ("packages/p/package.json", &package("p")),
                    ("packages/v/package.json", &package("v")),
                    ("packages/plain/package.json", &package("plain")),
                    ("node_modules/@types/w/package.json", "{}"),
                    ("node_modules/@types/w/index.d.ts", ""),
                    ("node_modules/@types/w/sub.d.ts", ""),
                    ("node_modules/@types/p/index.d.ts", ""),
                    (
                        "node_modules/@types/v/package.json",
                        "{\"typesVersions\": {\"*\": {\"*\": [\"ts/*\"]}}}",
                    ),
                    ("node_modules/@types/v/index.d.ts", ""),
                    ("node_modules/plain/package.json", &package("plain")),
                    ("node_modules/plain/index.js", ""),
                ],
                &nodes,
            );
            let graph = build_import_graph(&nodes, &manifests);
            let edges: Vec<&str> = graph
                .edges
                .get(&0)
                .into_iter()
                .flatten()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect();
            let expected: Vec<&str> = esm
                .iter()
                .copied()
                .chain(["packages/plain/index.js"])
                .collect();
            assert_eq!(edges, expected, "{importer} {options}");
            assert_eq!(
                graph.undetermined_imports,
                BTreeSet::from(["v".to_string()]),
                "{importer} {options}"
            );
        }
    }

    /// A package's `exports` maps by conditions, read in their written order
    /// as TypeScript 6 matches them: `types`, `default` and the configured
    /// `customConditions` always, `import` under `bundler` or for an ES
    /// module under `node16`, `require` for any other file under `node16`,
    /// `node` outside `bundler`. A key the importer's configuration leaves
    /// open — here through an `extends` that names no file — decides nothing
    /// unless the keys after it name the same file.
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
            ts(12, "open/app.ts", imports),
            ts(13, "packages/old/src/main.ts", ""),
            ts(14, "packages/old/src/esm.ts", ""),
            ts(15, "legacy/app.ts", "import o from \"@acme/old\";\n"),
            ts(16, "classic/app.ts", "import o from \"@acme/old\";\n"),
            ts(17, "unset/app.ts", imports),
            ts(18, "cjs/app.mts", "import b from \"@acme/modes\";\n"),
            ts(19, "esm/app.cts", "import b from \"@acme/modes\";\n"),
        ];
        let manifests = manifests_of(
            &[
                (
                    "package.json",
                    "{\"workspaces\": [\"packages/*\"], \
                     \"devDependencies\": {\"typescript\": \"^6.0.3\"}}",
                ),
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
                (
                    "open/tsconfig.json",
                    "{\"extends\": \"./missing.json\", \"compilerOptions\": {\"customConditions\": []}}",
                ),
                (
                    "packages/old/package.json",
                    "{\"name\": \"@acme/old\", \"main\": \"./src/main.ts\", \
                     \"exports\": {\".\": {\"import\": \"./src/esm.ts\"}}}",
                ),
                (
                    "legacy/tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"node\"}}",
                ),
                (
                    "classic/tsconfig.json",
                    "{\"compilerOptions\": {\"moduleResolution\": \"classic\"}}",
                ),
                (
                    "unset/tsconfig.json",
                    "{\"compilerOptions\": {\"strict\": true}}",
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
        // Without a configuration, or one setting neither `moduleResolution`
        // nor `module`, TypeScript 6's defaults resolve as `bundler`.
        assert_eq!(
            edges(3),
            vec![
                "packages/typed/src/index.ts",
                "packages/modes/src/esm.ts",
                "packages/nested/src/other.ts",
                "packages/same/src/index.ts"
            ]
        );
        assert_eq!(edges(17), edges(3));
        // Node reads `.mts` as an ES module and `.cts` as CommonJS, whatever
        // the nearest `package.json` says.
        assert_eq!(edges(18), vec!["packages/modes/src/esm.ts"]);
        assert_eq!(edges(19), vec!["packages/modes/src/cjs.ts"]);
        // Where the configuration leaves `import` and `require` open, only
        // what every reading agrees on is resolved.
        assert_eq!(
            edges(12),
            vec!["packages/typed/src/index.ts", "packages/same/src/index.ts"]
        );
        // `node10` reads no `exports`, `classic` no `package.json`.
        assert_eq!(edges(15), vec!["packages/old/src/main.ts"]);
        assert!(edges(16).is_empty());
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from([
                "@acme/bare".to_string(),
                "@acme/modes".to_string(),
                "@acme/nested".to_string()
            ])
        );
    }

    /// TypeScript resolves an import under `bundler` with the condition
    /// its form and its file's emitted format select (`tsdef_bundler`, and
    /// each case run under tsc 5.9.3, 6.0.3 and 7.0.2): a declaration in a
    /// file emitted as an ES module matches `import`; one in a `.cts` file,
    /// or in any file a `module: commonjs` emits as CommonJS, matches
    /// `require`, and so does every `import x = require()`, under `nodenext`
    /// in an ES module too.
    #[test]
    fn an_import_matches_the_condition_its_form_and_its_files_format_select() {
        let ts = |id, rel, source| node_in(id, rel, source, Language::TypeScript);
        let declaration = "import { main } from 'p6'\n";
        let required = "import p = require('p6')\n";
        let nodes = vec![
            ts(0, "esnext/src/a.ts", declaration),
            ts(1, "esnext/src/b.cts", declaration),
            ts(2, "esnext/src/c.ts", required),
            ts(3, "esnext/src/d.mts", declaration),
            ts(4, "commonjs/src/a.ts", declaration),
            ts(5, "commonjs/src/d.mts", declaration),
            ts(6, "nodenext/src/a.ts", declaration),
            ts(7, "nodenext/src/c.ts", required),
            ts(8, "packages/p6/src/esm.ts", ""),
            ts(9, "packages/p6/src/cjs.ts", ""),
        ];
        let manifests = manifests_of(
            &[
                ("package.json", "{\"workspaces\": [\"packages/*\"]}"),
                (
                    "packages/p6/package.json",
                    "{\"name\": \"p6\", \"exports\": {\".\": {\"import\": \"./src/esm.ts\", \
                     \"require\": \"./src/cjs.ts\"}}}",
                ),
                (
                    "esnext/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"esnext\", \"moduleResolution\": \"bundler\"}}",
                ),
                (
                    "commonjs/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"commonjs\", \"moduleResolution\": \"bundler\"}}",
                ),
                (
                    "nodenext/tsconfig.json",
                    "{\"compilerOptions\": {\"module\": \"nodenext\"}}",
                ),
                ("nodenext/package.json", "{\"type\": \"module\"}"),
            ],
            &nodes,
        );
        let (esm, cjs) = ("packages/p6/src/esm.ts", "packages/p6/src/cjs.ts");
        let edges: Vec<Vec<String>> = (0..8)
            .map(|id| edges_under(&nodes, id, &manifests))
            .collect();
        assert_eq!(
            edges,
            [esm, cjs, cjs, esm, cjs, esm, esm, cjs].map(|file| vec![file.to_string()])
        );
    }

    /// TypeScript's defaults depend on its major, read from the `typescript`
    /// installed nearest above a configuration, else the range the nearest
    /// `package.json` declares — a pnpm catalog entry and an `npm:` alias
    /// among them; the defaults are each major's as tsc 5.9.3, 6.0.3 and
    /// 7.0.2 apply them (`tsdef_none`, `tsdef_target`, `tsdef_cjs`), a major
    /// below 5 read as 5 (`ts4`). Where no major is known, or it is above 7,
    /// an import forms an edge only where every major's reading agrees, and
    /// is disclosed where they differ.
    #[test]
    fn the_defaults_follow_the_typescript_major_a_workspace_uses() {
        let node16 = "{\"compilerOptions\": {\"moduleResolution\": \"node16\"}}";
        let node10 = "{\"compilerOptions\": {\"moduleResolution\": \"node10\"}}";
        let commonjs = "{\"compilerOptions\": {\"module\": \"commonjs\"}}";
        // Each app: its declared `typescript`, the `"type"` its package
        // states, its tsconfig, and the `p6` file an import declaration in
        // its `a.ts` names.
        let apps = [
            ("ts5", "^5.9.0", "", None, Some("types")),
            (
                "ts5t",
                "^5.9.0",
                "",
                Some("{\"compilerOptions\": {\"target\": \"es2022\"}}"),
                None,
            ),
            ("ts5cjs", "^5.9.0", "", Some(commonjs), Some("types")),
            ("ts6cjs", "~6.0", "", Some(commonjs), Some("cjs")),
            // Installed 5.9.3 over a declared ^6.
            ("installed", "^6.0.0", "", Some("{}"), Some("types")),
            // TypeScript 7 has no `node10` and reads it as `bundler`.
            ("cat", "catalog:", "", Some(node10), Some("esm")),
            (
                "alias",
                "npm:@typescript/typescript6@^6.0.2",
                "",
                Some(node10),
                Some("types"),
            ),
            ("named", "catalog:ts6", "", Some(node10), Some("types")),
            ("unknown", "latest", "", Some("{}"), None),
            // A major below 5 reads 5's defaults; one above 7 is unknown.
            ("ts4", "^4.9.5", "", Some("{}"), Some("types")),
            ("ts8", "^8.0.0", "", Some("{}"), None),
            // 5 reads an ES5 `target` without `module` as `commonjs`, 6 reads
            // `amd` as `classic`, and 7 reads `classic` as `bundler`.
            (
                "ts5es5",
                "^5.9.0",
                "",
                Some("{\"compilerOptions\": {\"target\": \"es5\"}}"),
                Some("types"),
            ),
            (
                "ts6amd",
                "~6.0",
                "",
                Some("{\"compilerOptions\": {\"module\": \"amd\"}}"),
                None,
            ),
            (
                "ts7classic",
                "^7.0.2",
                "",
                Some("{\"compilerOptions\": {\"moduleResolution\": \"classic\"}}"),
                Some("esm"),
            ),
            (
                "nodenext",
                ">=5",
                "",
                Some("{\"compilerOptions\": {\"module\": \"nodenext\"}}"),
                Some("cjs"),
            ),
            // `node16` with a `module` of another kind: a `"type"` the
            // package states decides, else the `module` default would.
            (
                "r16cjs",
                "~6.0",
                "\"type\": \"commonjs\", ",
                Some(node16),
                Some("cjs"),
            ),
            (
                "r16esm",
                "^5.9.0",
                "\"type\": \"module\", ",
                Some(node16),
                Some("esm"),
            ),
            ("r16unset", "~6.0", "", Some(node16), Some("esm")),
        ];
        let ts = |id, rel: &str, source: &str| node_in(id, rel, source, Language::TypeScript);
        let declaration = "import { main } from 'p6'\n";
        let required = "import p = require('p6')\n";
        let mut nodes: Vec<Node> = Vec::new();
        let mut expected = Vec::new();
        for (app, .., file) in apps {
            nodes.push(ts(
                nodes.len(),
                &format!("apps/{app}/src/a.ts"),
                declaration,
            ));
            expected.push(file);
        }
        // TypeScript 5's `node10` and 6's `bundler` under `module: commonjs`
        // read a `.cts` file and an `import x = require()` alike.
        for (app, file, source, target) in [
            ("ts5", "b.cts", declaration, "types"),
            ("ts5", "c.ts", required, "types"),
            ("ts6cjs", "b.cts", declaration, "cjs"),
            ("ts6cjs", "c.ts", required, "cjs"),
        ] {
            nodes.push(ts(nodes.len(), &format!("apps/{app}/src/{file}"), source));
            expected.push(Some(target));
        }
        let importers = nodes.len();
        for file in ["main", "types", "esm", "cjs"] {
            nodes.push(ts(nodes.len(), &format!("packages/p6/src/{file}.ts"), ""));
        }
        let mut files: Vec<(String, String)> = vec![
            (
                "pnpm-workspace.yaml".into(),
                "packages:\n  - 'packages/*'\n  - 'apps/*'\n\ncatalog:\n  # the compiler\n  \
                 typescript: ^7.0.2\ncatalogs:\n  ts6:\n    \"typescript\": '~6.0' # API\n"
                    .into(),
            ),
            (
                "packages/p6/package.json".into(),
                "{\"name\": \"p6\", \"main\": \"./src/main.ts\", \"types\": \"./src/types.ts\", \
                 \"exports\": {\".\": {\"import\": \"./src/esm.ts\", \"require\": \"./src/cjs.ts\"}}}"
                    .into(),
            ),
            (
                "apps/installed/node_modules/typescript/package.json".into(),
                "{\"name\": \"typescript\", \"version\": \"5.9.3\"}".into(),
            ),
        ];
        for (app, range, kind, config, _) in apps {
            files.push((
                format!("apps/{app}/package.json"),
                format!("{{{kind}\"devDependencies\": {{\"typescript\": \"{range}\"}}}}"),
            ));
            files.extend(config.map(|config| (format!("apps/{app}/tsconfig.json"), config.into())));
        }
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, contents)| (path.as_str(), contents.as_str()))
            .collect();
        let manifests = manifests_of(&files, &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges: Vec<Option<&str>> = (0..importers)
            .map(|id| {
                let found: Vec<&str> = graph.edges[&id]
                    .iter()
                    .map(|dst| &nodes[*dst].rel_path["packages/p6/src/".len()..])
                    .collect();
                match found.as_slice() {
                    [] => None,
                    [file] => file.strip_suffix(".ts"),
                    _ => panic!("{} names several files: {found:?}", nodes[id].rel_path),
                }
            })
            .collect();
        assert_eq!(edges, expected);
        // Where the majors disagree (`unknown`, `ts8`), the import is
        // disclosed as one the TypeScript version decides, with the ranges
        // declared there.
        assert!(graph.undetermined_imports.is_empty());
        assert_eq!(
            graph.version_undetermined_imports,
            BTreeSet::from(["p6".to_string()])
        );
        assert_eq!(
            graph.typescript_ranges,
            BTreeSet::from(["^8.0.0".to_string(), "latest".to_string()])
        );
        assert!(graph.typescript_not_installed && !graph.typescript_installed_unread);
    }

    /// A file TypeScript does not compile — a JavaScript file no
    /// configuration takes, or any file where no TypeScript is installed or
    /// declared — is resolved by Node or a bundler, so a condition stands
    /// only where they agree: `default`, `import` for a declaration and
    /// `require` for a `require`, and never `types`; `node`, `browser` and
    /// the rest some of them match and others do not (`jsnode`, where Node
    /// prints the `node` file and bundlers take the `import` one). A
    /// TypeScript file under a declared range fixing no major is
    /// TypeScript's: tsc 5.9.3, 6.0.3 and 7.0.2 each compile
    /// `scripts/gen.ts` without a configuration to the package's `types`,
    /// where Node would take its unbuilt `main` (`latest_noconfig`).
    #[test]
    fn a_file_typescript_does_not_compile_matches_what_node_and_bundlers_agree_on() {
        let imports = "import a from 'p7'\nimport b from 'p8'\nimport c from 'p9'\n\
                       import d from 'p10'\nimport e from 'p11'\n";
        let nodes = vec![
            node_in(0, "apps/cli/src/a.js", imports, Language::JavaScript),
            node_in(
                1,
                "apps/cli/src/t.ts",
                "import p = require('p8')\n",
                Language::TypeScript,
            ),
            node_in(
                2,
                "tools/x.mjs",
                "import b from 'p8'\n",
                Language::JavaScript,
            ),
            node_in(3, "packages/p7/src/node.js", "", Language::JavaScript),
            node_in(4, "packages/p7/src/esm.js", "", Language::JavaScript),
            node_in(5, "packages/p8/src/esm.js", "", Language::JavaScript),
            node_in(6, "packages/p8/src/cjs.js", "", Language::JavaScript),
            node_in(7, "packages/p9/src/x.d.ts", "", Language::TypeScript),
            node_in(8, "packages/p9/src/def.js", "", Language::JavaScript),
            node_in(9, "packages/p10/src/b.js", "", Language::JavaScript),
            node_in(10, "packages/p10/src/d.js", "", Language::JavaScript),
            node_in(11, "packages/p11/src/index.ts", "", Language::TypeScript),
            node_in(
                12,
                "scripts/gen.ts",
                "import e from 'p11'\n",
                Language::TypeScript,
            ),
        ];
        let manifests = manifests_of(
            &[
                ("package.json", "{\"workspaces\": [\"packages/*\"]}"),
                ("apps/cli/package.json", "{\"type\": \"module\"}"),
                // TypeScript is declared, but no configuration takes the file.
                (
                    "tools/package.json",
                    "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}",
                ),
                (
                    "packages/p7/package.json",
                    "{\"name\": \"p7\", \"exports\": {\".\": {\"node\": \"./src/node.js\", \
                     \"import\": \"./src/esm.js\", \"default\": \"./src/esm.js\"}}}",
                ),
                (
                    "packages/p8/package.json",
                    "{\"name\": \"p8\", \"exports\": {\".\": {\"import\": \"./src/esm.js\", \
                     \"require\": \"./src/cjs.js\"}}}",
                ),
                (
                    "packages/p9/package.json",
                    "{\"name\": \"p9\", \"exports\": {\".\": {\"types\": \"./src/x.d.ts\", \
                     \"default\": \"./src/def.js\"}}}",
                ),
                (
                    "packages/p10/package.json",
                    "{\"name\": \"p10\", \"exports\": {\".\": {\"browser\": \"./src/b.js\", \
                     \"default\": \"./src/d.js\"}}}",
                ),
                (
                    "packages/p11/package.json",
                    "{\"name\": \"p11\", \"types\": \"./src/index.ts\", \"main\": \"./dist/index.js\"}",
                ),
                // A range fixing no major: TypeScript compiles the file under
                // the defaults of 5, 6 and 7, which agree on `types`.
                (
                    "scripts/package.json",
                    "{\"devDependencies\": {\"typescript\": \"latest\"}}",
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
            vec!["packages/p8/src/esm.js", "packages/p9/src/def.js"]
        );
        assert_eq!(edges(1), vec!["packages/p8/src/cjs.js"]);
        assert_eq!(edges(2), vec!["packages/p8/src/esm.js"]);
        assert_eq!(edges(12), vec!["packages/p11/src/index.ts"]);
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["p10".to_string(), "p11".to_string(), "p7".to_string()])
        );
    }

    /// Node and the bundlers take a package without `exports` for its
    /// `main`, else its `index`, never its `types` (Node 26.12.1 resolves
    /// each). A bundler's `module` or `browser` field naming another file
    /// leaves which one open, but not a subpath's file, which esbuild 0.28.2
    /// takes on either platform as Node does. They stop at the first
    /// `exports` key that matches, whatever its target names, where tsc
    /// 5.9.3, 6.0.3 and 7.0.2 pass over a target naming no file to the next
    /// key, and so to an array's next entry, where Node stops at the first.
    #[test]
    fn node_and_the_bundlers_take_a_packages_main_and_stop_at_its_first_matched_export() {
        let names = [
            "pm", "pmi", "pmod", "same", "pbs", "pbo", "pfs", "pdist", "pend", "parr",
        ];
        let imports: String = names
            .iter()
            .map(|name| format!("import {name} from '{name}'\n"))
            .chain(["import format from 'pmod/src/format.js'\n".to_string()])
            .collect();
        let mut nodes = vec![
            node_in(0, "app/a.js", &imports, Language::JavaScript),
            node_in(
                1,
                "ts/b.ts",
                "import pend from 'pend'\nimport parr from 'parr'\n",
                Language::TypeScript,
            ),
        ];
        for file in [
            "pm/src/main.js",
            "pm/src/types.ts",
            "pmi/index.js",
            "pmod/src/main.js",
            "pmod/src/esm.js",
            "pmod/src/format.js",
            "same/src/main.js",
            "pbs/src/main.js",
            "pbs/src/browser.js",
            "pbo/src/main.js",
            "pbo/src/browser.js",
            "pfs/src/main.js",
            "pdist/src/index.ts",
            "pend/src/index.js",
            "parr/src/index.ts",
        ] {
            let language = if file.ends_with(".ts") {
                Language::TypeScript
            } else {
                Language::JavaScript
            };
            nodes.push(node_in(
                nodes.len(),
                &format!("packages/{file}"),
                "",
                language,
            ));
        }
        let package = |name: &str, fields: &str| {
            (
                format!("packages/{name}/package.json"),
                format!("{{\"name\": \"{name}\", {fields}}}"),
            )
        };
        let main = "\"main\": \"./src/main.js\"";
        let configs = [
            (
                "package.json".to_string(),
                "{\"workspaces\": [\"packages/*\"]}".to_string(),
            ),
            (
                "ts/package.json".to_string(),
                "{\"devDependencies\": {\"typescript\": \"^5.9.0\"}}".to_string(),
            ),
            (
                "ts/tsconfig.json".to_string(),
                "{\"compilerOptions\": {\"module\": \"esnext\", \"moduleResolution\": \"bundler\"}}"
                    .to_string(),
            ),
            package("pm", &format!("{main}, \"types\": \"./src/types.ts\"")),
            package("pmi", "\"main\": \"./dist/index.js\""),
            package("pmod", &format!("{main}, \"module\": \"./src/esm.js\"")),
            package("same", &format!("{main}, \"module\": \"./src/main.js\"")),
            package("pbs", &format!("{main}, \"browser\": \"./src/browser.js\"")),
            package(
                "pbo",
                &format!("{main}, \"browser\": {{\"./src/main.js\": \"./src/browser.js\"}}"),
            ),
            package("pfs", &format!("{main}, \"browser\": {{\"fs\": false}}")),
            package(
                "pdist",
                "\"main\": \"./dist/index.js\", \"types\": \"./src/index.ts\"",
            ),
            package(
                "pend",
                "\"exports\": {\".\": {\"import\": \"./dist/index.mjs\", \
                 \"default\": \"./src/index.js\"}}",
            ),
            package(
                "parr",
                "\"exports\": {\".\": [\"./dist/index.js\", \"./src/index.ts\"]}",
            ),
        ];
        let configs: Vec<(&str, &str)> = configs
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let manifests = manifests_of(&configs, &nodes);
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
                "packages/pm/src/main.js",
                "packages/pmi/index.js",
                "packages/same/src/main.js",
                "packages/pfs/src/main.js",
                "packages/pmod/src/format.js"
            ]
        );
        assert_eq!(
            edges(1),
            vec!["packages/pend/src/index.js", "packages/parr/src/index.ts"]
        );
        assert_eq!(
            graph.undetermined_imports,
            ["parr", "pbo", "pbs", "pdist", "pend", "pmod"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        );
    }

    /// A configuration `extends` a package's configuration as TypeScript
    /// resolves the specifier (each case run under tsc 5.9.3, 6.0.3 and
    /// 7.0.2): the file the package's `exports` maps the subpath to under
    /// `require`, the package's `tsconfig.json` for its bare name, through
    /// the workspace package of that name where none is installed, and at
    /// its real path where one is linked. An `extends` that names no file
    /// leaves the options it could set open, so an import their conditions
    /// decide is disclosed; one the extending configuration sets itself is
    /// read, and any condition the configuration could add stays open.
    #[test]
    fn a_configuration_extends_a_package_as_typescript_resolves_its_specifier() {
        let ts = |id, rel: &str| {
            let source =
                "import { main } from 'p6'\nimport { gen } from '#gen'\nimport { x } from 'p7'\n";
            node_in(id, rel, source, Language::TypeScript)
        };
        let nodes = vec![
            ts(0, "apps/a/src/a.ts"),
            ts(1, "apps/b/src/b.ts"),
            ts(2, "apps/c/src/c.ts"),
            ts(3, "apps/d/src/d.ts"),
            ts(4, "apps/e/src/e.ts"),
            ts(5, "apps/f/src/f.ts"),
            node_in(6, "packages/p6/src/esm.ts", "", Language::TypeScript),
            node_in(7, "packages/p6/src/cjs.ts", "", Language::TypeScript),
            node_in(8, "packages/cfg2/gen.ts", "", Language::TypeScript),
            node_in(9, "packages/p7/src/source.ts", "", Language::TypeScript),
            node_in(10, "packages/p7/src/default.ts", "", Language::TypeScript),
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        let files = [
            (
                "package.json",
                "{\"workspaces\": [\"packages/*\", \"apps/*\"]}",
            ),
            (
                "packages/cfg/package.json",
                "{\"name\": \"@repo/cfg\", \"exports\": {\"./base\": {\"import\": \"./esm.json\", \
                 \"require\": \"./node.json\"}}}",
            ),
            (
                "packages/cfg/node.json",
                "{\"compilerOptions\": {\"module\": \"nodenext\"}}",
            ),
            (
                "packages/cfg/esm.json",
                "{\"compilerOptions\": {\"moduleResolution\": \"bundler\"}}",
            ),
            (
                "packages/p6/package.json",
                "{\"name\": \"p6\", \"exports\": {\".\": {\"import\": \"./src/esm.ts\", \
                 \"require\": \"./src/cjs.ts\"}}}",
            ),
            (
                "packages/p7/package.json",
                "{\"name\": \"p7\", \"exports\": {\".\": {\"source\": \"./src/source.ts\", \
                 \"default\": \"./src/default.ts\"}}}",
            ),
            (
                "node_modules/@tsconfig/node20/package.json",
                "{\"name\": \"@tsconfig/node20\"}",
            ),
            (
                "node_modules/@tsconfig/node20/tsconfig.json",
                "{\"compilerOptions\": {\"module\": \"node16\"}}",
            ),
            ("packages/cfg2/package.json", "{\"name\": \"@repo/cfg2\"}"),
            (
                "packages/cfg2/tsconfig.json",
                "{\"compilerOptions\": {\"module\": \"nodenext\", \
                 \"paths\": {\"#gen\": [\"./gen.ts\"]}}}",
            ),
            ("apps/a/tsconfig.json", "{\"extends\": \"@repo/cfg/base\"}"),
            (
                "apps/b/tsconfig.json",
                "{\"extends\": \"@tsconfig/node20\"}",
            ),
            (
                "apps/c/tsconfig.json",
                "{\"extends\": \"@repo/missing/base.json\"}",
            ),
            (
                "apps/d/tsconfig.json",
                "{\"extends\": \"@repo/missing/base.json\", \"compilerOptions\": \
                 {\"module\": \"nodenext\", \"moduleResolution\": \"nodenext\"}}",
            ),
            ("apps/e/tsconfig.json", "{\"extends\": \"./base.json\"}"),
            ("apps/f/tsconfig.json", "{\"extends\": \"@repo/cfg2\"}"),
        ];
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
        let linked = dir.path().join("apps/f/node_modules/@repo");
        std::fs::create_dir_all(&linked).expect("mkdir");
        std::os::unix::fs::symlink(dir.path().join("packages/cfg2"), linked.join("cfg2"))
            .expect("symlink");
        let manifests = Manifests::read(dir.path(), &nodes);
        let graph = build_import_graph(&nodes, &manifests);
        let edges = |id: usize| -> Vec<&str> {
            graph.edges[&id]
                .iter()
                .map(|dst| nodes[*dst].rel_path.as_str())
                .collect()
        };
        let (cjs, default) = ("packages/p6/src/cjs.ts", "packages/p7/src/default.ts");
        assert_eq!(edges(0), vec![cjs, default]);
        assert_eq!(edges(1), vec![cjs, default]);
        assert_eq!(edges(3), vec![cjs]);
        assert_eq!(edges(5), vec![cjs, "packages/cfg2/gen.ts", default]);
        assert!(edges(2).is_empty() && edges(4).is_empty());
        assert_eq!(
            graph.undetermined_imports,
            BTreeSet::from(["p6".to_string(), "p7".to_string()])
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

    /// A path that goes into an item — an enum's variants, a type's
    /// associated items — names the module holding that item: the deepest
    /// module of the crate the path passes through.
    #[test]
    fn a_rust_path_into_an_item_names_the_module_holding_the_item() {
        let nodes = vec![
            node(0, "src/lib.rs", "mod model;\nmod a;\npub struct Thing;\n"),
            node(
                1,
                "src/model/mod.rs",
                "pub mod kind;\npub enum Shape { Round }\n",
            ),
            node(2, "src/model/kind.rs", "pub enum Kind { A, B }\n"),
            node(
                3,
                "src/a.rs",
                "use crate::model::kind::Kind::{A, B};\nuse crate::model::Shape::Round;\n\
                 use super::model::kind::Kind::A as K;\nuse crate::Thing::new;\n",
            ),
        ];
        assert_eq!(
            rust_edges(&nodes, 3),
            vec!["src/model/kind.rs", "src/model/mod.rs", "src/lib.rs"]
        );
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
        // The library builds `util.rs` too, and there `config` is no module:
        // for the path to compile, the library's root binds the name.
        assert_eq!(edges(2), vec!["src/lib.rs", "src/config.rs"]);
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
