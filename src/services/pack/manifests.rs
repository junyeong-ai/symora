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
    /// The directories pytest takes for its root where no configuration
    /// file above a test configures it (`Rootdir`).
    pub(super) pytest_rootdirs: HashMap<Vec<String>, Rootdir>,
    /// The TypeScript projects the tree's configurations describe.
    pub(super) ts_projects: TsProjects,
    /// Each package a JavaScript workspace lists, by its directory.
    pub(super) js_packages: HashMap<Vec<String>, JsPackage>,
    /// What each `package.json` above a JavaScript or TypeScript file
    /// declares, by its directory: the nearest is a file's package scope, as
    /// Node and TypeScript read it (`getPackageScopeForPath`).
    pub(super) package_scopes: HashMap<Vec<String>, PackageScope>,
    /// The TypeScript versions that may compile the files of each directory
    /// holding a JavaScript or TypeScript file (`TypeScriptVersions`).
    pub(super) typescript: HashMap<Vec<String>, TypeScript>,
}

/// What a `package.json` declares for the files it is the nearest of.
#[derive(Debug, Clone)]
pub(super) struct PackageScope {
    /// Its `"type"`, which decides whether Node reads a `.js` or `.ts` file
    /// there as an ES module.
    pub(super) kind: PackageType,
    /// Its `name` and `exports`, which a file reads the package's own name
    /// through, whether or not a workspace lists the package.
    pub(super) name: Option<String>,
    pub(super) exports: Option<serde_json::Value>,
    /// Its `imports`, which a file reads a `#` specifier through.
    pub(super) imports: Option<serde_json::Value>,
}

/// The `"type"` a `package.json` declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PackageType {
    Module,
    CommonJs,
    /// Neither, which Node reads as CommonJS where it must choose.
    Unset,
}

/// The TypeScript versions that may compile a directory's files, by major.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum TypeScript {
    /// Neither installed nor declared above the directory.
    #[default]
    Absent,
    /// The major installed or declared, one below 5 read as 5 (`of_major`).
    Major(u8),
    /// A major above those whose defaults pack reads (`MAJORS`), installed
    /// or declared (`^8.0.0`): read as any of them, though it excludes every
    /// major below 7; the declared range where one is written.
    Above(Option<String>),
    /// Installed at a version that is no number, or declared as a range pack
    /// reads no major from (`typescript_major`), so any of them; the
    /// declared range where one is written.
    Unknown(Option<String>),
}

/// The TypeScript majors whose defaults pack reads, measured with tsc
/// 5.9.3, 6.0.3 and 7.0.2; a 5.x release is read as 5.9.3 reads.
const MAJORS: [u8; 3] = [5, 6, 7];

/// Whether TypeScript of `major` reads `baseUrl`, which 7 removed.
pub(super) fn reads_base_url(major: u8) -> bool {
    major < 7
}

impl TypeScript {
    /// The majors that may compile the files: every one pack reads where
    /// none is known.
    pub(super) fn majors(&self) -> &[u8] {
        match self {
            Self::Major(major) => std::slice::from_ref(major),
            Self::Absent | Self::Above(_) | Self::Unknown(_) => &MAJORS,
        }
    }

    /// What the version installed, or the `range` declared, is read as: one
    /// below 5 as 5, whose `module` and `moduleResolution` defaults
    /// TypeScript's documentation gives every earlier major too (`commonjs`
    /// resolving as `node10`, an ES `module` as `classic`); one above 7 as
    /// above them, and none as unknown.
    fn of_major(major: Option<u64>, range: Option<String>) -> Self {
        match major {
            Some(major) if major < u64::from(MAJORS[0]) => Self::Major(MAJORS[0]),
            Some(major) => match u8::try_from(major) {
                Ok(major) if MAJORS.contains(&major) => Self::Major(major),
                _ => Self::Above(range),
            },
            None => Self::Unknown(range),
        }
    }
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
    /// The `main` Node and the bundlers take; TypeScript reads the fields
    /// of the package's `package.json` on disk (`directory_module`).
    pub(super) main: Option<String>,
    /// The bundlers' `module` field, and their `browser` field: a path, or
    /// an object replacing the package's own paths.
    pub(super) module: Option<String>,
    pub(super) browser: Option<serde_json::Value>,
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
/// it says about its files' specifiers, what it says about a package's own
/// name read from inside it, the files it compiles itself, and the projects
/// its `references` name, by their indices in `TsProjects`.
#[derive(Debug, Clone, Default)]
struct TsProject {
    config: TsConfig,
    self_name: SelfName,
    sources: TsSources,
    references: Vec<usize>,
}

/// The options tsc 5.9.3, 6.0.3 and 7.0.2 read a package's own name from
/// inside it with (`loadModuleFromSelfNameReference`), each against the
/// directory of the configuration setting it: whether they read JavaScript,
/// which has them walk the package's `exports` once with every extension,
/// and the output directories and root they map a target in an output
/// directory back to its input by (`tryLoadInputFileForPath`). They are
/// kept beside the project's `TsConfig`, outside the equality that decides
/// whether projects compiling a file say different things about its
/// specifiers: projects differing in these alone read every other specifier
/// alike. Each is `None` where a configuration the chain could not read may
/// set it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SelfName {
    /// The directory of the configuration file the project is built from,
    /// which has to lie in the package's directory for tsc to map a target.
    pub(super) directory: Vec<String>,
    /// `declarationDir`, `outDir` and `rootDir`, as paths on disk;
    /// `Some(None)` where the chain sets none.
    pub(super) declaration_dir: Option<Option<PathBuf>>,
    pub(super) out_dir: Option<Option<PathBuf>>,
    pub(super) root_dir: Option<Option<PathBuf>>,
    /// Whether `composite` is set.
    pub(super) composite: Option<bool>,
    /// Whether tsc reads JavaScript: `allowJs`, else `checkJs`, which a
    /// `jsconfig.json` reads as `allowJs` unless it sets it.
    pub(super) allow_js: Option<bool>,
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
    /// The configuration above it has `references`, and neither it nor a
    /// project they reach takes it, so `tsc -b` builds it in none: the
    /// configurations of that nearest project, by its index, and of those it
    /// reaches (`Manifests::reached`) are what an import's writer could have
    /// meant.
    Uncompiled(usize),
    /// The projects that compile it, of that configuration and those its
    /// `references` reach, say different things about specifiers: their
    /// configurations.
    Undetermined(Vec<&'a TsConfig>),
}

impl TsProjects {
    /// The configurations of the project at `project` and of every project
    /// its references reach, each once.
    fn reached(&self, project: usize) -> Vec<&TsConfig> {
        self.reached_projects(project)
            .into_iter()
            .map(|at| &self.projects[at].config)
            .collect()
    }

    /// The project at `project` and every project its references reach, by
    /// their indices, each once.
    fn reached_projects(&self, project: usize) -> Vec<usize> {
        let mut reached = Vec::new();
        let mut visited = HashSet::new();
        let mut pending = vec![project];
        while let Some(at) = pending.pop() {
            if visited.insert(at) {
                reached.push(at);
                pending.extend(self.projects[at].references.iter().rev());
            }
        }
        reached
    }

    /// The configurations `file` is compiled under in the project at
    /// `project`: its own where it has no `references`, a configuration's
    /// files being whatever is below it; else, as `tsc -b` builds them, those
    /// of the projects among it and those its references reach, each once,
    /// whose `files`, `include` and `exclude` take the file and that reach
    /// no other project taking it. A project's program reads a file a
    /// project it reaches compiles as that project's output declaration,
    /// as tsc 5.9.3, 6.0.3 and 7.0.2 all build it.
    fn owners(&self, project: usize, file: &Path) -> Vec<&TsConfig> {
        self.owning(project, file)
            .into_iter()
            .map(|at| &self.projects[at].config)
            .collect()
    }

    /// The projects whose configurations `file` is compiled under
    /// (`owners`), by their indices.
    fn owning(&self, project: usize, file: &Path) -> Vec<usize> {
        if self.projects[project].references.is_empty() {
            return vec![project];
        }
        let taking: Vec<usize> = self
            .reached_projects(project)
            .into_iter()
            .filter(|at| self.projects[*at].sources.covers(file))
            .collect();
        taking
            .iter()
            .copied()
            .filter(|at| {
                !self
                    .reached_projects(*at)
                    .into_iter()
                    .any(|below| below != *at && taking.contains(&below))
            })
            .collect()
    }
}

/// A `paths` substitution as written, `${configDir}` replaced, with the
/// directory that template fixes it against (`below_config_dir`).
pub(super) type Substitution = (Option<Vec<String>>, String);

/// What a TypeScript configuration, with every configuration it extends,
/// says about the specifiers its files import: the directory `baseUrl`
/// names, each `paths` pattern with its substitutions, which resolve
/// against `baseUrl` or, without one, the directory of the configuration
/// declaring them, and the conditions a package's `exports` is read under.
/// TypeScript 7 reads no `baseUrl` (tsc 7.0.2 reports TS5102 and resolves
/// `paths` against the declaring configuration's directory).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TsConfig {
    pub(super) base_url: Option<Vec<String>>,
    pub(super) paths: Vec<(String, Vec<Substitution>)>,
    pub(super) paths_base: Vec<String>,
    /// The directory of the configuration declaring `paths`, which its
    /// substitutions resolve against where `baseUrl` is not read.
    pub(super) paths_directory: Vec<String>,
    /// The options that, with the TypeScript version compiling it, select
    /// how a package's name resolves for its files.
    pub(super) module_options: ModuleOptions,
    /// The TypeScript versions that may compile it, as its directory's
    /// `package.json` files and `node_modules` say (`TypeScriptVersions`).
    pub(super) typescript: TypeScript,
    /// The conditions `customConditions` adds; `None` where a configuration
    /// the chain extends that could not be read may add any.
    pub(super) custom_conditions: Option<Vec<String>>,
    /// Whether a `jsconfig.json` holds it: a JavaScript project, which a
    /// bundler builds whatever TypeScript is installed.
    pub(super) javascript: bool,
    /// The directories `typeRoots` names, each against the directory of the
    /// configuration declaring it, as paths on disk: `classic` resolution
    /// looks a specifier up in them after `node_modules/@types`. Empty where
    /// the chain sets none, as tsc then looks in none.
    pub(super) type_roots: Vec<PathBuf>,
}

impl TsConfig {
    /// The TypeScript majors that may compile the configuration's files.
    /// A chain setting an option tsc 7.0.2 rejects as an error is one no
    /// TypeScript 7 compiles: `baseUrl` (TS5102), or a module option 7
    /// rejects that pack reads apart under 6 and 7
    /// (`ModuleOptions::removed_in_7`); `outFile` (TS5102) is rejected too,
    /// but changes no option pack reads. Such a chain is read under 5 and 6
    /// alone where none is installed or declared, where the version
    /// installed is no number, and where a declared range admits a major
    /// below 7 (`range_admits_below_7`): that is, unless what pack knows of
    /// the TypeScript excludes every major below 7. A major above 7
    /// installed or declared, and a declared range admitting only 7 and
    /// above (`>=7`), keep 7, so what the majors read apart there is
    /// disclosed: no major they admit builds the project.
    pub(super) fn majors(&self) -> Vec<u8> {
        let rejected_by_7 = self.base_url.is_some() || self.module_options.removed_in_7();
        let refused = rejected_by_7
            && match &self.typescript {
                TypeScript::Major(_) | TypeScript::Above(_) => false,
                TypeScript::Absent | TypeScript::Unknown(None) => true,
                TypeScript::Unknown(Some(range)) => range_admits_below_7(range),
            };
        self.typescript
            .majors()
            .iter()
            .copied()
            .filter(|major| !refused || *major != 7)
            .collect()
    }
}

