//! Whether the Python server resolves this tree's own workspace members here.
//!
//! pyright resolves an import of a workspace member through the interpreter
//! it runs with. Where that interpreter installs the member from another
//! checkout, every import of it, from every other member, resolves to that
//! checkout's copy: a reference to this tree's declaration from another file
//! is a reference to the copy, so every cross-file answer about the
//! declaration — references, callers, callees, implementations, impact, a
//! rename's edit set — is short, and the server reports nothing amiss. No
//! retry changes that; building the tree's own environment does.
//!
//! The members are those of the root's uv workspace (`[tool.uv.workspace]`,
//! read by `map`'s [`uv_members`]). The root's own project is not one of
//! them here: its modules sit under the server's execution root, which
//! pyright searches before any environment, so no environment decides where
//! it resolves. (A root project whose modules lie under neither the root nor
//! `src` is the limit of that reading.)
//!
//! A member is judged by what the environment records, never by its paths'
//! spelling:
//!
//! - `member_shadowed`: the interpreter symora hands the server — the
//!   `python.pythonPath` setting, or the project's own environment
//!   ([`venv_python`]) — installs it editable (PEP 610 `dir_info.editable`)
//!   from a directory outside the project, or installs it as a copy: not
//!   editable, its `RECORD` putting into site-packages a top-level package
//!   or module the member holds (under its `src` or its own directory),
//!   which pyright then binds every import of it to, wherever that
//!   site-packages lies. `installed_from` is the editable source, or the
//!   site-packages directory holding the copy.
//! - `member_unresolved`: symora hands the server no interpreter, so the
//!   server picks its own, which symora cannot see. Nothing then proves the
//!   member resolves to this tree, so every member is unresolved. What symora
//!   can see is stated beside it: whether `$VIRTUAL_ENV`, which lies outside
//!   the project or it would have been handed over, shadows the member by
//!   the same reading, and so whether any interpreter symora can see installs
//!   the members at all. That is evidence about the server's choice, not a
//!   reading of it.
//!
//! Limits, each a member read as resolving here when it may not: an
//! environment's installs are read only as [`installs`] reads them, so one
//! recorded without a `.dist-info` directory is not flagged, nor a copy
//! whose `RECORD` installs only an extension module, nor one whose packages
//! the member holds under a `module-root` other than `src` or its own
//! directory; a selected interpreter that does not install a member at all
//! is not flagged, since the server may still reach it through `extraPaths`
//! or its execution root, which this does not read; a `python.pythonPath`
//! setting that is no string is not read; and `$VIRTUAL_ENV` is read from
//! the process running the command, while a daemon's servers run with the
//! daemon's own, fixed when it started. A workspace declared by anything
//! but uv is not read.

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::cli::OutputError;
use crate::cli::commands::map::{UvMember, uv_members};
use crate::cli::response::CoverageGap;
use crate::cli::response::disclosure::CoverageReason;
use crate::config::LspRuntimeConfig;
use crate::infra::lsp::init_options::{
    Install, InstallKind, installs, normalized_distribution_name, venv_python, virtual_env_python,
};
use crate::models::symbol::Language;

/// The interpreter a fresh Python server is handed, by the precedence the
/// server's configuration takes it in.
#[derive(Debug, Clone, PartialEq)]
pub enum Interpreter {
    /// `lsp.servers.python.settings.python.pythonPath`, as configured.
    Settings(serde_json::Value),
    /// The project's own environment ([`venv_python`]).
    ProjectEnvironment(PathBuf),
    /// None: the server picks its own.
    ServerDefault,
}

impl Interpreter {
    pub fn selected(root: &Path, config: &LspRuntimeConfig) -> Self {
        if let Some(path) = config
            .servers
            .get(Language::Python.lsp_id())
            .and_then(|server| server.settings.as_ref())
            .and_then(|settings| settings.get("python"))
            .and_then(|python| python.get("pythonPath"))
        {
            return Self::Settings(path.clone());
        }
        match venv_python(root) {
            Some(interpreter) => Self::ProjectEnvironment(interpreter),
            None => Self::ServerDefault,
        }
    }

