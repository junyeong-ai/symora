//! What a project's manifests declare about its shape: the packages its
//! workspaces list, and the entrypoints its packages declare.
//!
//! Both are read from what a build tool reads, never guessed from a file's
//! name. A workspace whose packages all sit under `packages/` is one row of
//! `top_directories`; its members are the rows an agent picks a package
//! from. And a project's `app.py` or `server.ts` may be where it starts or
//! may be one module among many — a manifest's `[[bin]]`, script or `main`
//! is where the build says it starts.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::FileRecord;

/// One package a workspace manifest lists.
#[derive(Debug, Serialize)]
pub(super) struct PackageOutput {
    path: String,
    /// The name its own manifest declares, which the build tool selects it
    /// by (`cargo -p`, `uv run --package`, `pnpm --filter`).
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// The manifest that declares it: which tool builds it.
    manifest: &'static str,
    /// Code files below it and none of the other packages listed.
    file_count: usize,
    test_files: usize,
}

/// What a declared entrypoint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EntryKind {
    /// An executable target: a Cargo binary, a `package.json` `bin`, a Go
    /// `main` package's `main.go`.
    Bin,
    /// What importing the package loads: a Cargo library, a `package.json`
    /// `main`.
    Lib,
    /// A Python console or GUI script.
    Script,
    /// A Python package's `__main__.py`, which `python -m` runs.
    MainModule,
}

#[derive(Debug, Serialize)]
pub(super) struct EntryPointOutput {
    pub(super) file: String,
    pub(super) kind: EntryKind,
    /// What the manifest calls it: the binary, library or script name, or
    /// the module `python -m` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl EntryPointOutput {
    pub(super) fn go_main(file: String) -> Self {
        Self {
            file,
            kind: EntryKind::Bin,
            name: None,
        }
    }
}

/// What the manifests declare, and what kept some of it from being read.
pub(super) struct Declared {
    /// `None` where no manifest declares a workspace.
    pub(super) packages: Option<Vec<PackageOutput>>,
    pub(super) entrypoints: Vec<EntryPointOutput>,
    /// `main.go` files, which are a command's where the grammar reads their
    /// package as `main`.
    pub(super) go_mains: Vec<PathBuf>,
    pub(super) unread: Vec<PathBuf>,
    pub(super) hints: Vec<String>,
}

/// A manifest the walk found, parsed.
struct Manifest<'r> {
    record: &'r FileRecord,
    content: Parsed,
}

enum Parsed {
    Toml(toml::Table),
    Json(serde_json::Value),
    /// Its globs, or `None` where it could not be parsed: the file still
    /// declares a workspace, whatever it lists.
    PnpmWorkspace(Option<Vec<String>>),
}

impl Manifest<'_> {
    fn directory(&self) -> &str {
        &self.record.parent
    }

    fn file_name(&self) -> &str {
        Path::new(&self.record.rel_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    }

    fn toml(&self) -> Option<&toml::Table> {
        match &self.content {
            Parsed::Toml(table) => Some(table),
            _ => None,
        }
    }

    fn json(&self) -> Option<&serde_json::Value> {
        match &self.content {
            Parsed::Json(value) => Some(value),
            _ => None,
        }
    }

    /// The package name the manifest declares.
    fn package_name(&self) -> Option<String> {
        let name = match self.file_name() {
            "Cargo.toml" => toml_at(self.toml()?, &["package", "name"])?.as_str(),
            "pyproject.toml" => toml_at(self.toml()?, &["project", "name"])?.as_str(),
            "package.json" => self.json()?.get("name")?.as_str(),
            _ => None,
        }?;
        Some(name.to_string())
    }
}

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "pnpm-workspace.yaml",
];