/// What a configuration's `extends` chain says of one compiler option.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Setting {
    /// No configuration of the chain sets it.
    #[default]
    Unset,
    /// The value the nearest configuration setting it gives, in lower case.
    Set(String),
    /// A configuration the chain extends that could not be read may set it,
    /// or the value set is no string.
    Unknown,
}

impl Setting {
    fn value(&self) -> Option<Option<&str>> {
        match self {
            Self::Unset => Some(None),
            Self::Set(value) => Some(Some(value)),
            Self::Unknown => None,
        }
    }
}

/// The compiler options that, with the TypeScript version compiling a
/// configuration's files, select how they resolve a package's name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ModuleOptions {
    pub(super) module: Setting,
    pub(super) module_resolution: Setting,
    pub(super) target: Setting,
}

/// The module system TypeScript emits a file in, by the `module` option or
/// the default its `target` implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModuleKind {
    CommonJs,
    /// `es2015` to `esnext`.
    Es,
    Preserve,
    /// `node16` to `nodenext`, which emit each file as Node reads it.
    Node,
    /// `amd`, `umd`, `system` or `none`.
    Other,
}

impl ModuleOptions {
    /// Whether the chain sets a module option tsc 7.0.2 rejects as an error
    /// and pack reads apart under 6 and 7: `moduleResolution` `node`,
    /// `node10` or `classic`, `module` `amd`, `umd` or `system` (TS5108),
    /// and `module` `none` (TS6046). It also rejects an ES3 or ES5 `target`,
    /// which pack reads alike under 6 and 7.
    pub(super) fn removed_in_7(&self) -> bool {
        matches!(
            self.module_resolution.value(),
            Some(Some("node" | "node10" | "classic"))
        ) || matches!(
            self.module.value(),
            Some(Some("amd" | "umd" | "system" | "none"))
        )
    }

    /// The module system TypeScript of `major` emits a file in, as tsc
    /// 5.9.3, 6.0.3 and 7.0.2 do: the `module` set, else `commonjs` for an
    /// ES5 `target` and `es2015` from ES2015 — the default `target` being ES5
    /// in TypeScript 5 and a later one in 6 and 7. An ES3 `target` is read as
    /// `commonjs` too, a project no tsc from 5 on builds: 5.9.3 and 6.0.3
    /// reject it (TS5108) and 7.0.2 does (TS6046), where 6.0.3 and 7.0.2
    /// resolve an import as from an ES module. `None` where an option it
    /// reads is unknown, or a value no version names.
    pub(super) fn module_kind(&self, major: u8) -> Option<ModuleKind> {
        Some(match self.module.value()? {
            Some("commonjs") => ModuleKind::CommonJs,
            Some("es6" | "es2015" | "es2020" | "es2022" | "esnext") => ModuleKind::Es,
            Some("preserve") => ModuleKind::Preserve,
            Some("node16" | "node18" | "node20" | "nodenext") => ModuleKind::Node,
            Some("amd" | "umd" | "system" | "none") => ModuleKind::Other,
            Some(_) => return None,
            None => match self.target.value()? {
                None if major == 5 => ModuleKind::CommonJs,
                None => ModuleKind::Es,
                Some("es3" | "es5") => ModuleKind::CommonJs,
                Some("es6" | "esnext") => ModuleKind::Es,
                Some(target) => {
                    let year = target.strip_prefix("es")?.parse::<u16>().ok()?;
                    if year < 2015 {
                        return None;
                    }
                    ModuleKind::Es
                }
            },
        })
    }

    /// The module resolution TypeScript of `major` reads a package's name
    /// under, as tsc 5.9.3, 6.0.3 and 7.0.2 do. An explicit
    /// `moduleResolution` holds, but TypeScript 7 no longer has `node10` or
    /// `classic` and reads either as `bundler`. Without one the module kind
    /// implies it: `node16` to `nodenext` themselves, and otherwise in
    /// TypeScript 5 `commonjs` `node10`, `preserve` `bundler` and the rest
    /// `classic`; in 6 `amd`, `umd`, `system` and `none` `classic` and the
    /// rest `bundler`; in 7 `bundler`. `None` where an option it reads is
    /// unknown, or a value no version names.
    pub(super) fn resolution(
        &self,
        major: u8,
        kind: Option<ModuleKind>,
    ) -> Option<ModuleResolution> {
        Some(match self.module_resolution.value()? {
            Some("bundler") => ModuleResolution::Bundler,
            Some("node16" | "nodenext") => ModuleResolution::Node16,
            Some("node" | "node10") if major >= 7 => ModuleResolution::Bundler,
            Some("node" | "node10") => ModuleResolution::Node10,
            Some("classic") if major >= 7 => ModuleResolution::Bundler,
            Some("classic") => ModuleResolution::Classic,
            Some(_) => return None,
            None => match (kind?, major) {
                (ModuleKind::Node, _) => ModuleResolution::Node16,
                (ModuleKind::Preserve, _) => ModuleResolution::Bundler,
                (ModuleKind::CommonJs, 5) => ModuleResolution::Node10,
                (ModuleKind::Es | ModuleKind::Other, 5) => ModuleResolution::Classic,
                (ModuleKind::Other, 6) => ModuleResolution::Classic,
                _ => ModuleResolution::Bundler,
            },
        })
    }
}

/// A module resolution strategy that says how a package's `exports` is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModuleResolution {
    /// `bundler`: an import matches `import` or `require` by its form and
    /// the format its file is emitted in, never `node`.
    Bundler,
    /// `node16` or `nodenext`: an import matches `node`, and `import` or
    /// `require` by its form and the format Node reads its file in.
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
    pub(super) paths: Vec<Substituted>,
    /// Whether the pattern is `*`, which every specifier matches: one it maps
    /// to no file is looked up as a package, as any unmapped one is.
    pub(super) catch_all: bool,
}

impl TsConfig {
    /// What the `paths` pattern a specifier matches maps it to, or `None`
    /// where it matches none, its substitutions resolving against `baseUrl`
    /// where `base_url` says the compiler reads it. An exact pattern wins,
    /// then the one whose prefix before `*` is longest, as TypeScript picks.
    pub(super) fn mapped(&self, specifier: &str, base_url: bool) -> Option<Mapping> {
        let base = if base_url {
            &self.paths_base
        } else {
            &self.paths_directory
        };
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
                .filter_map(|(fixed, substitution)| {
                    let base = fixed.as_ref().unwrap_or(base);
                    Some(Substituted {
                        path: project_path(base, &substitution.replacen('*', star, 1))?,
                        extension: SCRIPT_EXTENSIONS
                            .iter()
                            .any(|extension| has_extension(substitution, extension)),
                    })
                })
                .collect(),
            catch_all: pattern == "*",
        })
    }
}

/// The project path a `paths` substitution names for a specifier.
pub(super) struct Substituted {
    pub(super) path: Vec<String>,
    /// Whether the substitution writes an extension TypeScript knows of its
    /// own (`./src/*.js`), so that tsc tries the file it names before any
    /// it names with an extension in place of that one or added.
    pub(super) extension: bool,
}

/// The extensions TypeScript strips from a path it looks a module up by
/// (`removeFileExtension`), the longer of two that both end a name first.
const SCRIPT_EXTENSIONS: [&str; 12] = [
    ".d.ts", ".d.mts", ".d.cts", ".mjs", ".mts", ".cjs", ".cts", ".ts", ".js", ".tsx", ".jsx",
    ".json",
];

/// A specifier as TypeScript looks it up in a `node_modules/@types`: a scope
/// mangled (`@scope/name/sub` as `scope__name/sub`).
pub(super) fn mangle_scope(specifier: &str) -> String {
    specifier.strip_prefix('@').map_or_else(
        || specifier.to_string(),
        |scoped| scoped.replacen('/', "__", 1),
    )
}

/// The name TypeScript looks a specifier up by below a `typeRoots`
/// directory: the specifier as written, its scope mangled only where the
/// directory is itself a `node_modules/@types`.
pub(super) fn type_root_name(type_root: &Path, specifier: &str) -> String {
    match type_root.ends_with("node_modules/@types") {
        true => mangle_scope(specifier),
        false => specifier.to_string(),
    }
}

/// Whether a `typeRoots` directory may hold a declaration for a specifier
/// (`holds_declaration`), which pack checks on disk without reading it.
pub(super) fn type_root_holds(type_root: &Path, specifier: &str) -> bool {
    holds_declaration(type_root, &type_root_name(type_root, specifier))
}

/// Whether `directory`'s `node_modules/@types` may hold a declaration for a
/// specifier's whole path: one the path names, scope mangled
/// (`holds_declaration`), or, for a subpath, a package whose `package.json`
/// holds `typesVersions`, which may map it to another file.
fn holds_types(directory: &Path, specifier: &str) -> bool {
    let mangled = mangle_scope(specifier);
    let (package, subpath) = split_package_name(&mangled);
    let types = directory.join("node_modules/@types");
    holds_declaration(&types, &mangled)
        || (!subpath.is_empty()
            && package_json(&types.join(package).join("package.json"))
                .is_some_and(|manifest| manifest.get("typesVersions").is_some()))
}

/// Whether TypeScript's lookup of a package in one `node_modules` directory
/// on disk finds a file with a set of extensions, as tsc 5.9.3, 6.0.3 and
/// 7.0.2 look one up (`loadModuleFromSpecificNodeModulesDirectory`): where
/// a name is given an extension, as all but an ES import's is, the file the
/// package's name names beside its directory (`node_modules/x.d.ts`), then
/// what the package holds (`package_module`). `None` where the package's
/// `exports`, which the lookup reads, or the `typesVersions` a subpath goes
/// through decides, but that a package with `exports` that is a link to
/// `looked_up`, a workspace package the caller walked with these
/// extensions and found nothing in, finds nothing here either.
fn finds_package(
    modules: &Path,
    specifier: &str,
    extensions: Extensions,
    exports: bool,
    add: bool,
    looked_up: Option<&Path>,
) -> Option<bool> {
    let (package, subpath) = split_package_name(specifier);
    let package = split_components(package);
    let directory = project_file(modules, &package);
    let root = package_json(&directory.join("package.json"));
    if exports
        && root
            .as_ref()
            .is_some_and(|root| root.get("exports").is_some())
    {
        let linked = looked_up
            .is_some_and(|looked_up| directory.canonicalize().is_ok_and(|real| real == looked_up));
        return linked.then_some(false);
    }
    let on_disk = |file: &[String]| project_file(modules, file).is_file().then(Vec::new);
    if add && subpath.is_empty() && module_file(&package, extensions, add, &on_disk).is_some() {
        return Some(true);
    }
    let found = package_module(
        modules,
        &package,
        root.as_ref(),
        subpath,
        extensions,
        add,
        &on_disk,
    );
    match found {
        Some(Hit::Files(_)) => Some(true),
        Some(Hit::Open) => None,
        None => Some(false),
    }
}