    /// The interpreter whose installs decide where the server resolves the
    /// members, where symora selects one it can read.
    fn path(&self, root: &Path) -> Option<PathBuf> {
        match self {
            Self::Settings(serde_json::Value::String(path)) => Some(root.join(path)),
            Self::Settings(_) | Self::ServerDefault => None,
            Self::ProjectEnvironment(path) => Some(path.clone()),
        }
    }
}

/// A workspace member whose cross-file answers the server's environment
/// does not establish here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberGap {
    /// The member's project name, as its manifest writes it.
    pub name: String,
    /// Its directory, relative to the project root.
    pub path: String,
    /// [`CoverageReason::MemberShadowed`] or [`CoverageReason::MemberUnresolved`].
    pub reason: CoverageReason,
    /// Where an environment resolves it other than to this tree: the
    /// selected interpreter's for a shadowed member, `$VIRTUAL_ENV`'s for an
    /// unresolved one. An editable install's source outside the project, or
    /// the site-packages directory holding a copy.
    pub installed_from: Option<PathBuf>,
    /// `installed_from` holds a copy rather than an editable source.
    pub copied: bool,
}

/// Where an install resolves a member other than to this tree.
struct Shadow {
    from: PathBuf,
    copied: bool,
}

/// Where the install of `member` among `installs` resolves it other than to
/// this tree: an editable source outside the project, or the site-packages
/// directory holding a copy of a top-level package or module the member
/// holds, wherever that lies; `None` where it resolves the member here, or
/// none installs it.
fn shadowing(root: &Path, member: &Member, installs: &[Install]) -> Option<Shadow> {
    let install = installs
        .iter()
        .find(|install| install.name == member.normalized)?;
    match &install.kind {
        InstallKind::Editable(source) => (!source.starts_with(root)).then(|| Shadow {
            from: source.clone(),
            copied: false,
        }),
        InstallKind::Copy {
            site_packages,
            top_level,
        } => {
            let directory = root.join(&member.directory);
            let held = |top: &String| {
                [directory.join("src"), directory.clone()]
                    .iter()
                    .any(|base| base.join(top).is_dir() || base.join(format!("{top}.py")).is_file())
            };
            top_level.iter().any(held).then(|| Shadow {
                from: site_packages.clone(),
                copied: true,
            })
        }
    }
}

struct Member {
    directory: String,
    name: String,
    normalized: String,
}

impl Member {
    fn of(member: UvMember) -> Self {
        Self {
            normalized: normalized_distribution_name(&member.name),
            directory: member.directory,
            name: member.name,
        }
    }
}

/// The members of the root's uv workspace, and those of them the Python
/// server's environment does not resolve here.
pub struct PythonMembers {
    root: PathBuf,
    members: Vec<Member>,
    gaps: Vec<MemberGap>,
    interpreter: Interpreter,
    /// `$VIRTUAL_ENV`'s interpreter, read where symora hands the server none.
    virtual_env: Option<PathBuf>,
    /// How many members `$VIRTUAL_ENV` installs, from anywhere and either way.
    virtual_env_installs: usize,
}

impl PythonMembers {
    pub fn read(root: &Path, config: &LspRuntimeConfig) -> Self {
        Self::read_with(root, config, virtual_env_python)
    }