/// Read every manifest the walk found outside test material — a fixture's
/// manifest declares the fixture, not the project.
pub(super) fn read(records: &[FileRecord], root: &Path) -> Declared {
    let mut declared = Declared {
        packages: None,
        entrypoints: Vec::new(),
        go_mains: Vec::new(),
        unread: Vec::new(),
        hints: Vec::new(),
    };
    let mut manifests = Vec::new();
    for record in records.iter().filter(|record| !record.is_test) {
        let Some(file_name) = Path::new(&record.rel_path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| MANIFESTS.contains(name))
        else {
            continue;
        };
        let text = match std::fs::read_to_string(&record.abs_path) {
            Ok(text) => text,
            Err(e) => {
                if crate::infra::hides_text(&e) {
                    declared.unread.push(record.abs_path.clone());
                }
                continue;
            }
        };
        let content = match file_name {
            "package.json" => serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map(Parsed::Json)
                .map_err(|e| e.to_string()),
            "pnpm-workspace.yaml" => {
                pnpm_workspace_globs(&text).map(|g| Parsed::PnpmWorkspace(Some(g)))
            }
            _ => text
                .parse::<toml::Table>()
                .map(Parsed::Toml)
                .map_err(|e| e.message().to_string()),
        };
        match content {
            Ok(content) => manifests.push(Manifest { record, content }),
            Err(e) => {
                declared.hints.push(format!(
                    "{} could not be parsed ({e}), so what it declares is not listed.",
                    record.rel_path
                ));
                if file_name == "pnpm-workspace.yaml" {
                    manifests.push(Manifest {
                        record,
                        content: Parsed::PnpmWorkspace(None),
                    });
                }
            }
        }
    }

    let paths: HashSet<&str> = records.iter().map(|r| r.rel_path.as_str()).collect();
    declared.packages = packages(&manifests, records, root, &mut declared.hints);
    let mut unplaced = Vec::new();
    for manifest in &manifests {
        entrypoints_of(manifest, &paths, &mut declared.entrypoints, &mut unplaced);
    }
    for record in records.iter().filter(|record| !record.is_test) {
        let file_name = Path::new(&record.rel_path)
            .file_name()
            .and_then(|name| name.to_str());
        match file_name {
            Some("__main__.py") => declared.entrypoints.push(EntryPointOutput {
                file: record.rel_path.clone(),
                kind: EntryKind::MainModule,
                name: super::tests_of::python_module_names(&record.rel_path, &paths)
                    .into_iter()
                    .next()
                    .and_then(|name| name.strip_suffix(".__main__").map(str::to_string)),
            }),
            Some("main.go") => declared.go_mains.push(record.abs_path.clone()),
            _ => {}
        }
    }
    if !unplaced.is_empty() {
        let shown: Vec<_> = unplaced.iter().take(3).cloned().collect();
        declared.hints.push(format!(
            "{} declared entrypoint{} name no file the walk found — a build output, or a \
             layout this reading does not follow: {}{}.",
            unplaced.len(),
            if unplaced.len() == 1 { "" } else { "s" },
            shown.join(", "),
            if unplaced.len() > shown.len() {
                ", …"
            } else {
                ""
            }
        ));
    }
    declared
}