/// The file TypeScript's lookup of a package in a `node_modules` directory
/// takes below the package's directory, its `exports` not read, as tsc 5.9.3,
/// 6.0.3 and 7.0.2 look one up (`loadModuleFromSpecificNodeModulesDirectory`),
/// `package` being that directory's path below `base`, `root` its
/// `package.json` and `add` whether a name is given an extension: for its name,
/// the directory by that `package.json` (`directory_module`), whose `index` an
/// ES import reads only where the `package.json` exists, as tsc's `index.js`
/// fallback does; for a subpath, the file it names, then the directory by its
/// own `package.json`, whose `index` an ES import does not read. Open where the
/// `typesVersions` a subpath goes through decides. `existing` says what a path
/// below `base` names.
pub(super) fn package_module(
    base: &Path,
    package: &[String],
    root: Option<&serde_json::Value>,
    subpath: &str,
    extensions: Extensions,
    add: bool,
    existing: Existing,
) -> Option<Hit> {
    if subpath.is_empty() {
        let index = add || project_file(base, package).join("package.json").is_file();
        return directory_module(package, root, extensions, index, existing);
    }
    if root.is_some_and(|root| root.get("typesVersions").is_some()) {
        return Some(Hit::Open);
    }
    let path = project_path(package, subpath)?;
    let manifest = package_json(&project_file(base, &path).join("package.json"));
    module_file(&path, extensions, add, existing)
        .map(Hit::Files)
        .or_else(|| directory_module(&path, manifest.as_ref(), extensions, add, existing))
}

/// Whether TypeScript's lookup in one directory's `node_modules` on disk
/// finds a file for a non-relative specifier with a set of extensions, as
/// tsc 5.9.3, 6.0.3 and 7.0.2 look before a `typeRoots` directory: its
/// package (`finds_package`), then, for declarations, its `@types` one.
/// With every extension, as `bundler` and `node16` look, tsc looks for
/// TypeScript's and then for JavaScript's, so a package's `main` is read
/// for JavaScript where its `types` names no file. `add` is whether a name
/// is given an extension, `exports` whether a package's `exports` is read.
/// `None` where a `package.json` pack does not follow decides: `exports`
/// the lookup reads, or `typesVersions`. A package that is a link to
/// `looked_up` finds nothing (`finds_package`).
pub(super) fn node_modules_in(
    directory: &Path,
    specifier: &str,
    extensions: Extensions,
    exports: bool,
    add: bool,
    looked_up: Option<&Path>,
) -> Option<bool> {
    let modules = directory.join("node_modules");
    if !modules.is_dir() {
        return Some(false);
    }
    let passes = match extensions {
        Extensions::All => &[Extensions::TypeScript, Extensions::JavaScript][..],
        _ => std::slice::from_ref(&extensions),
    };
    let mut found: Vec<_> = passes
        .iter()
        .map(|pass| finds_package(&modules, specifier, *pass, exports, add, looked_up))
        .collect();
    if extensions != Extensions::JavaScript {
        let (types, mangled) = (modules.join("@types"), mangle_scope(specifier));
        let declaration = Extensions::Declaration;
        found.push(finds_package(
            &types,
            &mangled,
            declaration,
            exports,
            add,
            None,
        ));
    }
    either(&found)
}

/// Whether one of several `node_modules` lookups finds a file: `None` where
/// none does and one is left open.
pub(super) fn either(found: &[Option<bool>]) -> Option<bool> {
    match found.contains(&Some(true)) {
        true => Some(true),
        false => (!found.contains(&None)).then_some(false),
    }
}

/// What a lookup of a path finds (`directory_module`,
/// `ProjectIndex::typescript_path`).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Hit {
    /// The files: the project's file where pack reads it, none where it is
    /// only on disk.
    Files(Vec<usize>),
    /// A `package.json` pack does not follow (`typesVersions`) decides it.
    Open,
}

/// What a path names where a lookup checks for it: the project's file where
/// pack reads it, none where it is only on disk; `None` where it names none.
pub(super) type Existing<'e> = &'e dyn Fn(&[String]) -> Option<Vec<usize>>;

/// The file TypeScript takes for a path with a set of extensions: the first
/// of the names `module_names` gives in the path's directory that
/// `existing` finds. `None` where none of them exists.
pub(super) fn module_file(
    path: &[String],
    extensions: Extensions,
    add: bool,
    existing: Existing,
) -> Option<Vec<usize>> {
    let (name, directory) = path.split_last()?;
    module_names(name, extensions, add)
        .into_iter()
        .find_map(|candidate| existing(&[directory, &[candidate]].concat()))
}

/// The file TypeScript takes for a path a `package.json` field or an
/// `exports` target names with a set of extensions, as tsc 5.9.3, 6.0.3 and
/// 7.0.2 read one (`loadFileNameFromPackageJsonField`): where it writes a
/// TypeScript extension (`.ts`, `.tsx`, `.mts`, `.cts`, a declaration's
/// among them) and the extensions hold TypeScript's, the file as written,
/// none where it is missing; else the path with the extensions in place of
/// the one it writes, none added (`module_names`), so a path writing none
/// names no file, and a declaration read for declarations alone names
/// itself, as tsc tries it as written.
pub(super) fn field_file(
    path: &[String],
    extensions: Extensions,
    existing: Existing,
) -> Option<Vec<usize>> {
    let name = path.last()?;
    let typescript = matches!(extensions, Extensions::TypeScript | Extensions::All)
        && [".ts", ".tsx", ".mts", ".cts"]
            .iter()
            .any(|written| has_extension(name, written));
    match typescript {
        true => existing(path),
        false => module_file(path, extensions, false, existing),
    }
}

/// The file TypeScript takes for a directory a path names, by the
/// `package.json` it holds, as tsc 5.9.3, 6.0.3 and 7.0.2 trace it: where that
/// names one, the file the first of `typings` and `types`, where the extensions
/// hold declarations, and `main` that is set names (`field_file`), so a
/// declaration as written before its TypeScript twin, `lib/m.js` as `lib/m.ts`
/// and never as written, and a declaration lookup's `x.ts` as `x.d.ts`; else
/// that path with an extension added and, for declarations alone, as a
/// TypeScript file too (`named_extensions`), or that directory's `index`; else,
/// a field naming no file included, its own `index`. A `typesVersions` there,
/// which pack does not follow, leaves it open, since it may map the directory
/// to another file. Its `exports` is not read. The `index` is looked up with an
/// extension added where `add`, which an ES import under `node16` does not.
pub(super) fn directory_module(
    directory: &[String],
    manifest: Option<&serde_json::Value>,
    extensions: Extensions,
    add: bool,
    existing: Existing,
) -> Option<Hit> {
    let index = |directory: &[String], extensions, add| {
        let index = [directory, &["index".to_string()]].concat();
        module_file(&index, extensions, add, existing)
    };
    if let Some(manifest) = manifest {
        if manifest.get("typesVersions").is_some() {
            return Some(Hit::Open);
        }
        let declarations = extensions != Extensions::JavaScript;
        let named = ["typings", "types"]
            .into_iter()
            .filter(|_| declarations)
            .chain(["main"])
            .find_map(|field| {
                manifest
                    .get(field)?
                    .as_str()
                    .filter(|path| !path.is_empty())
            });
        let found = named.and_then(|named| {
            let path = project_path(directory, named)?;
            let expanded = named_extensions(extensions);
            field_file(&path, extensions, existing)
                .or_else(|| module_file(&path, expanded, true, existing))
                .or_else(|| index(&path, expanded, true))
        });
        if let Some(files) = found {
            return Some(Hit::Files(files));
        }
    }
    index(directory, extensions, add).map(Hit::Files)
}

/// The extensions TypeScript looks the file a `package.json` field names
/// up with: a lookup for declarations alone takes a TypeScript file there
/// too.
fn named_extensions(extensions: Extensions) -> Extensions {
    match extensions {
        Extensions::Declaration => Extensions::TypeScript,
        other => other,
    }
}

/// Whether the path a specifier names below `base` may hold the declaration
/// `classic` resolution takes for it there, as tsc 5.9.3 and 6.0.3 look one
/// up: a declaration the path names (`module_names` of declarations), else the directory it names holding an `index.d.ts`, or a
/// `package.json` naming `types`, `typings`, `main` or `typesVersions`
/// (`declares_types`), whose file pack does not look up. A directory holding
/// neither, or a `package.json` naming none of them, holds none: tsc takes
/// the JavaScript file there.
fn holds_declaration(base: &Path, specifier: &str) -> bool {
    let candidate = base.join(specifier);
    let named = candidate
        .file_name()
        .and_then(|name| name.to_str())
        .zip(candidate.parent());
    let Some((name, parent)) = named else {
        return false;
    };
    module_names(name, Extensions::Declaration, true)
        .iter()
        .any(|declaration| parent.join(declaration).is_file())
        || candidate.join("index.d.ts").is_file()
        || package_json(&candidate.join("package.json"))
            .is_some_and(|manifest| declares_types(&manifest))
}

/// Whether a `package.json` names a field TypeScript reads a declaration
/// for its directory by: `typings`, `types`, `main` or `typesVersions`.
fn declares_types(manifest: &serde_json::Value) -> bool {
    ["typings", "types", "main", "typesVersions"]
        .iter()
        .any(|field| manifest.get(field).is_some())
}

/// The `package.json` at a path, `None` where it is missing or no JSON.
fn package_json(path: &Path) -> Option<serde_json::Value> {
    json(&std::fs::read_to_string(path).ok()?)
}

/// A `package.json`'s text read as JSON, a UTF-8 byte order mark it starts
/// with removed, as Node 26.12.1 and tsc 5.9.3, 6.0.3 and 7.0.2 read it.
fn json(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)).ok()
}

/// Whether a name ends with an extension and holds more than it.
fn has_extension(name: &str, extension: &str) -> bool {
    name.len() > extension.len() && name.ends_with(extension)
}

/// The extensions TypeScript looks a module up with in one pass of its
/// lookup (`module_names`): `classic` and `node10` look for TypeScript's
/// files, then JavaScript's, `bundler` and `node16` for both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Extensions {
    /// TypeScript's own files, then declarations.
    TypeScript,
    /// Declarations alone, as it looks in `@types` and `typeRoots`.
    Declaration,
    /// JavaScript files, `allowJs` set or not.
    JavaScript,
    /// TypeScript's files, declarations, then JavaScript's, as `bundler` and
    /// `node16` look in one pass.
    All,
}