    /// [`Self::read`] with `$VIRTUAL_ENV`'s interpreter given, read only where
    /// symora hands the server none.
    fn read_with(
        root: &Path,
        config: &LspRuntimeConfig,
        virtual_env_python: impl FnOnce() -> Option<PathBuf>,
    ) -> Self {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let members = members_of(&root);
        if members.is_empty() {
            return Self::unread(root, members);
        }
        let interpreter = Interpreter::selected(&root, config);
        let names: Vec<String> = members.iter().map(|m| m.normalized.clone()).collect();
        let gap = |member: &Member, reason, shadow: Option<Shadow>| MemberGap {
            name: member.name.clone(),
            path: member.directory.clone(),
            reason,
            copied: shadow.as_ref().is_some_and(|shadow| shadow.copied),
            installed_from: shadow.map(|shadow| shadow.from),
        };
        let (mut virtual_env, mut virtual_env_installs) = (None, 0);
        let gaps = match (interpreter.path(&root), &interpreter) {
            (Some(selected), _) => {
                let installs = installs(&selected, &names);
                members
                    .iter()
                    .filter_map(|member| {
                        let shadow = shadowing(&root, member, &installs)?;
                        Some(gap(member, CoverageReason::MemberShadowed, Some(shadow)))
                    })
                    .collect()
            }
            (None, Interpreter::ServerDefault) => {
                virtual_env = virtual_env_python();
                let installs = virtual_env
                    .as_deref()
                    .map(|interpreter| installs(interpreter, &names))
                    .unwrap_or_default();
                virtual_env_installs = installs.len();
                members
                    .iter()
                    .map(|member| {
                        let shadow = shadowing(&root, member, &installs);
                        gap(member, CoverageReason::MemberUnresolved, shadow)
                    })
                    .collect()
            }
            (None, _) => Vec::new(),
        };
        Self {
            root,
            members,
            gaps,
            interpreter,
            virtual_env,
            virtual_env_installs,
        }
    }

    /// The members alone, with nothing read of any environment.
    fn unread(root: PathBuf, members: Vec<Member>) -> Self {
        Self {
            root,
            members,
            gaps: Vec::new(),
            interpreter: Interpreter::ServerDefault,
            virtual_env: None,
            virtual_env_installs: 0,
        }
    }

    pub fn interpreter(&self) -> &Interpreter {
        &self.interpreter
    }

    pub fn gaps(&self) -> &[MemberGap] {
        &self.gaps
    }

    /// The gap of the member a Python `file` is in, or, for one outside the
    /// project, of the member it is another checkout's copy of
    /// ([`Self::copy_of`]). A member's other languages are other servers'.
    pub fn gap_for(&self, file: &Path) -> Option<&MemberGap> {
        if self.gaps.is_empty() || Language::from_path(file) != Language::Python {
            return None;
        }
        let member = match self.relative(file) {
            Some(relative) => self.holding(&relative)?,
            None => self.copy_of(file)?.0,
        };
        self.gaps.iter().find(|gap| gap.path == member.directory)
    }