/// The packages the workspace manifests list, each counting the code files
/// below it that no package below it holds; `None` where no manifest
/// declares a workspace.
fn packages(
    manifests: &[Manifest],
    records: &[FileRecord],
    root: &Path,
    hints: &mut Vec<String>,
) -> Option<Vec<PackageOutput>> {
    let mut members: BTreeSet<(String, &'static str, Option<String>)> = BTreeSet::new();
    let mut declares_workspace = false;
    for workspace in manifests {
        let (member_manifest, globs, excluded) = match workspace.file_name() {
            "Cargo.toml" => {
                let Some(table) = workspace.toml().and_then(|t| t.get("workspace")) else {
                    continue;
                };
                (
                    "Cargo.toml",
                    strings(table.get("members")),
                    strings(table.get("exclude")),
                )
            }
            "pyproject.toml" => {
                let Some(table) = workspace
                    .toml()
                    .and_then(|t| toml_at(t, &["tool", "uv", "workspace"]))
                else {
                    continue;
                };
                (
                    "pyproject.toml",
                    strings(table.get("members")),
                    strings(table.get("exclude")),
                )
            }
            "package.json" => {
                let Some(listed) = workspace.json().and_then(|json| json.get("workspaces")) else {
                    continue;
                };
                let listed = listed.get("packages").unwrap_or(listed);
                let globs: Vec<String> = listed
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|glob| glob.as_str().map(str::to_string))
                    .collect();
                let (excluded, globs) = globs.into_iter().partition(|glob| glob.starts_with('!'));
                ("package.json", globs, excluded)
            }
            "pnpm-workspace.yaml" => {
                let Parsed::PnpmWorkspace(globs) = &workspace.content else {
                    continue;
                };
                let (excluded, globs) = globs
                    .iter()
                    .flatten()
                    .cloned()
                    .partition(|g| g.starts_with('!'));
                ("package.json", globs, excluded)
            }
            _ => continue,
        };
        declares_workspace = true;
        let included = glob_set(&globs);
        let excluded = glob_set(
            &excluded
                .iter()
                .map(|glob| glob.trim_start_matches('!').to_string())
                .collect::<Vec<_>>(),
        );
        let base = workspace.directory();
        for manifest in manifests
            .iter()
            .filter(|manifest| manifest.file_name() == member_manifest)
        {
            let directory = manifest.directory();
            let member = if directory == base {
                // A workspace's root is one of its packages where it
                // declares one of its own, as Cargo, uv and pnpm take it.
                if manifest.package_name().is_none() {
                    continue;
                }
                Some("")
            } else if base.is_empty() {
                Some(directory)
            } else {
                directory
                    .strip_prefix(base)
                    .and_then(|rest| rest.strip_prefix('/'))
            };
            let Some(member) = member else {
                continue;
            };
            let listed = member.is_empty()
                || (included.as_ref().is_some_and(|set| set.is_match(member))
                    && !excluded.as_ref().is_some_and(|set| set.is_match(member)));
            if listed {
                members.insert((
                    directory.to_string(),
                    member_manifest,
                    manifest.package_name(),
                ));
            }
        }
    }
    if let Some(listed) = go_work_members(root, hints) {
        declares_workspace = true;
        members.extend(
            listed
                .into_iter()
                .map(|(directory, module)| (directory, "go.mod", module)),
        );
    }
    if !declares_workspace {
        return None;
    }

    // Deepest first, so a file counts for the innermost package holding it.
    let mut by_depth: Vec<&(String, &'static str, Option<String>)> = members.iter().collect();
    by_depth.sort_by_key(|(directory, _, _)| std::cmp::Reverse(directory.len()));
    let mut counts = vec![(0usize, 0usize); by_depth.len()];
    for record in records.iter().filter(|record| record.language.is_code()) {
        let holder = by_depth.iter().position(|(directory, _, _)| {
            directory.is_empty()
                || record
                    .rel_path
                    .strip_prefix(directory.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if let Some(at) = holder {
            counts[at].0 += 1;
            counts[at].1 += usize::from(record.is_test);
        }
    }
    let mut packages: Vec<_> = by_depth
        .into_iter()
        .zip(counts)
        .map(
            |((directory, manifest, name), (file_count, test_files))| PackageOutput {
                path: if directory.is_empty() {
                    ".".to_string()
                } else {
                    directory.clone()
                },
                name: name.clone(),
                manifest,
                file_count,
                test_files,
            },
        )
        .collect();
    packages.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.path.cmp(&b.path))
    });
    Some(packages)
}

/// The modules a root `go.work` uses, each with the module path its
/// `go.mod` declares; `None` without one. What it cannot read — the file,
/// a directory outside the project, a member's `go.mod` — goes to `hints`,
/// never past them: a go.work that does not parse lists no member, as `go`
/// loads none from it.
fn go_work_members(root: &Path, hints: &mut Vec<String>) -> Option<Vec<(String, Option<String>)>> {
    let text = match std::fs::read_to_string(root.join("go.work")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            hints.push(format!(
                "go.work could not be read ({e}), so the modules it uses are not listed."
            ));
            return Some(Vec::new());
        }
    };
    let used = match super::modfile::used_directories(&text) {
        Ok(used) => used,
        Err(e) => {
            hints.push(format!(
                "go.work could not be parsed ({e}), so the modules it uses are not listed."
            ));
            return Some(Vec::new());
        }
    };
    let mut members = Vec::new();
    for written in used {
        let path = Path::new(&written);
        let directory = if path.is_absolute() {
            path.strip_prefix(root)
                .ok()
                .and_then(|inside| inside.to_str())
                .map(|inside| inside.trim_end_matches('/').to_string())
        } else {
            super::tests_of::joined("", &written)
        };
        let Some(directory) = directory else {
            hints.push(format!(
                "go.work uses {written}, outside the project, which is not listed."
            ));
            continue;
        };
        let go_mod = if directory.is_empty() {
            "go.mod".to_string()
        } else {
            format!("{directory}/go.mod")
        };
        let module = match std::fs::read_to_string(root.join(&go_mod)) {
            Ok(text) => match super::modfile::module_path(&text) {
                Ok(Some(module)) => Some(module),
                Ok(None) => {
                    hints.push(format!("{go_mod}, which go.work uses, declares no module."));
                    None
                }
                Err(e) => {
                    hints.push(format!(
                        "{go_mod}, which go.work uses, could not be parsed ({e}), so its \
                         module is not named."
                    ));
                    None
                }
            },
            Err(e) => {
                hints.push(format!(
                    "{go_mod}, which go.work uses, could not be read ({e}), so its module is \
                     not named."
                ));
                None
            }
        };
        members.push((directory, module));
    }
    Some(members)
}

