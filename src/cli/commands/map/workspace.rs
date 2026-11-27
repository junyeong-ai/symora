//! What a project's manifests declare about its shape: the packages its
//! workspaces list.
//!
//! Read from what a build tool reads, never guessed from a directory's
//! name. A workspace whose packages all sit under `packages/` is one row of
//! `top_directories`; its members are the rows an agent picks a package
//! from.

use std::collections::BTreeSet;
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

/// What the manifests declare, and what kept some of it from being read.
pub(super) struct Declared {
    /// `None` where no manifest declares a workspace.
    pub(super) packages: Option<Vec<PackageOutput>>,
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

    declared.packages = packages(&manifests, records, root, &mut declared.hints);
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

/// The globs a `pnpm-workspace.yaml` lists under `packages`, read as YAML:
/// a block or a flow sequence, on one line or several. `Err` says why it is
/// no workspace file pnpm reads — not YAML, `packages` no list of globs.
fn pnpm_workspace_globs(yaml: &str) -> Result<Vec<String>, String> {
    let documents = yaml_rust2::YamlLoader::load_from_str(yaml).map_err(|e| e.to_string())?;
    let Some(document) = documents.first() else {
        return Ok(Vec::new());
    };
    if document.as_hash().is_none() {
        return Err("it is no mapping".to_string());
    }
    let packages = &document["packages"];
    if packages.is_badvalue() || packages.is_null() {
        return Ok(Vec::new());
    }
    packages
        .as_vec()
        .ok_or("its packages are no list")?
        .iter()
        .map(|glob| {
            glob.as_str()
                .map(str::to_string)
                .ok_or_else(|| "its packages hold a value that is no glob".to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnpm_lists_its_packages_as_a_block_or_a_flow() {
        assert_eq!(
            pnpm_workspace_globs(
                "packages:\n  - 'apps/*'\n  # c\n  - \"libs/**\"\n  - '!libs/x'\ncatalog:\n  a: 1\n"
            )
            .unwrap(),
            vec!["apps/*", "libs/**", "!libs/x"]
        );
        assert_eq!(
            pnpm_workspace_globs("packages: ['a/*', \"b\"]\n").unwrap(),
            vec!["a/*", "b"]
        );
        assert_eq!(
            pnpm_workspace_globs("packages: [\n  \"packages/*\",\n  'tools/*' # c\n  ]\n").unwrap(),
            vec!["packages/*", "tools/*"]
        );
        assert_eq!(
            pnpm_workspace_globs("catalog:\n  a: 1\n").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(pnpm_workspace_globs("").unwrap(), Vec::<String>::new());
        // YAML 1.2 indents a flow sequence's lines past the key holding it,
        // so a `]` at the margin is refused, and said, though pnpm 12.23.0
        // reads the file; refused too are YAML cut short and `packages`
        // that is no list of globs.
        for broken in [
            "packages: [\n  \"packages/*\"\n]\n",
            "packages: [\n  'a/*'\n",
            "packages: 3\n",
            "packages: [1]\n",
            "- a\n",
        ] {
            assert!(pnpm_workspace_globs(broken).is_err(), "{broken:?}");
        }
    }
}