    /// `file` relative to the root, where it lies inside it.
    fn relative(&self, file: &Path) -> Option<PathBuf> {
        file.strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                let canonical = file.canonicalize().ok()?;
                Some(canonical.strip_prefix(&self.root).ok()?.to_path_buf())
            })
    }

    /// The innermost member whose directory holds `relative`.
    fn holding(&self, relative: &Path) -> Option<&Member> {
        self.members
            .iter()
            .filter(|member| relative.starts_with(&member.directory))
            .max_by_key(|member| member.directory.len())
    }

    /// The member, and this tree's own file, that `file` outside the project
    /// is another checkout's copy of. The proof is the relative path: the
    /// nearest project enclosing `file` declares the name of one of this
    /// tree's members, and that member holds a file at the path `file` has
    /// in it. A file no project encloses, or one whose nearest project is no
    /// member's, is a copy of nothing here.
    fn copy_of(&self, file: &Path) -> Option<(&Member, PathBuf)> {
        if self.relative(file).is_some() {
            return None;
        }
        let project = file
            .ancestors()
            .skip(1)
            .find(|directory| directory.join("pyproject.toml").is_file())?;
        let manifest: toml::Table = std::fs::read_to_string(project.join("pyproject.toml"))
            .ok()?
            .parse()
            .ok()?;
        let name = normalized_distribution_name(manifest.get("project")?.get("name")?.as_str()?);
        let member = self.members.iter().find(|m| m.normalized == name)?;
        let own = Path::new(&member.directory).join(file.strip_prefix(project).ok()?);
        self.root.join(&own).is_file().then_some((member, own))
    }

    /// What a cross-file answer about the declaration in `file` says of its
    /// member, and the remedy; `None` where the member resolves here.
    pub fn qualifier(&self, file: &Path) -> Option<Qualifier> {
        let gap = self.gap_for(file)?;
        let member = format!("`{}` ({})", gap.name, gap.path);
        // A declaration in another checkout's copy is that copy's answer,
        // not a short one of this tree's.
        let copy = self.relative(file).is_none();
        let fact = match (gap.reason, copy) {
            (CoverageReason::MemberShadowed, false) => format!(
                "{member}, the workspace member this declaration is in, is installed, in the \
                 environment of the interpreter symora hands the Python server ({}), {}, so the \
                 server resolves every import of it to that copy: references and calls \
                 reaching this declaration from other files are found there rather than here, \
                 and this answer is a lower bound that retrying does not raise",
                self.interpreter_named(),
                installed(gap),
            ),
            (CoverageReason::MemberShadowed, true) => format!(
                "{member} is installed, in the environment of the interpreter symora hands the \
                 Python server ({}), {}, so the server resolves every import of it to that copy, \
                 which holds this declaration: this answer is about that copy rather than this \
                 tree's own declaration, and retrying does not change that",
                self.interpreter_named(),
                installed(gap),
            ),
            (_, false) => format!(
                "{member}, the workspace member this declaration is in, resolves wherever the \
                 Python server's own choice of interpreter puts it: symora hands the server \
                 none, and {}, so references and calls reaching this declaration from other \
                 files are not established, and retrying does not establish them",
                match (&gap.installed_from, &self.virtual_env) {
                    (Some(_), Some(venv)) => format!(
                        "$VIRTUAL_ENV ({}), outside this project, installs it {}",
                        environment_of(venv).display(),
                        installed(gap)
                    ),
                    _ => "no interpreter symora can see installs it from this tree".to_string(),
                }
            ),
            (_, true) => format!(
                "{member} resolves wherever the Python server's own choice of interpreter puts \
                 it, symora handing the server none, and here that is another checkout's copy, \
                 which holds this declaration: this answer is about that copy rather than this \
                 tree's own declaration, and retrying does not change that"
            ),
        };
        Some(Qualifier {
            gap: gap.clone(),
            hint: format!("{fact}. {}", self.remedy()),
        })
    }

    /// One sentence for every flagged member among `gaps`, with the remedy:
    /// what `doctor` and `diff-impact` lead with.
    pub fn summary_hint(&self, gaps: &[&MemberGap]) -> Option<String> {
        let first = gaps.first()?;
        let members = match gaps.len() {
            1 => format!("workspace member `{}`", first.name),
            n => format!("{n} workspace members"),
        };
        let fact = match first.reason {
            CoverageReason::MemberShadowed => format!(
                "The environment of the interpreter symora hands the Python server ({}) installs \
                 {members} {}, so the server resolves every import of them to those copies, and \
                 cross-file answers about their declarations (references, callers, callees, \
                 implementations, impact, rename) are lower bounds that retrying does not raise",
                self.interpreter_named(),
                match (
                    gaps.iter().any(|gap| !gap.copied),
                    gaps.iter().any(|gap| gap.copied),
                ) {
                    (true, false) => "from outside this project",
                    (false, _) => "as copies in its site-packages, not editable from this tree",
                    (true, true) => "from outside this project or as copies in its site-packages",
                }
            ),
            _ => format!(
                "symora hands the Python server no interpreter, so it picks its own, and {}: \
                 cross-file answers about the declarations of {members} (references, callers, \
                 callees, implementations, impact, rename) are not established, and retrying \
                 does not establish them",
                match &self.virtual_env {
                    Some(venv) => format!(
                        "$VIRTUAL_ENV ({}) lies outside this project, so it is not handed to the \
                         server; it installs {} of them other than editable from this tree{}",
                        environment_of(venv).display(),
                        gaps.iter()
                            .filter(|gap| gap.installed_from.is_some())
                            .count(),
                        match self.virtual_env_installs {
                            0 => ", and no interpreter symora can see installs them at all",
                            _ => "",
                        }
                    ),
                    None => "no interpreter symora can see installs them at all".to_string(),
                }
            ),
        };
        Some(format!("{fact}. {}", self.remedy()))
    }

    fn interpreter_named(&self) -> String {
        match &self.interpreter {
            Interpreter::Settings(path) => format!(
                "{}, the configured python.pythonPath",
                path.as_str().unwrap_or_default()
            ),
            Interpreter::ProjectEnvironment(path) => path.display().to_string(),
            Interpreter::ServerDefault => String::new(),
        }
    }

    /// What builds an environment that resolves the members here, as the
    /// selection stands: a link to another environment is replaced rather
    /// than synced, which would rewrite that environment's own installs.
    fn remedy(&self) -> String {
        let restart = "then restart the project's server with `symora daemon restart --project`";
        match &self.interpreter {
            Interpreter::Settings(_) => format!(
                "Point lsp.servers.python.settings.python.pythonPath at this tree's own \
                 environment (`uv sync` at the project root builds .venv) or remove it, {restart}."
            ),
            Interpreter::ProjectEnvironment(interpreter) => {
                let venv = environment_of(interpreter);
                let link = std::fs::symlink_metadata(&venv)
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                    .then(|| venv.canonicalize().ok())
                    .flatten()
                    .filter(|target| !target.starts_with(&self.root));
                let named = venv.strip_prefix(&self.root).unwrap_or(&venv).display();
                match link {
                    Some(target) => format!(
                        "{named} is a link to {}, another environment: replace the link, not \
                         its target, with this tree's own (`rm {named} && uv sync` at the project \
                         root), {restart}.",
                        target.display()
                    ),
                    None => format!(
                        "Rebuild this tree's environment so it installs the members from here \
                         (`uv sync` at the project root), {restart}."
                    ),
                }
            }
            Interpreter::ServerDefault => format!(
                "Build this tree's own environment (`uv sync` at the project root), whose .venv \
                 symora then hands the server, {restart}."
            ),
        }
    }
}