/// The entrypoints a package manifest declares, placed on the files the walk
/// found; those it names no such file for go to `unplaced`.
fn entrypoints_of(
    manifest: &Manifest,
    paths: &HashSet<&str>,
    found: &mut Vec<EntryPointOutput>,
    unplaced: &mut Vec<String>,
) {
    let directory = manifest.directory();
    let at = |relative: &str| -> String {
        let relative = relative.trim_start_matches("./");
        if directory.is_empty() {
            relative.to_string()
        } else {
            format!("{directory}/{relative}")
        }
    };
    let mut place = |file: Option<String>, kind, name: Option<String>, declared: String| match file
        .filter(|file| paths.contains(file.as_str()))
    {
        Some(file) => found.push(EntryPointOutput { file, kind, name }),
        None => unplaced.push(format!("{} {declared}", manifest.record.rel_path)),
    };
    match manifest.file_name() {
        "Cargo.toml" => {
            let Some(table) = manifest.toml() else {
                return;
            };
            let Some(package) = toml_at(table, &["package", "name"]).and_then(|n| n.as_str())
            else {
                return;
            };
            let crate_name = package.replace('-', "_");
            let lib = table.get("lib");
            let lib_path = lib
                .and_then(|lib| lib.get("path"))
                .and_then(|path| path.as_str())
                .unwrap_or("src/lib.rs");
            if lib.is_some() || paths.contains(at(lib_path).as_str()) {
                let name = lib
                    .and_then(|lib| lib.get("name"))
                    .and_then(|name| name.as_str())
                    .map_or(crate_name, str::to_string);
                place(
                    Some(at(lib_path)),
                    EntryKind::Lib,
                    Some(name),
                    "[lib]".into(),
                );
            }
            let mut bins: Vec<(String, String)> = Vec::new();
            for bin in table
                .get("bin")
                .and_then(|bins| bins.as_array())
                .into_iter()
                .flatten()
            {
                let Some(name) = bin.get("name").and_then(|name| name.as_str()) else {
                    continue;
                };
                let file = match bin.get("path").and_then(|path| path.as_str()) {
                    Some(path) => Some(at(path)),
                    None => [
                        (name == package).then(|| at("src/main.rs")),
                        Some(at(&format!("src/bin/{name}.rs"))),
                        Some(at(&format!("src/bin/{name}/main.rs"))),
                    ]
                    .into_iter()
                    .flatten()
                    .find(|file| paths.contains(file.as_str())),
                };
                if let Some(file) = &file {
                    bins.push((file.clone(), name.to_string()));
                }
                place(
                    file,
                    EntryKind::Bin,
                    Some(name.into()),
                    format!("[[bin]] {name}"),
                );
            }
            let discovers =
                toml_at(table, &["package", "autobins"]).and_then(|a| a.as_bool()) != Some(false);
            if discovers {
                let bin_dir = at("src/bin/");
                let mut discovered: Vec<(String, String)> = Vec::new();
                if paths.contains(at("src/main.rs").as_str()) {
                    discovered.push((at("src/main.rs"), package.to_string()));
                }
                for path in paths
                    .iter()
                    .filter_map(|path| path.strip_prefix(bin_dir.as_str()))
                {
                    let name = match path.split_once('/') {
                        Some((name, "main.rs")) => name,
                        None => match path.strip_suffix(".rs") {
                            Some(name) => name,
                            None => continue,
                        },
                        Some(_) => continue,
                    };
                    discovered.push((format!("{bin_dir}{path}"), name.to_string()));
                }
                discovered.sort();
                for (file, name) in discovered {
                    if bins.iter().any(|(declared, _)| *declared == file) {
                        continue;
                    }
                    found.push(EntryPointOutput {
                        file,
                        kind: EntryKind::Bin,
                        name: Some(name),
                    });
                }
            }
        }
        "pyproject.toml" => {
            let Some(table) = manifest.toml() else {
                return;
            };
            let module_root = toml_at(table, &["tool", "uv", "build-backend", "module-root"])
                .and_then(|root| root.as_str());
            let roots: Vec<String> = match module_root {
                Some(root) => vec![at(root)],
                None => vec![at("src"), directory.to_string()],
            };
            for scripts in [
                &["project", "scripts"][..],
                &["project", "gui-scripts"][..],
                &["tool", "poetry", "scripts"][..],
            ] {
                let Some(scripts) = toml_at(table, scripts).and_then(|s| s.as_table()) else {
                    continue;
                };
                for (name, target) in scripts {
                    let Some(target) = target.as_str() else {
                        continue;
                    };
                    let module = target.split(':').next().unwrap_or_default().trim();
                    let module_path = module.replace('.', "/");
                    let mut candidates = roots.iter().flat_map(|root| {
                        let base = if root.is_empty() {
                            module_path.clone()
                        } else {
                            format!("{root}/{module_path}")
                        };
                        [format!("{base}.py"), format!("{base}/__init__.py")]
                    });
                    let file = candidates.find(|file| paths.contains(file.as_str()));
                    place(
                        file,
                        EntryKind::Script,
                        Some(name.clone()),
                        format!("script {name}"),
                    );
                }
            }
        }
        "package.json" => {
            let Some(json) = manifest.json() else {
                return;
            };
            let package = json.get("name").and_then(|name| name.as_str());
            match json.get("bin") {
                Some(serde_json::Value::String(file)) => {
                    let name = package.map(|p| p.rsplit('/').next().unwrap_or(p).to_string());
                    place(Some(at(file)), EntryKind::Bin, name, "bin".into());
                }
                Some(serde_json::Value::Object(bins)) => {
                    for (name, file) in bins {
                        if let Some(file) = file.as_str() {
                            place(
                                Some(at(file)),
                                EntryKind::Bin,
                                Some(name.clone()),
                                format!("bin {name}"),
                            );
                        }
                    }
                }
                _ => {}
            }
            if let Some(main) = json.get("main").and_then(|main| main.as_str()) {
                place(
                    Some(at(main)),
                    EntryKind::Lib,
                    package.map(str::to_string),
                    "main".into(),
                );
            }
        }
        _ => {}
    }
}