/// The names TypeScript tries, in order, for a file a path names by `name`
/// with a set of extensions, as tsc 5.9.3, 6.0.3 and 7.0.2 trace it. A
/// name holding a `.` first has its extension replaced: one TypeScript
/// strips (`SCRIPT_EXTENSIONS`), else whatever follows its last `.`, gives
/// way to the set's extensions for it — for `.js`, `.ts`, `.d.ts`: `.ts`,
/// `.tsx`, `.d.ts`, then `.js`, `.jsx`; for `.jsx`, `.tsx`: `.tsx`, `.ts`,
/// `.d.ts`, then `.jsx`, `.js`; for `.mjs`, `.mts`, `.d.mts`: `.mts`,
/// `.d.mts`, then `.mjs`, and alike for `.cjs`; for `.json` and any other:
/// a declaration of it (`x.d.json.ts`, `x.d.css.ts`) and no JavaScript,
/// `resolveJsonModule` not read. Then, where `add` says the lookup adds
/// extensions, as all but `node16`'s for an ES import do, the set's are
/// added to the whole name (`util.js.ts`): `.ts`, `.tsx`, `.d.ts`, then
/// `.js`, `.jsx`. A directory's `index` is the caller's to try.
pub(super) fn module_names(name: &str, extensions: Extensions, add: bool) -> Vec<String> {
    let stripped = name.contains('.').then(|| {
        SCRIPT_EXTENSIONS
            .iter()
            .find(|extension| has_extension(name, extension))
            .map(|extension| &name[name.len() - extension.len()..])
            .or_else(|| name.rfind('.').map(|dot| &name[dot..]))
    });
    let mut names = Vec::new();
    if let Some(Some(extension)) = stripped {
        let stem = &name[..name.len() - extension.len()];
        let other = format!(".d{extension}.ts");
        let (typescript, declaration, javascript): (&[&str], &[&str], &[&str]) = match extension {
            ".js" | ".ts" | ".d.ts" => (&[".ts", ".tsx"], &[".d.ts"], &[".js", ".jsx"]),
            ".jsx" | ".tsx" => (&[".tsx", ".ts"], &[".d.ts"], &[".jsx", ".js"]),
            ".mjs" | ".mts" | ".d.mts" => (&[".mts"], &[".d.mts"], &[".mjs"]),
            ".cjs" | ".cts" | ".d.cts" => (&[".cts"], &[".d.cts"], &[".cjs"]),
            _ => (&[], &[other.as_str()], &[]),
        };
        let replaced: Vec<&str> = match extensions {
            Extensions::TypeScript => typescript.iter().chain(declaration).copied().collect(),
            Extensions::Declaration => declaration.to_vec(),
            Extensions::JavaScript => javascript.to_vec(),
            Extensions::All => typescript
                .iter()
                .chain(declaration)
                .chain(javascript)
                .copied()
                .collect(),
        };
        names.extend(
            replaced
                .iter()
                .map(|extension| format!("{stem}{extension}")),
        );
    }
    let added: &[&str] = match extensions {
        Extensions::TypeScript => &[".ts", ".tsx", ".d.ts"],
        Extensions::Declaration => &[".d.ts"],
        Extensions::JavaScript => &[".js", ".jsx"],
        Extensions::All => &[".ts", ".tsx", ".d.ts", ".js", ".jsx"],
    };
    if add {
        names.extend(added.iter().map(|extension| format!("{name}{extension}")));
    }
    names
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
        // One that is no JSON declares nothing, but is still the package
        // scope of the files below it, as tsc 5.9.3, 6.0.3 and 7.0.2 read it.
        let package_jsons: Vec<(Vec<String>, serde_json::Value)> =
            manifests_above(root, nodes, is_javascript, "package.json")
                .into_iter()
                .map(|(directory, text)| (directory, json(&text).unwrap_or_default()))
                .collect();
        let package_scopes = package_jsons
            .iter()
            .map(|(directory, manifest)| {
                let kind = match manifest.get("type").and_then(|kind| kind.as_str()) {
                    Some("module") => PackageType::Module,
                    Some("commonjs") => PackageType::CommonJs,
                    _ => PackageType::Unset,
                };
                let name = manifest.get("name").and_then(|name| name.as_str());
                let scope = PackageScope {
                    kind,
                    name: name.map(str::to_string),
                    exports: manifest.get("exports").cloned(),
                    imports: manifest.get("imports").cloned(),
                };
                (directory.clone(), scope)
            })
            .collect();
        let python = python_configurations(root, nodes);
        let workspaces = js_workspaces(root, nodes, &package_jsons);
        let mut versions = TypeScriptVersions::new(root);
        let ts_projects = ts_projects(root, nodes, &workspaces, &mut versions);
        let typescript = nodes
            .iter()
            .filter(|node| is_javascript(node.language))
            .map(|node| (node.directory.clone(), versions.of(&node.directory)))
            .collect();
        Self {
            root: Some(root.to_path_buf()),
            go_modules,
            dart_packages,
            rust_packages,
            python_projects: python.projects,
            pytest_configurations: python.pytest,
            pytest_rootdirs: python.rootdirs,
            ts_projects,
            js_packages: js_packages(&package_jsons, &workspaces),
            package_scopes,
            typescript,
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
    /// the nearest above it, or, where that one has `references`, the one
    /// the projects compiling it, of that one and those its references
    /// reach, agree on (`TsProjects::owners`). Projects that say different
    /// things about specifiers leave it undetermined; where none compiles
    /// it, no configuration applies to it.
    pub(super) fn ts_owner(&self, from: &Node) -> TsOwner<'_> {
        let projects = &self.ts_projects;
        let Some((_, project)) = nearest(&projects.by_directory, &from.directory) else {
            return TsOwner::None;
        };
        let owners = projects.owners(*project, Path::new(&from.rel_path));
        match owners.split_first() {
            None => TsOwner::Uncompiled(*project),
            Some((first, rest)) if rest.iter().all(|other| other == first) => {
                TsOwner::Config(first)
            }
            Some(_) => TsOwner::Undetermined(owners),
        }
    }

    /// What the projects compiling a file (`ts_owner`) say about a
    /// package's own name read from it (`SelfName`), each different one
    /// once: none where no configuration compiles it.
    pub(super) fn self_names(&self, from: &Node) -> Vec<&SelfName> {
        let projects = &self.ts_projects;
        let Some((_, project)) = nearest(&projects.by_directory, &from.directory) else {
            return Vec::new();
        };
        let mut found: Vec<&SelfName> = Vec::new();
        for at in projects.owning(*project, Path::new(&from.rel_path)) {
            let self_name = &projects.projects[at].self_name;
            if !found.contains(&self_name) {
                found.push(self_name);
            }
        }
        found
    }

    /// The configurations of the TypeScript project at `project` and of
    /// every project its references reach, each once.
    pub(super) fn reached(&self, project: usize) -> Vec<&TsConfig> {
        self.ts_projects.reached(project)
    }

    /// Whether the file a path names whole is on disk.
    pub(super) fn is_file_on_disk(&self, written: &[String]) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| project_file(root, written).is_file())
    }

    /// The `package.json` a project directory holds on disk, `None` where it
    /// holds none or one that is no JSON.
    pub(super) fn package_json_at(&self, directory: &[String]) -> Option<serde_json::Value> {
        let root = self.root.as_ref()?;
        package_json(&project_file(root, directory).join("package.json"))
    }

    /// Whether a directory above the project's root, up to the filesystem's,
    /// holds on disk the file a path names below it.
    pub(super) fn is_file_above_root(&self, written: &[String]) -> bool {
        self.root.as_ref().is_some_and(|root| {
            root.ancestors()
                .skip(1)
                .any(|ancestor| project_file(ancestor, written).is_file())
        })
    }

    /// The project path a path on disk lies at, `None` where it lies outside
    /// the project.
    pub(super) fn project_path_of(&self, path: &Path) -> Option<Vec<String>> {
        let relative = path.strip_prefix(self.root.as_ref()?).ok()?;
        relative
            .components()
            .map(|component| match component {
                std::path::Component::Normal(part) => part.to_str().map(str::to_string),
                _ => None,
            })
            .collect()
    }

    /// Whether a path is the `node_modules/@types` of `directory` or of a
    /// directory above it, up to the filesystem's root.
    pub(super) fn is_types_of_ancestor(&self, path: &Path, directory: &[String]) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let holder = path
            .ends_with("node_modules/@types")
            .then(|| path.parent().and_then(Path::parent))
            .flatten();
        holder.is_some_and(|holder| project_file(root, directory).starts_with(holder))
    }

    /// Whether a `node_modules/@types` in a directory above the project's
    /// root, up to the filesystem's, may hold a declaration for a
    /// specifier's whole path (`holds_types`), which pack checks on disk
    /// without reading it.
    pub(super) fn types_above_root(&self, specifier: &str) -> bool {
        self.root.as_ref().is_some_and(|root| {
            root.ancestors()
                .skip(1)
                .any(|directory| holds_types(directory, specifier))
        })
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
fn ts_projects(
    root: &Path,
    nodes: &[Node],
    workspaces: &[(Vec<String>, Vec<String>)],
    versions: &mut TypeScriptVersions,
) -> TsProjects {
    let mut workspaces = workspaces.to_vec();
    workspaces.sort_by_key(|(directory, _)| std::cmp::Reverse(directory.len()));
    let mut reader = TsReader {
        root,
        versions,
        workspaces: &workspaces,
        members: HashMap::new(),
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
    versions: &'r mut TypeScriptVersions,
    /// The project's JavaScript workspaces, the deepest first.
    workspaces: &'r [(Vec<String>, Vec<String>)],
    /// The packages each workspace lists, by its root, found once.
    members: HashMap<Vec<String>, Vec<(String, Vec<String>)>>,
    parsed: HashMap<Vec<String>, Option<serde_json::Value>>,
    indices: HashMap<Vec<String>, Option<usize>>,
    projects: TsProjects,
}

/// A configuration with each it extends, nearest first, and where in that
/// order the first one that could not be read would stand: what it sets is
/// unknown, and so is every option no configuration before it sets.
#[derive(Default)]
struct Chain {
    configs: Vec<(Vec<String>, serde_json::Value)>,
    unread: Option<usize>,
}

impl Chain {
    /// The value of an option the chain decides, with the directory of the
    /// configuration setting it: `Some(None)` where it sets none, `None`
    /// where a configuration it could not read may set it.
    fn decided(&self, key: &str) -> Option<Option<(&Vec<String>, &serde_json::Value)>> {
        let read = self.unread.unwrap_or(self.configs.len());
        let value = self.configs[..read].iter().find_map(|(directory, config)| {
            Some((directory, config.get("compilerOptions")?.get(key)?))
        });
        match value {
            Some(value) => Some(Some(value)),
            None => self.unread.is_none().then_some(None),
        }
    }
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
        let mut chain = Chain::default();
        self.config_chain(path, &mut chain);
        let Some((directory, head)) = chain.configs.first() else {
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
        let javascript = path.last().is_some_and(|file| file == "jsconfig.json");
        self.projects.projects.push(TsProject {
            config: ts_config(self.root, &chain, self.versions.of(directory), javascript),
            self_name: self_name(self.root, &chain, javascript),
            sources: ts_sources(&chain.configs),
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
    /// it extends, and of several it extends at once the later wins. A
    /// configuration that cannot be read — one its `extends` names no file
    /// for (`extended_path`), a file that is no JSON with comments, or a
    /// chain extending itself — marks where it would stand.
    fn config_chain(&mut self, path: &[String], chain: &mut Chain) {
        // A configuration extending itself through others would never end.
        const DEPTH: usize = 16;
        let config = (chain.configs.len() < DEPTH)
            .then(|| self.config(path).cloned())
            .flatten();
        let Some(config) = config else {
            chain.unread.get_or_insert(chain.configs.len());
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
        chain.configs.push((directory.clone(), config));
        for extended in extends.iter().rev() {
            match self.extended_path(&directory, extended) {
                Some(path) => self.config_chain(&path, chain),
                None => {
                    chain.unread.get_or_insert(chain.configs.len());
                }
            }
        }
    }

    /// The configuration file an `extends` written in a configuration at
    /// `directory` names, as TypeScript finds it: a relative path as written,
    /// then with `.json` added; any other name as a module specifier, looked
    /// up as Node looks up a package (`extended_package`). `None` where it
    /// names no file in the project — a rooted path among them.
    fn extended_path(&mut self, directory: &[String], extended: &str) -> Option<Vec<String>> {
        let extended = extended.replace('\\', "/");
        if extended.starts_with("./") || extended.starts_with("../") {
            let mut path = project_path(directory, &extended)?;
            if !extended.ends_with(".json")
                && !project_file(self.root, &path).is_file()
                && let Some(last) = path.last_mut()
            {
                last.push_str(".json");
            }
            return Some(path);
        }
        if extended.starts_with('/') || extended.is_empty() {
            return None;
        }
        self.extended_package(directory, &extended)
    }

    /// The configuration file a package specifier names for an `extends`,
    /// as TypeScript resolves one (its `nodeNextJsonConfigResolver`): the
    /// package is the first `node_modules/<name>` from the configuration's
    /// directory up to the project root, or, where none is installed, the
    /// workspace package of that name (`workspace_member`); in it, the file
    /// its `exports` maps the subpath to under the conditions `require`,
    /// `types`, `node` and `default`, and without `exports` the file the
    /// subpath names, as written or with `.json` added, or else the directory
    /// it names: the file the `package.json` `tsconfig` field names there,
    /// else its `tsconfig.json`. An installed package is read where its real
    /// path is, as TypeScript reads it.
    fn extended_package(&mut self, directory: &[String], specifier: &str) -> Option<Vec<String>> {
        let (name, subpath) = split_package_name(specifier);
        let installed = (0..=directory.len()).rev().find_map(|depth| {
            let at = &directory[..depth];
            if at.last().is_some_and(|last| last == "node_modules") {
                return None;
            }
            let package = [at, &["node_modules".to_string()], &split_components(name)].concat();
            project_file(self.root, &package)
                .is_dir()
                .then_some(package)
        });
        let package = match installed {
            Some(package) => package,
            None => self.workspace_member(directory, name)?,
        };
        let manifest = self.package_manifest(&package);
        let file = match manifest
            .as_ref()
            .and_then(|manifest| manifest.get("exports"))
        {
            Some(exports) => {
                let target = config_export(exports, subpath)?;
                let file = project_path(&package, &target)?;
                (target.ends_with(".json") && self.is_file(&file)).then_some(file)?
            }
            None => {
                let candidate = [package.as_slice(), &split_components(subpath)].concat();
                let manifest = self.package_manifest(&candidate).or(manifest);
                let field = manifest
                    .as_ref()
                    .and_then(|manifest| manifest.get("tsconfig")?.as_str())
                    .and_then(|field| project_path(&candidate, field));
                self.json_file(&candidate)
                    .or_else(|| {
                        let field = field?;
                        self.json_file(&field).or_else(|| self.tsconfig_in(&field))
                    })
                    .or_else(|| self.tsconfig_in(&candidate))?
            }
        };
        self.real_path(&file)
    }

    /// The `package.json` in the directory at `directory`, where it holds
    /// one that is JSON.
    fn package_manifest(&self, directory: &[String]) -> Option<serde_json::Value> {
        package_json(&project_file(self.root, directory).join("package.json"))
    }

    fn is_file(&self, path: &[String]) -> bool {
        project_file(self.root, path).is_file()
    }

    /// The configuration file a path names as a file: itself where it ends
    /// in `.json`, else with `.json` added.
    fn json_file(&self, path: &[String]) -> Option<Vec<String>> {
        let last = path.last()?;
        if last.ends_with(".json") && self.is_file(path) {
            return Some(path.to_vec());
        }
        let mut with_json = path.to_vec();
        with_json.last_mut()?.push_str(".json");
        self.is_file(&with_json).then_some(with_json)
    }

    /// The `tsconfig.json` in the directory a path names.
    fn tsconfig_in(&self, directory: &[String]) -> Option<Vec<String>> {
        let file = [directory, &["tsconfig.json".to_string()]].concat();
        self.is_file(&file).then_some(file)
    }

    /// The project path of a file where its real path, symbolic links
    /// resolved, lies below the project root.
    fn real_path(&self, path: &[String]) -> Option<Vec<String>> {
        let real = std::fs::canonicalize(project_file(self.root, path)).ok()?;
        let root = std::fs::canonicalize(self.root).ok()?;
        let below = real.strip_prefix(root).ok()?;
        below
            .components()
            .map(|component| component.as_os_str().to_str().map(str::to_string))
            .collect()
    }

    /// The directory of the package named `name` that a workspace above
    /// `directory` lists, the nearest workspace listing one deciding, as a
    /// package manager links it into `node_modules`; `None` where none, or
    /// several of the nearest, carry the name.
    fn workspace_member(&mut self, directory: &[String], name: &str) -> Option<Vec<String>> {
        for (workspace, globs) in self.workspaces {
            if !directory.starts_with(workspace) {
                continue;
            }
            let root = self.root;
            let members = self
                .members
                .entry(workspace.clone())
                .or_insert_with(|| workspace_members(root, workspace, globs));
            let mut named = members.iter().filter(|(member, _)| member == name);
            match (named.next(), named.next()) {
                (Some((_, member)), None) => return Some(member.clone()),
                (Some(_), Some(_)) => return None,
                (None, _) => {}
            }
        }
        None
    }
}

/// A package specifier's package name — its first segment, or its first
/// two where it is scoped (`@scope/name`) — and the subpath after it.
pub(super) fn split_package_name(specifier: &str) -> (&str, &str) {
    let segments = if specifier.starts_with('@') { 2 } else { 1 };
    let end = specifier
        .match_indices('/')
        .nth(segments - 1)
        .map_or(specifier.len(), |(at, _)| at);
    (
        &specifier[..end],
        specifier.get(end + 1..).unwrap_or_default(),
    )
}

/// The target a package's `exports` maps a subpath of the package to, as
/// TypeScript and Node look it up: the entry for the key `.` or
/// `./<subpath>` (`mapped_key`), with what a pattern key's `*` matched,
/// which replaces each `*` its target's paths hold. Without subpath keys
/// `exports` is what `.` maps to. The caller reads the target's conditions.
pub(super) fn export_subpath<'e>(
    exports: &'e serde_json::Value,
    subpath: &str,
) -> Option<(&'e serde_json::Value, Option<String>)> {
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    match exports {
        serde_json::Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => {
            mapped_key(map, &key)
        }
        _ if key == "." => Some((exports, None)),
        _ => None,
    }
}

/// The target a map of `exports` subpaths or `imports` specifiers holds
/// for a key: its exact entry, else the pattern key with one `*` matching
/// it whose part before the `*` is longest, the longer key on a tie, with
/// what its `*` matched.
pub(super) fn mapped_key<'e>(
    map: &'e serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<(&'e serde_json::Value, Option<String>)> {
    match map.get(key) {
        Some(target) => Some((target, None)),
        None => map
            .iter()
            .filter_map(|(pattern, target)| {
                let (prefix, suffix) = pattern.split_once('*')?;
                if suffix.contains('*') || key.len() < prefix.len() + suffix.len() {
                    return None;
                }
                let star = key.strip_prefix(prefix)?.strip_suffix(suffix)?;
                Some(((prefix.len(), pattern.len()), target, star))
            })
            .max_by_key(|(rank, ..)| *rank)
            .map(|(_, target, star)| (target, Some(star.to_string()))),
    }
}

/// The path a target of `exports` names, `*` replaced by what a pattern
/// key's `*` matched (`export_subpath`); `None` where it does not start
/// `./`.
pub(super) fn export_path(path: &str, star: Option<&str>) -> Option<String> {
    let path = path.strip_prefix("./")?;
    Some(match star {
        Some(star) => path.replace('*', star),
        None => path.to_string(),
    })
}

/// The target a package's `exports` maps an `extends` subpath to
/// (`export_subpath`), as TypeScript reads it for a configuration: of a
/// condition object, the first key in written order among `require`,
/// `types`, `node` and `default` whose target resolves; of an array, the
/// first entry that resolves. `None` where none does, a `null` target
/// excludes the subpath, or a `types@<range>` key, which depends on the
/// compiler's version, comes first.
fn config_export(exports: &serde_json::Value, subpath: &str) -> Option<String> {
    let (target, star) = export_subpath(exports, subpath)?;
    let star = star.as_deref();
    fn resolve(target: &serde_json::Value, star: Option<&str>) -> Result<Option<String>, ()> {
        match target {
            serde_json::Value::String(path) => Ok(export_path(path, star)),
            serde_json::Value::Array(entries) => {
                for entry in entries {
                    if let Some(found) = resolve(entry, star)? {
                        return Ok(Some(found));
                    }
                }
                Ok(None)
            }
            serde_json::Value::Object(conditions) => {
                for (condition, target) in conditions {
                    if condition.starts_with("types@") {
                        return Err(());
                    }
                    if matches!(condition.as_str(), "require" | "types" | "node" | "default")
                        && let Some(found) = resolve(target, star)?
                    {
                        return Ok(Some(found));
                    }
                }
                Ok(None)
            }
            serde_json::Value::Null => Err(()),
            _ => Ok(None),
        }
    }
    resolve(target, star).ok().flatten()
}

/// The packages a JavaScript workspace rooted at `workspace` lists, found on
/// disk as a package manager finds them: each directory below it whose path
/// one of `globs` matches and none of its `!` globs does, holding a
/// `package.json` with a `name` — `node_modules` and hidden directories
/// never among them.
fn workspace_members(
    root: &Path,
    workspace: &[String],
    globs: &[String],
) -> Vec<(String, Vec<String>)> {
    let Some(listed) = WorkspaceGlobs::new(globs) else {
        return Vec::new();
    };
    let deepest = globs
        .iter()
        .map(|glob| {
            if glob.contains("**") {
                usize::MAX
            } else {
                glob.trim_start_matches("./")
                    .trim_end_matches('/')
                    .split('/')
                    .count()
            }
        })
        .max()
        .unwrap_or_default();
    let mut members = Vec::new();
    let mut pending = vec![Vec::<String>::new()];
    while let Some(below) = pending.pop() {
        let directory = [workspace, below.as_slice()].concat();
        let Ok(entries) = std::fs::read_dir(project_file(root, &directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name == "node_modules"
                || name.starts_with('.')
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let mut member = below.clone();
            member.push(name);
            if listed.lists(&member.join("/")) {
                let path = [workspace, member.as_slice()].concat();
                let manifest = package_json(&project_file(root, &path).join("package.json"));
                if let Some(name) = manifest
                    .as_ref()
                    .and_then(|manifest| manifest.get("name")?.as_str())
                {
                    members.push((name.to_string(), path));
                }
            }
            if member.len() < deepest {
                pending.push(member);
            }
        }
    }
    members
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
                .filter_map(|path| {
                    let (directory, path) = option_path(head, directory, path.as_str()?);
                    project_path(directory, &path)
                })
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

/// Each JavaScript workspace root in the project, with the globs listing
/// its packages: a `package.json` of `manifests` with `workspaces`, or a
/// `pnpm-workspace.yaml`.
fn js_workspaces(
    root: &Path,
    nodes: &[Node],
    manifests: &[(Vec<String>, serde_json::Value)],
) -> Vec<(Vec<String>, Vec<String>)> {
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
    workspaces
}

/// What a workspace's globs list: a member path relative to its root that
/// one of its globs matches and none of its `!` globs does.
struct WorkspaceGlobs {
    included: globset::GlobSet,
    excluded: Option<globset::GlobSet>,
}

impl WorkspaceGlobs {
    fn new(globs: &[String]) -> Option<Self> {
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
        let included = matcher(
            &mut globs
                .iter()
                .filter(|glob| !glob.starts_with('!'))
                .map(String::as_str),
        )?;
        let excluded = matcher(&mut globs.iter().filter_map(|glob| glob.strip_prefix('!')));
        Some(Self { included, excluded })
    }

    fn lists(&self, member: &str) -> bool {
        !member.is_empty()
            && self.included.is_match(member)
            && !self
                .excluded
                .as_ref()
                .is_some_and(|excluded| excluded.is_match(member))
    }
}

/// Every package a JavaScript workspace in the project lists: each
/// `package.json` of `manifests` with a name below a workspace root whose
/// globs list its directory (`WorkspaceGlobs`).
fn js_packages(
    manifests: &[(Vec<String>, serde_json::Value)],
    workspaces: &[(Vec<String>, Vec<String>)],
) -> HashMap<Vec<String>, JsPackage> {
    let mut packages = HashMap::new();
    for (workspace, globs) in workspaces {
        let Some(listed) = WorkspaceGlobs::new(globs) else {
            continue;
        };
        for (directory, manifest) in manifests {
            let Some(member) = directory.strip_prefix(workspace.as_slice()) else {
                continue;
            };
            if !listed.lists(&member.join("/")) {
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
                    main: field("main"),
                    module: field("module"),
                    browser: manifest.get("browser").cloned(),
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
/// says, with what it extends, for the TypeScript versions that may
/// compile it.
fn ts_config(root: &Path, chain: &Chain, typescript: TypeScript, javascript: bool) -> TsConfig {
    let configs = chain.configs.as_slice();
    let directory = configs
        .first()
        .map(|(directory, _)| directory.as_slice())
        .unwrap_or_default();
    let option = |key: &str| {
        configs.iter().find_map(|(directory, config)| {
            let value = config.get("compilerOptions")?.get(key)?;
            Some((directory, value))
        })
    };
    let decided = |key: &str| chain.decided(key).map(|set| set.map(|(_, value)| value));
    let setting = |key: &str| match decided(key) {
        Some(Some(value)) => value.as_str().map_or(Setting::Unknown, |value| {
            Setting::Set(value.to_ascii_lowercase())
        }),
        Some(None) => Setting::Unset,
        None => Setting::Unknown,
    };
    let base_url = option("baseUrl").and_then(|(at, value)| {
        let (at, written) = option_path(directory, at, value.as_str()?);
        project_path(at, &written)
    });
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
                .filter_map(|substitution| substitution.as_str())
                .map(|substitution| match below_config_dir(substitution) {
                    Some(below) => (Some(directory.to_vec()), below),
                    None => (None, substitution.to_string()),
                })
                .collect();
            (pattern.clone(), substitutions)
        })
        .collect();
    let paths_directory = declared
        .map(|(directory, _)| directory.clone())
        .unwrap_or_else(|| directory.to_vec());
    let paths_base = base_url.clone().unwrap_or_else(|| paths_directory.clone());
    let module_options = ModuleOptions {
        module: setting("module"),
        module_resolution: setting("moduleResolution"),
        target: setting("target"),
    };
    let type_roots = option("typeRoots")
        .and_then(|(at, roots)| Some((at, roots.as_array()?)))
        .into_iter()
        .flat_map(|(at, roots)| {
            roots.iter().filter_map(move |written| {
                let (at, written) = option_path(directory, at, written.as_str()?);
                Some(configured_path(root, at, &written))
            })
        })
        .collect();
    let custom_conditions = decided("customConditions").map(|conditions| {
        conditions
            .and_then(|conditions| conditions.as_array())
            .into_iter()
            .flatten()
            .filter_map(|condition| condition.as_str().map(str::to_string))
            .collect()
    });
    TsConfig {
        base_url,
        paths,
        paths_base,
        paths_directory,
        module_options,
        typescript,
        custom_conditions,
        javascript,
        type_roots,
    }
}

/// The options a configuration chain has tsc read a package's own name
/// from inside it with (`SelfName`): a path option against the directory of
/// the configuration setting it, and `allowJs` set by default in a
/// `jsconfig.json`, as tsc's `getDefaultCompilerOptions` sets it.
fn self_name(root: &Path, chain: &Chain, javascript: bool) -> SelfName {
    let head = chain.configs.first().map(|(head, _)| head.as_slice());
    let path = |key: &str| {
        chain.decided(key).map(|set| {
            let (directory, written) = set?;
            let written = written.as_str().filter(|written| !written.is_empty())?;
            let (directory, written) = option_path(head.unwrap_or_default(), directory, written);
            Some(configured_path(root, directory, &written))
        })
    };
    let flag = |key: &str| {
        chain
            .decided(key)
            .map(|set| set.is_some_and(|(_, value)| value.as_bool() == Some(true)))
    };
    let allow_js = match chain.decided("allowJs") {
        Some(Some((_, value))) => Some(value.as_bool() == Some(true)),
        Some(None) if javascript => Some(true),
        Some(None) => flag("checkJs"),
        None => None,
    };
    SelfName {
        directory: head.unwrap_or_default().to_vec(),
        declaration_dir: path("declarationDir"),
        out_dir: path("outDir"),
        root_dir: path("rootDir"),
        composite: flag("composite"),
        allow_js,
    }
}

/// The directory a path option written in the configuration at `directory`
/// resolves against, with the path it names there: `head`, the directory of
/// the configuration its chain starts from, where it starts with
/// `${configDir}` (`below_config_dir`), else `directory`.
fn option_path<'p>(
    head: &'p [String],
    directory: &'p [String],
    written: &str,
) -> (&'p [String], String) {
    match below_config_dir(written) {
        Some(below) => (head, below),
        None => (directory, written.to_string()),
    }
}

/// What a path option starting with `${configDir}` names below the directory of
/// the configuration its chain starts from: the option with that template
/// replaced by `./`, as tsc 5.9.3, 6.0.3 and 7.0.2 substitute it in `baseUrl`,
/// `paths`, `typeRoots`, `rootDir`, `outDir`, `declarationDir`, `files`,
/// `include` and `exclude`, wherever in the chain it is set
/// (`getSubstitutedPathWithConfigDirTemplate`). `None` for any other path.
fn below_config_dir(written: &str) -> Option<String> {
    let below = written.strip_prefix("${configDir}")?;
    Some(format!("./{below}"))
}

/// A path a configuration writes, against the directory holding it, as
/// TypeScript normalizes one: `.` and `..` read by name, never through a
/// link, so it may name a directory above the project's root.
fn configured_path(root: &Path, directory: &[String], written: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for component in project_file(root, directory).join(written).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                path.pop();
            }
            component => path.push(component),
        }
    }
    path
}

/// The TypeScript versions each directory's files may be compiled by, each
/// read once: the version of the `typescript` package installed in the
/// nearest `node_modules` from the directory up to the project root, as
/// Node finds it (a link to a store, as pnpm makes, followed), else the
/// range the nearest `package.json` declaring `typescript` gives
/// (`declared_typescript`), else none.
struct TypeScriptVersions {
    root: PathBuf,
    installed: HashMap<Vec<String>, Option<Option<u64>>>,
    declared: HashMap<Vec<String>, Option<String>>,
}

impl TypeScriptVersions {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            installed: HashMap::new(),
            declared: HashMap::new(),
        }
    }

    fn of(&mut self, directory: &[String]) -> TypeScript {
        if let Some(major) = self.installed(directory) {
            return TypeScript::of_major(major, None);
        }
        match self.declared(directory) {
            Some(range) => TypeScript::of_major(typescript_major(&range), Some(range)),
            None => TypeScript::Absent,
        }
    }

    /// The major of the `typescript` package installed nearest above
    /// `directory`; `Some(None)` where its version is no number.
    fn installed(&mut self, directory: &[String]) -> Option<Option<u64>> {
        if let Some(known) = self.installed.get(directory) {
            return *known;
        }
        let here = (directory.last().is_none_or(|last| last != "node_modules"))
            .then(|| {
                let manifest = project_file(&self.root, directory)
                    .join("node_modules/typescript/package.json");
                let manifest = package_json(&manifest)?;
                let version = manifest.get("version")?.as_str()?;
                Some(version.split('.').next()?.parse::<u64>().ok())
            })
            .flatten();
        let found = match (here, directory.split_last()) {
            (Some(found), _) => Some(found),
            (None, Some((_, parent))) => self.installed(parent),
            (None, None) => None,
        };
        self.installed.insert(directory.to_vec(), found);
        found
    }

    /// The `typescript` range declared nearest above `directory`
    /// (`declared_typescript`).
    fn declared(&mut self, directory: &[String]) -> Option<String> {
        if let Some(known) = self.declared.get(directory) {
            return known.clone();
        }
        let found = match (
            declared_typescript(&self.root, directory),
            directory.split_last(),
        ) {
            (Some(found), _) => Some(found),
            (None, Some((_, parent))) => self.declared(parent),
            (None, None) => None,
        };
        self.declared.insert(directory.to_vec(), found.clone());
        found
    }
}

/// The `typescript` range the `package.json` at `directory` declares, in
/// its `dependencies`, `devDependencies`, `optionalDependencies` or
/// `peerDependencies`, the first holding it: a pnpm `catalog:` or
/// `catalog:<name>` reference read from the nearest `pnpm-workspace.yaml`'s
/// `catalog` or `catalogs` (`pnpm_catalog`), and an `npm:` alias
/// (`npm:@typescript/typescript6@^6.0.2`) read as the range it gives. `None`
/// where it declares none; an empty string where its catalog holds none.
fn declared_typescript(root: &Path, directory: &[String]) -> Option<String> {
    let manifest = package_json(&project_file(root, directory).join("package.json"))?;
    let range = [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .find_map(|field| manifest.get(field)?.get("typescript")?.as_str())?;
    let range = match range.strip_prefix("catalog:") {
        Some(catalog) => {
            let catalog = if catalog.is_empty() {
                "default"
            } else {
                catalog
            };
            (0..=directory.len())
                .rev()
                .find_map(|depth| {
                    let yaml = std::fs::read_to_string(
                        project_file(root, &directory[..depth]).join("pnpm-workspace.yaml"),
                    )
                    .ok()?;
                    Some(pnpm_catalog(&yaml, catalog, "typescript").unwrap_or_default())
                })
                .unwrap_or_default()
        }
        None => range.to_string(),
    };
    Some(match range.strip_prefix("npm:") {
        Some(alias) => alias
            .rsplit_once('@')
            .map(|(_, range)| range.to_string())
            .unwrap_or_default(),
        None => range,
    })
}

/// The major pack reads from a version range: that of a caret, tilde or
/// exact version, an x-range (`^5.9.0`, `~6.0`, `6.x`, `5`), a set of
/// comparators whose bounds lie within one major (`>=5.0.0 <6.0.0`, `>5
/// <7`) or a hyphen range whose ends do (`5.0.0 - 5.9.9`), as node-semver
/// reads them (`alternative_bounds`), each of the alternatives `||` joins
/// agreeing. `None` for any other range: an open one (`>=5`, `*`), one
/// spanning majors (`>=5 <7`, `5 - 6`), a tag (`latest`).
fn typescript_major(range: &str) -> Option<u64> {
    let mut majors = range.split("||").map(alternative_major);
    let first = majors.next()??;
    majors.all(|major| major == Some(first)).then_some(first)
}

/// The major pack reads from one alternative of a range
/// (`typescript_major`): the greatest major its comparators' lower bounds
/// name, where it is the least their upper bounds allow
/// (`alternative_bounds`).
fn alternative_major(alternative: &str) -> Option<u64> {
    match alternative_bounds(alternative)? {
        (Some(low), Some(high)) if low == high => Some(low),
        _ => None,
    }
}

/// Whether a declared range admits a major below 7: an alternative of it
/// whose comparators set no lower bound or one below 7
/// (`alternative_bounds`), or one pack reads no bound from (`latest`, `*`).
fn range_admits_below_7(range: &str) -> bool {
    range.split("||").any(|alternative| {
        alternative_bounds(alternative)
            .is_none_or(|(lowest, _)| lowest.is_none_or(|lowest| lowest < 7))
    })
}

/// The bounds one alternative of a range sets on the major, as node-semver
/// 7.8.5 expands it: the greatest major its comparators' lower bounds name
/// and the least their upper bounds allow, each `None` where none sets it.
/// A `>` bound on a version naming no minor starts at the next major (`>6`
/// as `>=7.0.0`), and one naming a minor stays in its major (`>6.2` as
/// `>=6.3.0`, `>6.0.0`). A hyphen range is bounded by the majors its two
/// ends name: node-semver reads the left end as `>=` it, what it leaves out
/// as zeros, and the right end as `<=` it, or where it leaves a part out as
/// below the next release of what it names (`5.0.0 - 5` as `>=5.0.0
/// <6.0.0-0`). `None` where the alternative is no set of comparators or
/// hyphen range of versions pack reads (`latest`, `*`, `* - 5`). An
/// operator may stand apart from its version (`>= 5.0.0`).
fn alternative_bounds(alternative: &str) -> Option<(Option<u64>, Option<u64>)> {
    if let [from, "-", to] = alternative.split_whitespace().collect::<Vec<_>>()[..] {
        let major = |end: &str| {
            let major = end.trim_start_matches(['v', '=']).split('.').next()?;
            major.parse::<u64>().ok()
        };
        return Some((Some(major(from)?), Some(major(to)?)));
    }
    let operator = |c: char| matches!(c, '<' | '>' | '=' | '^' | '~');
    let mut lowest: Option<u64> = None;
    let mut highest: Option<u64> = None;
    let mut pending = String::new();
    for token in alternative.split_whitespace() {
        if token.chars().all(operator) {
            pending.push_str(token);
            continue;
        }
        let comparator = std::mem::take(&mut pending) + token;
        let version_at = comparator.find(|c: char| !operator(c))?;
        let (op, version) = comparator.split_at(version_at);
        let version = version.strip_prefix('v').unwrap_or(version);
        let release = version
            .split_once('-')
            .map_or(version, |(release, _)| release);
        let mut parts = release.split('.');
        let major = parts.next()?.parse::<u64>().ok()?;
        let rest: Vec<&str> = parts.collect();
        // Whether it is at or below its major's first release (`6`,
        // `6.0.0`, `6.0.0-0`), below which a `<` bound ends with the major
        // before it.
        let first = rest
            .iter()
            .all(|part| matches!(*part, "0" | "x" | "X" | "*"));
        // Whether it names no minor (`6`, `6.x`), past which a `>` bound
        // starts with the next major.
        let bare = rest
            .first()
            .is_none_or(|minor| matches!(*minor, "x" | "X" | "*"));
        let (low, high) = match op {
            "" | "=" | "^" | "~" => (Some(major), Some(major)),
            ">" if bare => (Some(major.checked_add(1)?), None),
            ">=" | ">" => (Some(major), None),
            "<=" => (None, Some(major)),
            "<" if first => (None, Some(major.checked_sub(1)?)),
            "<" => (None, Some(major)),
            _ => return None,
        };
        lowest = lowest.max(low);
        highest = match (highest, high) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    Some((lowest, highest))
}

/// The value a `pnpm-workspace.yaml` catalog gives a package: the default
/// catalog's under the top-level `catalog` (or `catalogs.default`), a named
/// one's under `catalogs.<name>`, read as block mappings of plain or quoted
/// keys and values.
fn pnpm_catalog(yaml: &str, catalog: &str, package: &str) -> Option<String> {
    let entries: Vec<(usize, String, String)> = yaml
        .lines()
        .filter_map(|line| {
            let content = line.trim_start();
            if content.is_empty() || content.starts_with('#') {
                return None;
            }
            let indent = line.len() - content.len();
            let (key, value) = match content.strip_prefix(['"', '\'']) {
                Some(rest) => {
                    let quote = content.chars().next()?;
                    let (key, rest) = rest.split_once(quote)?;
                    (key.to_string(), rest.trim_start().strip_prefix(':')?)
                }
                None => {
                    let (key, rest) = content.split_once(':')?;
                    (key.trim().to_string(), rest)
                }
            };
            let value = match value.find(" #") {
                Some(comment) => &value[..comment],
                None => value,
            };
            let value = value.trim().trim_matches(['"', '\'']).to_string();
            Some((indent, key, value))
        })
        .collect();
    let lookup = |path: &[&str]| {
        let mut within = (0, entries.len(), None::<usize>);
        let mut found = None;
        for key in path {
            let (start, end, parent) = within;
            let child = entries[start..end]
                .iter()
                .map(|(indent, ..)| *indent)
                .filter(|indent| parent.is_none_or(|parent| *indent > parent))
                .min()?;
            let at = (start..end).find(|at| entries[*at].0 == child && entries[*at].1 == *key)?;
            let below = (at + 1..end)
                .find(|next| entries[*next].0 <= child)
                .unwrap_or(end);
            within = (at + 1, below, Some(child));
            found = Some(at);
        }
        found.map(|at| entries[at].2.clone())
    };
    match catalog {
        "default" => {
            lookup(&["catalog", package]).or_else(|| lookup(&["catalogs", "default", package]))
        }
        name => lookup(&["catalogs", name, package]),
    }
}

/// The file on disk a project path names.
pub(super) fn project_file(root: &Path, path: &[String]) -> PathBuf {
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
const PYTHON_CONFIGURATION_FILES: [&str; 8] = [
    "pyproject.toml",
    "setup.cfg",
    "setup.py",
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "tox.ini",
];

/// The files pytest 9 reads its configuration from, in the order it tries
/// them in each directory (`_pytest/config/findpaths.py` `locate_config`).
const PYTEST_CONFIGURATION_FILES: [&str; 7] = [
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
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

/// What makes a directory the one pytest roots a run at where no file above
/// a test configures pytest (`_pytest/config/findpaths.py`
/// `determine_setup`): the nearest `pyproject.toml` it passed on the way up
/// becomes its configuration file, and without one the nearest directory
/// holding a `setup.py` its root. Conftests load from that directory down
/// (`Config._is_in_confcutdir`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rootdir {
    Pyproject,
    SetupPy,
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
    files: &'f [(&'f str, String)],
    pyproject: Option<toml::Table>,
    /// Whether it holds a `pyproject.toml`, read or not.
    has_pyproject: bool,
    setup_cfg: Option<HashMap<String, HashMap<String, String>>>,
    has_setup_py: bool,
}

impl<'f> PythonConfiguration<'f> {
    fn read(files: &'f [(&'f str, String)]) -> Self {
        let file = |name: &str| {
            files
                .iter()
                .find(|(file, _)| *file == name)
                .map(|(_, contents)| contents.as_str())
        };
        Self {
            files,
            pyproject: file("pyproject.toml").and_then(|contents| toml::from_str(contents).ok()),
            has_pyproject: file("pyproject.toml").is_some(),
            setup_cfg: file("setup.cfg").map(ini_sections),
            has_setup_py: file("setup.py").is_some(),
        }
    }

    /// The contents of the file named `name` in the directory.
    fn file(&self, name: &str) -> Option<&'f str> {
        self.files
            .iter()
            .find(|(file, _)| *file == name)
            .map(|(_, contents)| contents.as_str())
    }

    /// Whether the directory's files configure a build, which is what makes
    /// it a project's: a `setup.py`, or a `pyproject.toml` or `setup.cfg`
    /// other than one holding pytest's own table and nothing else. One that
    /// pytest reads no table from — an empty file or `[tool]` table, a
    /// `pyproject.toml` that cannot be parsed — is no pytest configuration,
    /// so it configures a build. `pytest.toml`, `pytest.ini`, their dotted
    /// names and `tox.ini` configure tools, never a build.
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

    /// Whether the directory's files declare a project: a `setup.py`, a
    /// `pyproject.toml` with `[project]` or `[build-system]`, a `setup.cfg`
    /// with `[metadata]` or `[options]`. Its dependencies are installed
    /// whether or not a build installs it, as uv installs those of a
    /// project it does not build.
    fn declares_a_project(&self) -> bool {
        self.declared_by(|pyproject| {
            pyproject.contains_key("project") || pyproject.contains_key("build-system")
        })
    }

    /// Whether a build installs the directory's packages, which is where
    /// setuptools' automatic discovery puts its flat or `src/` layout on
    /// `sys.path`: a project its files declare (`declares_a_project`), but
    /// a `pyproject.toml` uv manages — one holding `[tool.uv]`, pack's mark
    /// of it — only with `[build-system]` unless `tool.uv.package` says
    /// otherwise, as uv documents and uv 0.13.5 installs it; so a uv
    /// workspace's virtual root, holding `[tool.uv.workspace]` alone,
    /// installs nothing.
    fn builds_its_packages(&self) -> bool {
        self.declared_by(|pyproject| match toml_at(pyproject, &["tool", "uv"]) {
            Some(uv) => uv
                .get("package")
                .and_then(toml::Value::as_bool)
                .unwrap_or_else(|| pyproject.contains_key("build-system")),
            None => pyproject.contains_key("project") || pyproject.contains_key("build-system"),
        })
    }

    /// Whether a `setup.py`, a `setup.cfg` with `[metadata]` or `[options]`,
    /// or a `pyproject.toml` that `pyproject` accepts is in the directory.
    fn declared_by(&self, pyproject: impl FnOnce(&toml::Table) -> bool) -> bool {
        self.has_setup_py
            || self.pyproject.as_ref().is_some_and(pyproject)
            || self.setup_cfg.as_ref().is_some_and(|sections| {
                sections.contains_key("metadata") || sections.contains_key("options")
            })
    }
}