/// What [`PythonMembers::qualifier`] gives a surface: the member's gap and
/// the hint stating it with its remedy.
#[derive(Debug, Clone)]
pub struct Qualifier {
    pub gap: MemberGap,
    pub hint: String,
}

impl Qualifier {
    /// The `coverage_gaps` item the answer carries.
    pub fn coverage_gap(&self) -> CoverageGap {
        CoverageGap {
            language: Language::Python.lsp_id().to_string(),
            reason: self.gap.reason.as_str().to_string(),
            message: None,
        }
    }

    /// The refusal of a mutation whose safety check compares against the
    /// declaration's references — rename's edit set, delete's no-references
    /// check, the caller files `--verify-callers` checks — which this member
    /// leaves short in a way that no retry, and no `--accept-unconfirmed`,
    /// settles.
    pub fn refusal(&self, operation: &str) -> OutputError {
        let state = match (self.gap.reason, self.gap.copied) {
            (CoverageReason::MemberShadowed, false) => {
                "is installed into the Python server's environment from outside this project"
            }
            (CoverageReason::MemberShadowed, true) => {
                "is installed into the Python server's environment as a copy, not editable from \
                 this tree"
            }
            _ => "is not known to resolve to this tree in the Python server's environment",
        };
        OutputError::precondition_failed(format!(
            "{operation} refused: its safety check compares against the declaration's \
             references, and `{}` ({}), the workspace member holding it, {state} ({}), so that \
             set is short and retrying does not complete it",
            self.gap.name,
            self.gap.path,
            self.gap.reason.as_str(),
        ))
        .with_hint(self.hint.clone())
    }
}

/// [`PythonMembers::qualifier`] for one declaration's file, read for a
/// Python file alone.
pub fn qualifier(app: &App, file: &Path) -> Option<Qualifier> {
    qualifier_among(app, [file])
}

/// The qualifier of the first of `files` that has one: an edit set's, whose
/// files any member can hold. The environment is read once, and only where
/// one of them is Python.
pub fn qualifier_among<'a>(
    app: &App,
    files: impl IntoIterator<Item = &'a Path>,
) -> Option<Qualifier> {
    let files: Vec<&Path> = files
        .into_iter()
        .filter(|file| Language::from_path(file) == Language::Python)
        .collect();
    if files.is_empty() {
        return None;
    }
    let members = PythonMembers::read(app.root(), &LspRuntimeConfig::from(app.config()));
    files.into_iter().find_map(|file| members.qualifier(file))
}