fn toml_at<'t>(table: &'t toml::Table, keys: &[&str]) -> Option<&'t toml::Value> {
    let (first, rest) = keys.split_first()?;
    rest.iter()
        .try_fold(table.get(*first)?, |value, key| value.get(*key))
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// The globs as one matcher over member paths relative to the workspace,
/// `*` stopping at `/` as Cargo's, uv's and the package managers' globs do.
fn glob_set(globs: &[String]) -> Option<globset::GlobSet> {
    if globs.is_empty() {
        return None;
    }
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
}

/// What pnpm reads of a `pnpm-workspace.yaml` to find its projects: the
/// `packages` of pnpm 12's `WorkspaceManifest`.
#[derive(serde::Deserialize)]
struct PnpmWorkspace {
    #[serde(default)]
    packages: Option<Vec<String>>,
}

/// The globs a `pnpm-workspace.yaml` lists under `packages`, read as pnpm
/// 12's `parse_workspace_manifest` reads them: with serde-saphyr and its
/// default options, so a scalar that is no string is the glob it spells,
/// and an empty entry refuses the file. `Err` says why it is no workspace
/// file pnpm reads. pnpm alone retries a file that does not parse with its
/// `${…}` placeholders replaced, so one that parses only then is refused
/// here, and said.
fn pnpm_workspace_globs(yaml: &str) -> Result<Vec<String>, String> {
    let globs = serde_saphyr::from_str::<Option<PnpmWorkspace>>(yaml)
        .map_err(|e| yaml_error(&e))?
        .and_then(|manifest| manifest.packages)
        .unwrap_or_default();
    if globs.iter().any(String::is_empty) {
        return Err(EMPTY_PACKAGE.to_string());
    }
    Ok(globs)
}