/// What the Python files of a project tree are configured by: every
/// project above a Python file, with what it puts on `sys.path`, every
/// pytest configuration there, and each directory pytest roots a run at
/// where no file configures it, each by its directory. A directory is a
/// project's where its files configure a build
/// (`PythonConfiguration::configures_a_build`) and it is no package — one
/// holding `__init__.py` is part of the project above it, its `setup.py` a
/// module like any other, and so is one below a root that an enclosing
/// project, which a build installs (`builds_its_packages`), puts on
/// `sys.path`, unless that project's uv workspace lists it as a member.
/// Below any root, a directory is a project's only where its files declare
/// one (`declares_a_project`).
fn python_configurations(root: &Path, nodes: &[Node]) -> PythonConfigurations {
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
    let mut rootdirs = HashMap::new();
    let mut pyprojects: HashMap<&[String], toml::Table> = HashMap::new();
    let mut projects: Vec<(&Vec<String>, PythonProject)> = Vec::new();
    // Each project's directory, the roots below it, whether a build installs
    // it, which puts those roots on `sys.path`, and its uv workspace's table.
    let mut enclosing: Vec<Enclosing> = Vec::new();
    for (directory, files) in &configurations {
        let mut configuration = PythonConfiguration::read(files);
        if let Some(options) = configuration.pytest(directory) {
            pytest.insert(directory.clone(), options);
        }
        if configuration.has_pyproject {
            rootdirs.insert(directory.clone(), Rootdir::Pyproject);
        } else if configuration.has_setup_py {
            rootdirs.insert(directory.clone(), Rootdir::SetupPy);
        }
        // A directory below a root an enclosing project puts on `sys.path`,
        // other than that project's own directory, lies in the packages that
        // root holds (PEP 420's namespace packages need no `__init__.py`) —
        // unless the project's uv workspace lists it among its members. Below
        // a root no build installs, or as such a member, it starts a project
        // only where its own files declare one, whether or not a build
        // installs it: a file that only configures a tool there leaves it the
        // enclosing project's.
        let mut below_root = false;
        let mut covered = false;
        for project in &enclosing {
            if project
                .roots
                .iter()
                .any(|root| directory.len() > root.len() && directory.starts_with(root))
            {
                below_root = true;
                covered |= project.installed
                    && !project.workspace.as_ref().is_some_and(|workspace| {
                        uv_member(project.directory, workspace, directory)
                    });
            }
        }
        if packages.contains(directory.as_slice())
            || covered
            || !configuration.configures_a_build()
            || (below_root && !configuration.declares_a_project())
        {
            continue;
        }
        let installed = configuration.builds_its_packages();
        let project = python_project(directory, Some(&configuration));
        let roots = project
            .roots
            .iter()
            .filter(|root| root.as_slice() != directory.as_slice())
            .cloned()
            .collect();
        let workspace = configuration
            .pyproject
            .as_ref()
            .and_then(|pyproject| toml_at(pyproject, &["tool", "uv", "workspace"]))
            .cloned();
        enclosing.push(Enclosing {
            directory,
            roots,
            installed,
            workspace,
        });
        projects.push((directory, project));
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
    PythonConfigurations {
        projects,
        pytest,
        rootdirs,
    }
}

/// A Python project as the directories below it see it
/// (`python_configurations`): its directory, the roots below it its
/// configuration names, whether a build installs it — which is what puts
/// those roots on `sys.path` — and its uv workspace's table.
struct Enclosing<'d> {
    directory: &'d [String],
    roots: Vec<Vec<String>>,
    installed: bool,
    workspace: Option<toml::Table>,
}