/// The tree's own file, relative to `root`, that a Python `file` outside it
/// is another checkout's copy of ([`PythonMembers::copy_of`]).
pub fn tree_copy_of(root: &Path, file: &Path) -> Option<PathBuf> {
    if Language::from_path(file) != Language::Python || file.starts_with(root) {
        return None;
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let members = members_of(&root);
    let members = PythonMembers::unread(root, members);
    members.copy_of(file).map(|(_, own)| own)
}

/// The members of the root's uv workspace other than the root's own project.
fn members_of(root: &Path) -> Vec<Member> {
    uv_members(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|member| !member.directory.is_empty())
        .map(Member::of)
        .collect()
}

/// How a gap's environment installs its member, `installed_from` named.
fn installed(gap: &MemberGap) -> String {
    let from = display(gap.installed_from.as_deref());
    match gap.copied {
        true => format!("as a copy in {from}, not editable from this tree"),
        false => format!("from {from}, outside this project"),
    }
}

/// The environment directory an interpreter at `<env>/bin/python` runs in.
fn environment_of(interpreter: &Path) -> PathBuf {
    interpreter
        .parent()
        .and_then(Path::parent)
        .unwrap_or(interpreter)
        .to_path_buf()
}

fn display(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::cli::errors::ErrorCode;

    /// A uv workspace at `root` listing `packages/*`, whose root names a
    /// project of its own, with a member per `(directory, project name)`.
    fn workspace(root: &Path, members: &[(&str, &str)]) {
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"ws\"\n\n[tool.uv.workspace]\nmembers = [\"packages/*\"]\n",
        )
        .unwrap();
        for (directory, name) in members {
            let directory = root.join(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("pyproject.toml"),
                format!("[project]\nname = \"{name}\"\n"),
            )
            .unwrap();
        }
    }

    /// An environment at `venv` with an executable interpreter whose
    /// site-packages records each `(name, source)` as installed editable
    /// from `source`; the interpreter's path.
    fn environment(venv: &Path, installs: &[(&str, &Path)]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(venv.join("bin")).unwrap();
        let interpreter = venv.join("bin/python");
        std::fs::write(&interpreter, "").unwrap();
        std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let site = venv.join("lib/python3.12/site-packages");
        for (name, source) in installs {
            let record = site.join(format!("{}-0.1.0.dist-info", name.replace('-', "_")));
            std::fs::create_dir_all(&record).unwrap();
            std::fs::write(record.join("METADATA"), format!("Name: {name}\n")).unwrap();
            std::fs::write(
                record.join("direct_url.json"),
                format!(
                    r#"{{"url": "file://{}", "dir_info": {{"editable": true}}}}"#,
                    source.display()
                ),
            )
            .unwrap();
        }
        interpreter
    }

    fn read(root: &Path, virtual_env: Option<PathBuf>) -> PythonMembers {
        PythonMembers::read_with(root, &LspRuntimeConfig::default(), || virtual_env)
    }

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    fn summary(members: &PythonMembers) -> Option<String> {
        members.summary_hint(&members.gaps().iter().collect::<Vec<_>>())
    }

    /// A member the project environment installs from another checkout is
    /// shadowed, for its Python files alone; one it installs from this tree
    /// is not, nor is the root's own project, whose modules the server
    /// resolves under its root whatever installs it.
    #[test]
    fn a_member_installed_from_outside_the_project_is_shadowed() {
        let tree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        workspace(
            tree.path(),
            &[("packages/core", "Core_Lib"), ("packages/app", "app")],
        );
        std::fs::create_dir_all(other.path().join("packages/core")).unwrap();
        environment(
            &tree.path().join(".venv"),
            &[
                ("core-lib", &other.path().join("packages/core")),
                ("app", &tree.path().join("packages/app")),
                ("ws", other.path()),
            ],
        );

        let members = read(tree.path(), None);
        assert_eq!(
            members.gaps(),
            [MemberGap {
                name: "Core_Lib".to_string(),
                path: "packages/core".to_string(),
                reason: CoverageReason::MemberShadowed,
                installed_from: Some(canonical(&other.path().join("packages/core"))),
                copied: false,
            }]
        );
        let root = canonical(tree.path());
        let file = |relative: &str| root.join(relative);
        assert!(
            members
                .gap_for(&file("packages/core/src/core_lib/x.py"))
                .is_some()
        );
        assert!(members.gap_for(&file("packages/core/web/x.ts")).is_none());
        assert!(
            members
                .gap_for(&file("packages/app/src/app/x.py"))
                .is_none()
        );
        assert!(members.gap_for(&file("scripts/x.py")).is_none());

        let qualifier = members
            .qualifier(&file("packages/core/src/core_lib/x.py"))
            .unwrap();
        assert_eq!(qualifier.coverage_gap().reason, "member_shadowed");
        assert!(qualifier.hint.contains("`uv sync`"), "{}", qualifier.hint);
        assert!(!qualifier.hint.contains("rm .venv"), "{}", qualifier.hint);
        let refused = qualifier.refusal("Rename");
        assert_eq!(refused.code, ErrorCode::PreconditionFailed);
        assert!(
            refused.message.contains("(member_shadowed)"),
            "{}",
            refused.message
        );
    }

    /// A member installed as a copy — no `direct_url.json`, or one recording
    /// no editable install — whose `RECORD` puts a package the member holds
    /// into site-packages is shadowed by that copy, even in the tree's own
    /// environment; one whose copy installs no package the member holds is
    /// not.
    #[test]
    fn a_member_installed_as_a_copy_of_its_package_is_shadowed() {
        let tree = tempfile::tempdir().unwrap();
        workspace(
            tree.path(),
            &[("packages/core", "core"), ("packages/app", "app")],
        );
        for held in ["packages/core/src/core", "packages/app/src/app"] {
            std::fs::create_dir_all(tree.path().join(held)).unwrap();
        }
        environment(&tree.path().join(".venv"), &[]);
        let site = tree.path().join(".venv/lib/python3.12/site-packages");
        for (name, direct_url, files) in [
            (
                "core",
                None,
                "core/__init__.py,sha256=x,1\ncore/errors.py,,\n",
            ),
            (
                "app",
                Some(r#"{"url": "file:///w", "dir_info": {}}"#),
                "unrelated/__init__.py,,\n",
            ),
        ] {
            let record = site.join(format!("{name}-0.1.0.dist-info"));
            std::fs::create_dir_all(&record).unwrap();
            std::fs::write(record.join("METADATA"), format!("Name: {name}\n")).unwrap();
            std::fs::write(record.join("RECORD"), files).unwrap();
            if let Some(direct_url) = direct_url {
                std::fs::write(record.join("direct_url.json"), direct_url).unwrap();
            }
        }

        let members = read(tree.path(), None);
        assert_eq!(
            members.gaps(),
            [MemberGap {
                name: "core".to_string(),
                path: "packages/core".to_string(),
                reason: CoverageReason::MemberShadowed,
                installed_from: Some(canonical(&site)),
                copied: true,
            }]
        );
        let qualifier = members
            .qualifier(&canonical(tree.path()).join("packages/core/src/core/errors.py"))
            .unwrap();
        assert!(
            qualifier.hint.contains("as a copy in"),
            "{}",
            qualifier.hint
        );
        assert!(
            qualifier.refusal("Rename").message.contains("as a copy"),
            "{}",
            qualifier.hint
        );
        let hint = summary(&members).unwrap();
        assert!(hint.contains("as copies in its site-packages"), "{hint}");
    }

    /// A `.venv` that links to another checkout's environment is replaced,
    /// never synced in place: `uv sync` through the link would rewrite that
    /// environment's own installs.
    #[test]
    fn a_linked_environment_is_replaced_rather_than_synced() {
        let tree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        workspace(tree.path(), &[("packages/core", "core")]);
        std::fs::create_dir_all(other.path().join("packages/core")).unwrap();
        environment(
            &other.path().join(".venv"),
            &[("core", &other.path().join("packages/core"))],
        );
        std::os::unix::fs::symlink(other.path().join(".venv"), tree.path().join(".venv")).unwrap();

        let hint = summary(&read(tree.path(), None)).expect("the linked environment shadows");
        assert!(hint.contains("`rm .venv && uv sync`"), "{hint}");
    }

    /// With no interpreter handed over, every member is unresolved, the
    /// root's own project aside; `$VIRTUAL_ENV`'s installs from outside the
    /// project are named as evidence, never taken for the server's choice.
    #[test]
    fn without_a_selected_interpreter_every_member_is_unresolved() {
        let tree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        workspace(
            tree.path(),
            &[("packages/core", "core"), ("packages/app", "app")],
        );
        std::fs::create_dir_all(other.path().join("packages/core")).unwrap();
        let elsewhere = environment(
            &other.path().join(".venv"),
            &[("core", &other.path().join("packages/core"))],
        );

        let unset = read(tree.path(), None);
        assert_eq!(unset.interpreter(), &Interpreter::ServerDefault);
        assert_eq!(unset.gaps().len(), 2);
        assert!(unset.gaps().iter().all(|gap| {
            gap.reason == CoverageReason::MemberUnresolved && gap.installed_from.is_none()
        }));
        let hint = summary(&unset).unwrap();
        assert!(
            hint.contains("no interpreter symora can see installs them"),
            "{hint}"
        );

        let evidence = read(tree.path(), Some(elsewhere));
        let core = canonical(&other.path().join("packages/core"));
        let from: Vec<Option<&Path>> = evidence
            .gaps()
            .iter()
            .map(|gap| gap.installed_from.as_deref())
            .collect();
        assert_eq!(from, [None, Some(core.as_path())]);
        assert!(
            evidence
                .gaps()
                .iter()
                .all(|gap| gap.reason == CoverageReason::MemberUnresolved)
        );
        let hint = summary(&evidence).unwrap();
        assert!(
            hint.contains("installs 1 of them other than editable from this tree"),
            "{hint}"
        );
    }

    /// The tree's environment installing every member from the tree is the
    /// control: nothing is flagged; and no workspace means no members.
    #[test]
    fn members_installed_from_the_tree_resolve_here() {
        let tree = tempfile::tempdir().unwrap();
        workspace(tree.path(), &[("packages/core", "core")]);
        environment(
            &tree.path().join(".venv"),
            &[("core", &tree.path().join("packages/core"))],
        );
        assert!(read(tree.path(), None).gaps().is_empty());

        let plain = tempfile::tempdir().unwrap();
        assert!(read(plain.path(), None).gaps().is_empty());
    }

    /// A file outside the project is another checkout's copy of this tree's
    /// where its nearest project names a member and the member holds a file
    /// at its path there; it then carries that member's gap.
    #[test]
    fn an_outside_file_is_a_copy_where_its_relative_path_proves_it() {
        let tree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        workspace(tree.path(), &[("packages/core", "core-lib")]);
        workspace(
            other.path(),
            &[("packages/core", "Core.Lib"), ("packages/x", "x")],
        );
        let relative = "src/core_lib/errors.py";
        for root in [tree.path(), other.path()] {
            let file = root.join("packages/core").join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "").unwrap();
        }
        std::fs::create_dir_all(other.path().join("packages/x/src")).unwrap();

        let copy = canonical(&other.path().join("packages/core").join(relative));
        assert_eq!(
            tree_copy_of(tree.path(), &copy),
            Some(Path::new("packages/core").join(relative))
        );
        let absent = other.path().join("packages/core/src/core_lib/new.py");
        assert_eq!(tree_copy_of(tree.path(), &absent), None);
        let unrelated = other.path().join("packages/x/src/errors.py");
        assert_eq!(tree_copy_of(tree.path(), &unrelated), None);

        let members = read(tree.path(), None);
        assert_eq!(
            members.gap_for(&copy).map(|gap| gap.path.as_str()),
            Some("packages/core")
        );
    }
}