const EMPTY_PACKAGE: &str =
    "an entry of packages is empty, which pnpm refuses as \"Missing or empty package\"";

/// A YAML manifest's parse failure as serde-saphyr words it for the file's
/// author, not for the program calling it, on one line ending in the line
/// and column it names: what a hint carries.
pub(super) fn yaml_error(error: &serde_saphyr::Error) -> String {
    error.render_with_options(serde_saphyr::render_options! {
        formatter: &serde_saphyr::UserMessageFormatter,
        snippets: serde_saphyr::SnippetMode::Off,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnpm_lists_its_packages_as_a_block_or_a_flow() {
        let globs = |yaml: &str| pnpm_workspace_globs(yaml).unwrap();
        assert_eq!(
            globs(
                "packages:\n  - 'apps/*'\n  # c\n  - \"libs/**\"\n  - '!libs/x'\ncatalog:\n  a: 1\n"
            ),
            vec!["apps/*", "libs/**", "!libs/x"]
        );
        assert_eq!(globs("packages: ['a/*', \"b\"]\n"), vec!["a/*", "b"]);
        assert_eq!(
            globs("packages: [\n  \"packages/*\",\n  'tools/*' # c\n  ]\n"),
            vec!["packages/*", "tools/*"]
        );
        // Each as pnpm 12.23.0 reads it: a flow sequence's lines at the
        // margin, a byte-order mark, a merge key, and a scalar that is no
        // string taken for the glob it spells.
        assert_eq!(
            globs("packages: [\n  \"packages/*\"\n]\n"),
            vec!["packages/*"]
        );
        assert_eq!(globs("packages: [\n'a/*',\n'b/*'\n]\n"), vec!["a/*", "b/*"]);
        assert_eq!(globs("\u{feff}packages:\n  - 'a/*'\n"), vec!["a/*"]);
        assert_eq!(
            globs("base: &b\n  packages: ['a/*']\n<<: *b\n"),
            vec!["a/*"]
        );
        assert_eq!(globs("packages: [1]\n"), vec!["1"]);
        for none in ["catalog:\n  a: 1\n", "packages:\n", "# c\n", ""] {
            assert_eq!(globs(none), Vec::<String>::new(), "{none:?}");
        }
        // And each as it refuses it: a tab for indentation, a duplicate key,
        // a second document, YAML cut short, `packages` no list of globs,
        // an empty entry.
        for broken in [
            "packages:\n\t- 'a/*'\n",
            "packages: ['a/*']\npackages: ['b/*']\n",
            "packages: ['a/*']\n---\npackages: ['b/*']\n",
            "packages: [\n  'a/*'\n",
            "packages: 3\n",
            "packages: [['a/*']]\n",
            "packages: ['a/*', ~]\n",
            "packages: ['a/*', '']\n",
            "- a\n",
        ] {
            assert!(pnpm_workspace_globs(broken).is_err(), "{broken:?}");
        }
        // Each refusal worded for the file's author, where it stands.
        for (broken, said) in [
            (
                "packages: ['a/*']\npackages: ['b/*']\n",
                "duplicate mapping key `packages` is not allowed here at line 2, column 1",
            ),
            (
                "packages:\n\t- 'a/*'\n",
                "tabs disallowed within this context (block indentation) at line 2, column 2",
            ),
            (
                "packages: [\n  'a/*'\n",
                "unclosed bracket '[' at line 1, column 11",
            ),
            ("packages:\n  - ''\n", EMPTY_PACKAGE),
        ] {
            assert_eq!(
                pnpm_workspace_globs(broken).unwrap_err(),
                said,
                "{broken:?}"
            );
        }
        // Where this departs from pnpm, said rather than guessed: a flow
        // sequence pnpm reads only by retrying with its `${…}` placeholder
        // replaced is refused here.
        assert!(pnpm_workspace_globs("packages: [${DIR}/*]\n").is_err());
    }
}