/// What the Python configuration files of a tree say, each by its
/// directory (`python_configurations`).
struct PythonConfigurations {
    projects: HashMap<Vec<String>, PythonProject>,
    pytest: HashMap<Vec<String>, Pytest>,
    rootdirs: HashMap<Vec<String>, Rootdir>,
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
        let options = self.pytest_options()?;
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

impl PythonConfiguration<'_> {
    /// The options of the one configuration file pytest reads in the
    /// directory, the first of `PYTEST_CONFIGURATION_FILES` that configures
    /// it (`_pytest/config/findpaths.py` `load_config_dict_from_file`):
    /// `pytest.toml` and `.pytest.toml` whatever they hold, their `[pytest]`
    /// table read in TOML's own types; `pytest.ini` and `.pytest.ini` whatever
    /// they hold, their `[pytest]` section; `pyproject.toml`'s `[tool.pytest]`
    /// in TOML's types or its `[tool.pytest.ini_options]` as INI values — a
    /// file holding both is one pytest refuses, which configures nothing —
    /// `tox.ini`'s `[pytest]`; `setup.cfg`'s `[tool:pytest]`. An INI value is
    /// the whitespace-separated items it lists; a TOML-typed one the strings
    /// of the array it must be.
    fn pytest_options(&self) -> Option<HashMap<String, Vec<String>>> {
        let ini = |section: Option<&HashMap<String, String>>| {
            section
                .into_iter()
                .flatten()
                .map(|(key, value)| (key.clone(), ini_list(Some(value))))
                .collect()
        };
        let typed = |table: &toml::Table| -> HashMap<String, Vec<String>> {
            table
                .iter()
                .map(|(key, value)| {
                    let items = value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_string)
                        .collect();
                    (key.clone(), items)
                })
                .collect()
        };
        PYTEST_CONFIGURATION_FILES
            .iter()
            .find_map(|name| match *name {
                "pytest.toml" | ".pytest.toml" => self.file(name).map(|text| {
                    let config = toml::from_str::<toml::Table>(text).ok();
                    config
                        .as_ref()
                        .and_then(|config| config.get("pytest")?.as_table())
                        .map(typed)
                        .unwrap_or_default()
                }),
                "pytest.ini" | ".pytest.ini" => self
                    .file(name)
                    .map(|text| ini(ini_sections(text).get("pytest"))),
                "pyproject.toml" => {
                    let pytest = toml_at(self.pyproject.as_ref()?, &["tool", "pytest"])?;
                    let native: toml::Table = pytest
                        .iter()
                        .filter(|(key, _)| *key != "ini_options")
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    match (native.is_empty(), pytest.get("ini_options")) {
                        (false, Some(_)) => Some(HashMap::new()),
                        (false, None) => Some(typed(&native)),
                        (true, Some(options)) => Some(
                            options
                                .as_table()
                                .into_iter()
                                .flatten()
                                .map(|(key, value)| {
                                    let items = toml_strings(value)
                                        .iter()
                                        .flat_map(|item| {
                                            item.split_whitespace().map(str::to_string)
                                        })
                                        .collect();
                                    (key.clone(), items)
                                })
                                .collect(),
                        ),
                        (true, None) => None,
                    }
                }
                "tox.ini" => {
                    let sections = ini_sections(self.file(name)?);
                    sections.get("pytest").map(|pytest| ini(Some(pytest)))
                }
                _ => self
                    .setup_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.get("tool:pytest"))
                    .map(|pytest| ini(Some(pytest))),
            })
    }
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

    /// The names `classic` tries for a path, in the order tsc 5.9.3 and
    /// 6.0.3 trace them (`rel_js`, `bare_written`): a written extension
    /// replaced by the pass's for it, else a declaration of it, then the
    /// pass's added to the whole name.
    #[test]
    fn classic_resolution_tries_the_names_tsc_traces() {
        let names = |name, extensions| module_names(name, extensions, true);
        let (ts, js) = (Extensions::TypeScript, Extensions::JavaScript);
        assert_eq!(names("util", ts), ["util.ts", "util.tsx", "util.d.ts"]);
        assert_eq!(names("util", js), ["util.js", "util.jsx"]);
        assert_eq!(
            names("w4.ts", ts),
            [
                "w4.ts",
                "w4.tsx",
                "w4.d.ts",
                "w4.ts.ts",
                "w4.ts.tsx",
                "w4.ts.d.ts"
            ]
        );
        assert_eq!(
            names("w4.ts", js),
            ["w4.js", "w4.jsx", "w4.ts.js", "w4.ts.jsx"]
        );
        assert_eq!(
            names("w10.d.ts", ts)[..3],
            ["w10.ts", "w10.tsx", "w10.d.ts"]
        );
        assert_eq!(
            names("w14.tsx", ts),
            [
                "w14.tsx",
                "w14.ts",
                "w14.d.ts",
                "w14.tsx.ts",
                "w14.tsx.tsx",
                "w14.tsx.d.ts"
            ]
        );
        assert_eq!(names("w5.jsx", js)[..2], ["w5.jsx", "w5.js"]);
        assert_eq!(
            names("w8.mjs", ts),
            [
                "w8.mts",
                "w8.d.mts",
                "w8.mjs.ts",
                "w8.mjs.tsx",
                "w8.mjs.d.ts"
            ]
        );
        assert_eq!(names("w8.mjs", js)[0], "w8.mjs");
        assert_eq!(names("w9.cjs", ts)[..2], ["w9.cts", "w9.d.cts"]);
        assert_eq!(
            names("w11.json", ts),
            [
                "w11.d.json.ts",
                "w11.json.ts",
                "w11.json.tsx",
                "w11.json.d.ts"
            ]
        );
        assert_eq!(names("w11.json", js), ["w11.json.js", "w11.json.jsx"]);
        assert_eq!(names("x.vue", ts)[..2], ["x.d.vue.ts", "x.vue.ts"]);
        assert_eq!(
            names("dotted.name", ts)[..2],
            ["dotted.d.name.ts", "dotted.name.ts"]
        );
    }

    /// A path a configuration writes is read against its directory as
    /// TypeScript normalizes one, `..` by name rather than through a link,
    /// and may name a directory above the project's root.
    #[test]
    fn a_configured_path_reads_its_parent_steps_by_name() {
        let directory = ["repo".to_string(), "base".to_string()];
        assert_eq!(
            configured_path(Path::new("/w/outer"), &directory, "../../types"),
            Path::new("/w/outer/types")
        );
        assert_eq!(
            configured_path(Path::new("/w/outer"), &directory, "./../../../t/./u"),
            Path::new("/w/t/u")
        );
        assert_eq!(
            configured_path(Path::new("/w/outer"), &directory, "/abs/types"),
            Path::new("/abs/types")
        );
    }

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

    /// A declared `typescript` range gives a major only where its form is
    /// one pack reads and every version it takes shares one, its lower
    /// bound's, every alternative agreeing. A `>` bound and a hyphen range
    /// are read as node-semver 7.8.5 expands them: `>5 <7` as
    /// `>=6.0.0 <7.0.0-0`, `>6.2 <7` as `>=6.3.0 <7.0.0-0`, `5.0.0 - 5.9.9`
    /// as `>=5.0.0 <=5.9.9`, `5.0.0 - 5` as `>=5.0.0 <6.0.0-0`; a wildcard
    /// end (`* - 5`) is not read, and `5.0.0 -5.9.9` is no range it reads.
    #[test]
    fn a_typescript_range_fixes_a_major_only_by_its_lower_bound() {
        for (range, major) in [
            ("^5.9.0", Some(5)),
            ("~6.0", Some(6)),
            ("6.x", Some(6)),
            ("7", Some(7)),
            ("=5.9.3", Some(5)),
            ("^7.0.0-beta", Some(7)),
            ("^5.9 || 5.x", Some(5)),
            ("^5.0.0 || ^6.0.0", None),
            (">=5", None),
            (">=5.0.0 <6.0.0", Some(5)),
            (">= 6.0.0 < 7", Some(6)),
            (">=6.0.0 <=6.0.3", Some(6)),
            (">=5.0.0 <6.0.0-0", Some(5)),
            (">=5.9 <6.1", None),
            (">=5 <7", None),
            ("<6", None),
            (">5 <7", Some(6)),
            (">6 <8", Some(7)),
            (">6.x <8", Some(7)),
            (">6.2 <7", Some(6)),
            (">5.0.0 <6", Some(5)),
            (">= 5 > 5", None),
            ("5.0.0 - 5.9.9", Some(5)),
            ("5.0.0 - 5", Some(5)),
            ("5.2 - 5.9", Some(5)),
            ("v5.0.0 - v5.9.9", Some(5)),
            ("5 - 6", None),
            ("* - 5", None),
            ("5 - *", None),
            ("5.0.0 -5.9.9", None),
            ("*", None),
            ("latest", None),
            ("workspace:*", None),
            ("", None),
        ] {
            assert_eq!(typescript_major(range), major, "{range:?}");
        }
    }

    /// A declared range admits a major below 7 where an alternative of it is
    /// bounded below under 7 or not at all, or is no set of comparators pack
    /// reads; one whose every alternative starts at 7 or above admits none.
    #[test]
    fn a_typescript_range_admits_a_major_below_7_by_its_alternatives_lower_bounds() {
        for (range, admits) in [
            (">=7", false),
            (">=7.0.0", false),
            ("7.x || 8.x", false),
            ("^7.0.2 || ^8.0.0", false),
            (">6", false),
            (">6.x", false),
            ("^7.0.2 || >6", false),
            ("7 - 8", false),
            (">6.2", true),
            (">6.0.0", true),
            ("6 - 7", true),
            ("* - 8", true),
            (">=6", true),
            ("<8", true),
            ("^7.0.2 || ^6.0.3", true),
            ("latest", true),
            ("*", true),
        ] {
            assert_eq!(range_admits_below_7(range), admits, "{range:?}");
        }
    }

    /// A pnpm catalog maps a package to its range: the default catalog under
    /// `catalog` or `catalogs.default`, a named one under `catalogs`, keys
    /// and values plain or quoted, comments aside.
    #[test]
    fn a_pnpm_catalog_names_the_range_of_a_package() {
        let yaml = "packages:\n  - apps/*\ncatalog:\n  # the compiler\n  \"typescript\": ^7.0.2 # native\n  \
                    react: 19.0.0\ncatalogs:\n  legacy:\n    typescript: '~5.9'\n  default:\n    vite: ^7\n";
        assert_eq!(
            pnpm_catalog(yaml, "default", "typescript").as_deref(),
            Some("^7.0.2")
        );
        assert_eq!(pnpm_catalog(yaml, "default", "vite").as_deref(), Some("^7"));
        assert_eq!(
            pnpm_catalog(yaml, "legacy", "typescript").as_deref(),
            Some("~5.9")
        );
        assert_eq!(pnpm_catalog(yaml, "legacy", "react"), None);
        assert_eq!(pnpm_catalog(yaml, "missing", "typescript"), None);
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
            TsOwner::Uncompiled(project) if manifests.reached(project).len() == PACKAGES
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
