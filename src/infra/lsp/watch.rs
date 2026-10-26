//! The client half of `workspace/didChangeWatchedFiles`: servers register the
//! files they care about, and changes on disk under the workspace are
//! forwarded to them. A server that registers watchers stops watching the
//! disk itself, so without this half it answers from the tree it saw at
//! startup.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex_automata::Anchored;
use regex_automata::hybrid::dfa::DFA;
use regex_automata::util::{start, syntax};
use tokio::sync::mpsc;

use super::protocol::{
    DidChangeWatchedFilesRegistrationOptions, FileChangeType, FileEvent, GlobPattern, Registration,
};
use crate::models::lsp::{path_to_uri, uri_to_path};

pub const WATCHED_FILES_METHOD: &str = "workspace/didChangeWatchedFiles";

/// Whether a manager keeps the servers it starts current with the disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileWatch {
    /// Its owner answers until stopped — the daemon, `mcp serve` — so a file
    /// that changes on disk meanwhile has to reach the servers.
    On,
    /// Its owner answers one command and exits. Its servers read the disk as
    /// it is, and a watch would cost a walk of the whole tree for nothing.
    /// They are still offered file watching, so none watches the tree itself
    /// for changes the one answer could not use: a server not offered it
    /// starts its own watcher, which held rust-analyzer's readiness past the
    /// indexing wait a command allows.
    #[default]
    Off,
}

/// Paths no server is told about: VCS object stores (the reference client's
/// default exclusions — a `**` watcher would otherwise receive every object a
/// git operation writes) and symora's own state directory.
const EXCLUDED: &[&[&str]] = &[
    &[".git", "objects"],
    &[".git", "subtree-cache"],
    &[".hg", "store"],
];
const STATE_DIR: &str = ".symora";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub change: FileChangeType,
    pub kind: PathKind,
}

impl FileChange {
    /// Whether this is a directory gone from the tree, or a path gone that
    /// might have been one. The files it held went with it unreported: a
    /// rename or a move out of the tree reports the directory alone.
    fn may_have_held_files(&self) -> bool {
        self.change == FileChangeType::Deleted && self.kind != PathKind::File
    }
}

/// What a changed path is, or was until it went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    File,
    Directory,
    /// Gone, and nothing said which it was: notify reports a rename as
    /// neither, and on Windows any removal, and what left a path that a
    /// directory took in the same batch is not told apart from that
    /// directory. On Linux the directories the watch keeps settle it
    /// (`WorkspaceWatcher::follow`); elsewhere it is taken for a directory,
    /// so a file renamed away may reach a watcher for paths below it, which
    /// finds nothing there.
    Unknown,
}

/// The file watchers one server has registered, by registration id.
#[derive(Default)]
pub struct WatchRegistry {
    registrations: HashMap<String, Vec<FileWatcher>>,
}

struct FileWatcher {
    /// Set for a relative pattern, which matches paths below this base.
    base: Option<PathBuf>,
    glob: GlobMatcher,
    /// The glob as an automaton read a byte at a time, which says whether
    /// a path that begins a certain way can still match.
    prefixes: DFA,
    kinds: u8,
}

impl FileWatcher {
    /// Whether the watcher asked for `change`. A directory that went away
    /// is reported as itself, as the protocol and the reference client
    /// report one, so it goes to a watcher that matches it or a path it
    /// could have held. Its files are not listed — they are no longer on
    /// disk to list. A server that tracks files below a directory drops
    /// them on hearing it went away; one that reads each event as a single
    /// path keeps them, as it does under the reference client.
    fn wants(&self, change: &FileChange) -> bool {
        if self.kinds & change.change.watch_kind() == 0 {
            return false;
        }
        let path = match &self.base {
            Some(base) => match change.path.strip_prefix(base) {
                Ok(relative) => relative,
                // The base itself went with a directory above it.
                Err(_) => return change.may_have_held_files() && base.starts_with(&change.path),
            },
            None => &change.path,
        };
        self.glob.is_match(path) || (change.may_have_held_files() && self.matches_below(path))
    }

    /// Whether the glob matches some path below `dir`: whether, having read
    /// `dir` and a separator, it can still reach a match.
    fn matches_below(&self, dir: &Path) -> bool {
        let mut prefix = glob_bytes(dir);
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let mut cache = self.prefixes.create_cache();
        let anchored = start::Config::new().anchored(Anchored::Yes);
        let state = self
            .prefixes
            .start_state(&mut cache, &anchored)
            .ok()
            .and_then(|state| {
                prefix.iter().try_fold(state, |state, &byte| {
                    self.prefixes.next_state(&mut cache, state, byte).ok()
                })
            });
        // An automaton that gave up has not ruled the directory out.
        state.is_none_or(|state| !state.is_dead())
    }
}

/// A path as a glob reads it: its bytes, with `/` between components.
#[cfg(unix)]
fn glob_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn glob_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}

impl WatchRegistry {
    /// Compile the complete registration batch before changing any active watcher.
    pub fn register(&mut self, registrations: &[Registration]) -> Result<(), String> {
        let compiled = registrations
            .iter()
            .map(|registration| {
                let options: DidChangeWatchedFilesRegistrationOptions = serde_json::from_value(
                    registration.register_options.clone().unwrap_or_default(),
                )
                .map_err(|error| {
                    format!("Invalid watcher registration {}: {error}", registration.id)
                })?;
                let watchers = options
                    .watchers
                    .into_iter()
                    .map(|watcher| {
                        let (base, pattern) = match watcher.glob_pattern {
                            GlobPattern::Absolute(pattern) => (None, pattern),
                            GlobPattern::Relative(relative) => {
                                (Some(uri_to_path(relative.base_uri.uri())), relative.pattern)
                            }
                        };
                        let invalid = |error: &dyn std::fmt::Display| {
                            format!("Invalid watcher glob {pattern:?}: {error}")
                        };
                        let glob = GlobBuilder::new(&pattern)
                            .literal_separator(true)
                            .build()
                            .map_err(|error| invalid(&error))?;
                        // The syntax globset compiles its own regex with.
                        let prefixes = DFA::builder()
                            .syntax(syntax::Config::new().utf8(false).dot_matches_new_line(true))
                            .build(glob.regex())
                            .map_err(|error| invalid(&error))?;
                        Ok(FileWatcher {
                            base,
                            glob: glob.compile_matcher(),
                            prefixes,
                            kinds: watcher.kind.unwrap_or(7),
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok((registration.id.clone(), watchers))
            })
            .collect::<Result<HashMap<_, _>, String>>()?;
        self.registrations.extend(compiled);
        Ok(())
    }

    pub fn unregister(&mut self, id: &str) {
        self.registrations.remove(id);
    }

    /// The changes some registered watcher asked for, as protocol events.
    pub fn select(&self, changes: &[FileChange]) -> Vec<FileEvent> {
        changes
            .iter()
            .filter(|change| {
                self.registrations
                    .values()
                    .flatten()
                    .any(|watcher| watcher.wants(change))
            })
            .map(|change| FileEvent {
                uri: path_to_uri(&change.path),
                change: change.change,
            })
            .collect()
    }
}

/// One batch of watcher output, reduced to what a server must be told.
#[derive(Debug, PartialEq, Eq)]
pub enum Batch {
    Changes(Vec<FileChange>),
    /// The watcher dropped events, so no list of changes is complete.
    Rescan,
}

/// A watch on the workspace tree. Dropping it stops the watch and closes the
/// event channel.
pub struct WorkspaceWatcher {
    watcher: RecommendedWatcher,
    /// inotify watches one directory at a time, and notify's recursive watch
    /// fails as a whole on the first directory it cannot watch — one the
    /// user cannot read, or one past the per-user watch limit. On Linux the
    /// tree is therefore watched a directory at a time here, skipping those.
    #[cfg(target_os = "linux")]
    watched: std::collections::BTreeSet<PathBuf>,
    #[cfg(target_os = "linux")]
    warned: bool,
}

pub type RawEvent = notify::Result<notify::Event>;

impl WorkspaceWatcher {
    /// Watch `root`, returning the watcher, the stream of raw events, and the
    /// path the events are reported under (the root with symlinks resolved).
    pub fn start(
        root: &Path,
    ) -> notify::Result<(Self, mpsc::UnboundedReceiver<RawEvent>, PathBuf)> {
        let watch_root = root.canonicalize()?;
        let (sender, events) = mpsc::unbounded_channel();
        let watcher = RecommendedWatcher::new(
            move |event: RawEvent| {
                let _ = sender.send(event);
            },
            // A link out of the project would widen the watch to whatever it
            // points at; the tree the servers were given is the project's.
            notify::Config::default().with_follow_symlinks(false),
        )?;
        let mut this = Self {
            watcher,
            #[cfg(target_os = "linux")]
            watched: std::collections::BTreeSet::new(),
            #[cfg(target_os = "linux")]
            warned: false,
        };
        this.watch_root(&watch_root)?;
        Ok((this, events, watch_root))
    }

    /// Watch the tree afresh once events were lost. On Linux the watch
    /// follows the tree through its own events, so a loss leaves it without
    /// the directories created meanwhile, and a directory renamed meanwhile
    /// still watched under its old path; elsewhere one recursive watch
    /// covers both.
    pub fn rewatch(&mut self, watch_root: &Path) -> notify::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // Paths that are gone are unwatched before the walk: a renamed
            // directory's one watch is named by its old path and, once the
            // walk adds it, its new one, and unwatching the old path after
            // that would take the new path's watch with it.
            let gone: Vec<PathBuf> = self
                .watched
                .iter()
                .filter(|path| !path.symlink_metadata().is_ok_and(|meta| meta.is_dir()))
                .cloned()
                .collect();
            for path in gone {
                let _ = self.watcher.unwatch(&path);
            }
            // Every directory is added again, not only those missing: one
            // replaced meanwhile is a new directory at a watched path. Adding
            // a directory already watched takes no second watch.
            self.watched.clear();
            self.warned = false;
            self.watch_root(watch_root)?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = watch_root;
        Ok(())
    }

    /// Keep the watch over the tree `changes` describe, and add to them the
    /// files inside each directory that appeared. A directory moved in, or
    /// created before a watch covered it, is reported as the directory
    /// alone, and a server that registered for its files would not hear of
    /// them. Its watch is extended first and its files listed after, so a
    /// file written in between is seen by one or the other. A path that
    /// went away with no event saying what it was is settled by the
    /// directories the watch keeps, where it keeps them, and a file created
    /// where the watch kept a directory is reported to have replaced it.
    pub fn follow(&mut self, changes: &mut Vec<FileChange>, watch_root: &Path, root: &Path) {
        let mut reported: HashSet<(PathBuf, FileChangeType)> =
            changes.iter().map(|c| (c.path.clone(), c.change)).collect();
        let mut followed = Vec::with_capacity(changes.len());
        let mut found = Vec::new();
        for mut change in std::mem::take(changes) {
            let Ok(relative) = change.path.strip_prefix(root) else {
                followed.push(change);
                continue;
            };
            let path = watch_root.join(relative);
            if change.change == FileChangeType::Deleted {
                let watched = self.retract(&path);
                if let (PathKind::Unknown, Some(watched)) = (change.kind, watched) {
                    change.kind = if watched {
                        PathKind::Directory
                    } else {
                        PathKind::File
                    };
                }
            } else if change.change == FileChangeType::Created
                && path.symlink_metadata().is_ok_and(|meta| meta.is_dir())
            {
                // A directory created where one was removed is a new one.
                self.retract(&path);
                self.extend(&path, watch_root);
                for file in files_under(&path, watch_root) {
                    let path = root.join(file.strip_prefix(watch_root).unwrap_or(&file));
                    for change in [FileChangeType::Created, FileChangeType::Changed] {
                        if reported.insert((path.clone(), change)) {
                            found.push(FileChange {
                                path: path.clone(),
                                change,
                                kind: PathKind::File,
                            });
                        }
                    }
                }
            } else if change.change == FileChangeType::Created && self.retract(&path) == Some(true)
            {
                // The watch kept a directory where a file is now, and has let
                // it go: one renamed away, of which inotify says neither that
                // it was a directory nor that it held files.
                followed.push(FileChange {
                    path: change.path.clone(),
                    change: FileChangeType::Deleted,
                    kind: PathKind::Directory,
                });
            }
            followed.push(change);
        }
        followed.extend(found);
        *changes = followed;
    }

    #[cfg(not(target_os = "linux"))]
    fn watch_root(&mut self, watch_root: &Path) -> notify::Result<()> {
        self.watcher.watch(watch_root, RecursiveMode::Recursive)
    }

    #[cfg(not(target_os = "linux"))]
    fn extend(&mut self, _dir: &Path, _watch_root: &Path) {}

    /// A recursive watch keeps no list of directories, so it cannot say
    /// whether a path that went away was one.
    #[cfg(not(target_os = "linux"))]
    fn retract(&mut self, _dir: &Path) -> Option<bool> {
        None
    }

    #[cfg(target_os = "linux")]
    fn watch_root(&mut self, watch_root: &Path) -> notify::Result<()> {
        self.watcher
            .watch(watch_root, RecursiveMode::NonRecursive)?;
        self.watched.insert(watch_root.to_path_buf());
        for dir in subdirectories(watch_root, watch_root) {
            self.extend(&dir, watch_root);
        }
        Ok(())
    }

    /// Watch `top` and every directory below it that can be watched. One
    /// that cannot is skipped — its changes reach no server — and said once
    /// per watch.
    #[cfg(target_os = "linux")]
    fn extend(&mut self, top: &Path, watch_root: &Path) {
        let mut pending = vec![top.to_path_buf()];
        while let Some(dir) = pending.pop() {
            if !self.watched.contains(&dir) {
                if let Err(e) = self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    if !std::mem::replace(&mut self.warned, true) {
                        tracing::warn!(
                            "Cannot watch every directory under {} ({e}); language servers \
                             will not see files change in those",
                            watch_root.display()
                        );
                    }
                    continue;
                }
                self.watched.insert(dir.clone());
            }
            pending.extend(subdirectories(&dir, watch_root));
        }
    }

    /// Drop the watches at and below `dir`, which is gone from that path,
    /// and say whether it was a directory the watch covered: each has a
    /// watch of its own.
    #[cfg(target_os = "linux")]
    fn retract(&mut self, dir: &Path) -> Option<bool> {
        let was_directory = self.watched.contains(dir);
        let stale: Vec<PathBuf> = self
            .watched
            .range(dir.to_path_buf()..)
            .take_while(|path| path.starts_with(dir))
            .cloned()
            .collect();
        for path in stale {
            let _ = self.watcher.unwatch(&path);
            self.watched.remove(&path);
        }
        Some(was_directory)
    }
}

/// The directories directly in `dir` a watch covers: not through a link,
/// and not into a store no server is told about.
#[cfg(target_os = "linux")]
fn subdirectories(dir: &Path, watch_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| !path.strip_prefix(watch_root).is_ok_and(is_excluded))
        .collect()
}

/// Every file below `dir`, not through a link and not into an excluded store.
fn files_under(dir: &Path, watch_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.strip_prefix(watch_root).is_ok_and(is_excluded) {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(_) => files.push(path),
                Err(_) => {}
            }
        }
    }
    files
}

#[derive(Default, Clone, Copy)]
struct Seen {
    appeared: bool,
    modified: bool,
    /// An event took what was at the path away: removed it, or renamed it
    /// elsewhere or without saying which way.
    left: bool,
    /// An event created or removed a directory at the path.
    folder: bool,
    /// An event created or removed a file at the path.
    file: bool,
}

impl Seen {
    /// What a path that went away was, by the events that said.
    fn kind(&self) -> PathKind {
        match (self.folder, self.file) {
            (true, false) => PathKind::Directory,
            (false, true) => PathKind::File,
            _ => PathKind::Unknown,
        }
    }

    /// What left a path that is `now` there after the batch, when it may
    /// have held files the path no longer holds: whatever left a path that
    /// is a directory now, which the events do not tell apart from that
    /// directory, or a directory whose path a file took.
    fn replaced(&self, now: PathKind) -> Option<PathKind> {
        if !self.left {
            None
        } else if now == PathKind::Directory {
            Some(PathKind::Unknown)
        } else if self.folder {
            Some(PathKind::Directory)
        } else {
            None
        }
    }
}

/// Reduce raw events under `watch_root` to protocol changes reported under
/// `root`, the same directory as the servers were given it.
///
/// Event kinds are not trusted to say what a path is now: FSEvents merges
/// every flag a path collected into one event, and a rename does not say
/// whether its target existed. Each touched path is therefore judged by
/// whether it exists once the batch is read. One that exists after being
/// created, renamed onto, or removed and recreated is reported as both
/// created and changed, so a server learns of it whether or not it was
/// already tracking that path. One that something left and a directory
/// took, or that the events say a directory left and a file took, is
/// reported deleted first: what left may have held files the path no
/// longer holds, and the files it holds now are reported created
/// (`WorkspaceWatcher::follow`).
/// A rename that does not say which way it went — FSEvents reports both of
/// its ends alike — is taken to have left the path, so a directory merely
/// renamed onto one is reported deleted as well, which costs the server a
/// look below a path that held nothing.
pub fn reduce(events: Vec<RawEvent>, watch_root: &Path, root: &Path) -> Batch {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut seen: HashMap<PathBuf, Seen> = HashMap::new();

    for event in events {
        // An error is a path the watch could not cover, not an event it
        // lost: what it does cover is still reported in full.
        let event = match event {
            Ok(event) if !event.need_rescan() => event,
            Ok(_) => return Batch::Rescan,
            Err(e) => {
                tracing::warn!("File watcher error: {e}");
                continue;
            }
        };
        let mark: fn(&mut Seen) = match event.kind {
            // The end a rename left, which a `Both` names first, is also
            // reported in a `From` of its own.
            EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both)) => {
                |s| s.appeared = true
            }
            EventKind::Modify(ModifyKind::Name(_)) => |s| {
                s.appeared = true;
                s.left = true;
            },
            EventKind::Remove(_) => |s| s.left = true,
            EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Access(_)
            | EventKind::Other => {
                continue;
            }
            EventKind::Modify(_) | EventKind::Any => |s| s.modified = true,
        };
        let said: fn(&mut Seen) = match event.kind {
            EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
                |s| s.folder = true
            }
            EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => {
                |s| s.file = true
            }
            _ => |_| {},
        };
        for path in event.paths {
            let Ok(relative) = path.strip_prefix(watch_root) else {
                continue;
            };
            if is_excluded(relative) {
                continue;
            }
            let relative = relative.to_path_buf();
            let entry = seen.entry(relative.clone()).or_insert_with(|| {
                order.push(relative);
                Seen::default()
            });
            mark(entry);
            said(entry);
        }
    }

    let mut changes = Vec::new();
    for relative in order {
        let touched = seen[&relative];
        let path = root.join(&relative);
        match watch_root.join(&relative).symlink_metadata() {
            Ok(meta) => {
                let kind = if meta.is_dir() {
                    PathKind::Directory
                } else {
                    PathKind::File
                };
                if let Some(gone) = touched.replaced(kind) {
                    changes.push(FileChange {
                        path: path.clone(),
                        change: FileChangeType::Deleted,
                        kind: gone,
                    });
                }
                if touched.appeared || touched.left {
                    changes.push(FileChange {
                        path: path.clone(),
                        change: FileChangeType::Created,
                        kind,
                    });
                }
                if kind == PathKind::File {
                    changes.push(FileChange {
                        path,
                        change: FileChangeType::Changed,
                        kind,
                    });
                }
            }
            Err(_) => changes.push(FileChange {
                path,
                change: FileChangeType::Deleted,
                kind: touched.kind(),
            }),
        }
    }
    Batch::Changes(changes)
}

fn is_excluded(relative: &Path) -> bool {
    let parts: Vec<&OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    parts.first().is_some_and(|first| *first == STATE_DIR)
        || EXCLUDED.iter().any(|excluded| {
            parts
                .windows(excluded.len())
                .any(|window| window == *excluded)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};

    fn registry(options: serde_json::Value) -> WatchRegistry {
        let mut registry = WatchRegistry::default();
        registry
            .register(&[Registration {
                id: "r1".to_string(),
                method: WATCHED_FILES_METHOD.to_string(),
                register_options: Some(options),
            }])
            .unwrap();
        registry
    }

    fn change(path: &str, change: FileChangeType) -> FileChange {
        FileChange {
            path: PathBuf::from(path),
            change,
            kind: PathKind::File,
        }
    }

    fn selected(registry: &WatchRegistry, changes: &[FileChange]) -> Vec<String> {
        registry
            .select(changes)
            .into_iter()
            .map(|event| format!("{}:{:?}", event.uri, event.change))
            .collect()
    }

    #[test]
    fn a_string_glob_matches_the_absolute_path() {
        let registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**/pyrightconfig.json" }]
        }));
        assert_eq!(
            selected(
                &registry,
                &[
                    change("/w/sub/pyrightconfig.json", FileChangeType::Changed),
                    change("/w/sub/pyrightconfig.json.bak", FileChangeType::Changed),
                ]
            ),
            ["file:///w/sub/pyrightconfig.json:Changed"]
        );
    }

    #[test]
    fn a_relative_pattern_matches_below_its_base_only() {
        let registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": { "baseUri": "file:///w/src", "pattern": "*.py" } }]
        }));
        assert_eq!(
            selected(
                &registry,
                &[
                    change("/w/src/a.py", FileChangeType::Created),
                    change("/w/src/deep/b.py", FileChangeType::Created),
                    change("/w/other/a.py", FileChangeType::Created),
                ]
            ),
            ["file:///w/src/a.py:Created"]
        );
    }

    #[test]
    fn a_watcher_receives_only_the_kinds_it_asked_for() {
        let registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**", "kind": 5 }]
        }));
        assert_eq!(
            selected(
                &registry,
                &[
                    change("/w/a.py", FileChangeType::Created),
                    change("/w/a.py", FileChangeType::Changed),
                    change("/w/b.py", FileChangeType::Deleted),
                ]
            ),
            ["file:///w/a.py:Created", "file:///w/b.py:Deleted"]
        );
    }

    #[test]
    fn an_unregistered_watcher_stops_matching() {
        let mut registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**" }]
        }));
        registry.unregister("r1");
        assert!(
            registry
                .select(&[change("/w/a.py", FileChangeType::Changed)])
                .is_empty()
        );
    }

    #[test]
    fn invalid_registration_batches_preserve_existing_watchers() {
        let mut registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**/*.go" }]
        }));
        let valid = Registration {
            id: "python".to_string(),
            method: WATCHED_FILES_METHOD.to_string(),
            register_options: Some(serde_json::json!({
                "watchers": [{ "globPattern": "**/*.py" }]
            })),
        };
        let invalid = Registration {
            id: "invalid".to_string(),
            method: WATCHED_FILES_METHOD.to_string(),
            register_options: Some(serde_json::json!({
                "watchers": [{ "globPattern": "[" }]
            })),
        };
        assert!(registry.register(&[valid, invalid]).is_err());
        assert_eq!(
            selected(
                &registry,
                &[
                    change("/w/main.go", FileChangeType::Changed),
                    change("/w/main.py", FileChangeType::Changed),
                ]
            ),
            ["file:///w/main.go:Changed"]
        );
    }

    fn gone(path: &str, kind: PathKind) -> FileChange {
        FileChange {
            path: PathBuf::from(path),
            change: FileChangeType::Deleted,
            kind,
        }
    }

    #[test]
    fn a_directory_that_went_away_reaches_a_watcher_for_the_files_it_held() {
        let registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**/*.go" }]
        }));
        assert_eq!(
            selected(
                &registry,
                &[
                    gone("/w/pkg", PathKind::Directory),
                    gone("/w/renamed", PathKind::Unknown),
                    gone("/w/notes.txt", PathKind::File),
                    // One that appeared brings its files instead (`follow`).
                    FileChange {
                        path: PathBuf::from("/w/new"),
                        change: FileChangeType::Created,
                        kind: PathKind::Directory,
                    },
                ]
            ),
            ["file:///w/pkg:Deleted", "file:///w/renamed:Deleted"]
        );
    }

    #[test]
    fn a_directory_that_went_away_reaches_only_a_watcher_that_could_match_below_it() {
        use PathKind::Directory;
        let registry = registry(serde_json::json!({
            "watchers": [
                { "globPattern": { "baseUri": "file:///w/src", "pattern": "*.py" } },
                { "globPattern": "/v/lib/*/mod.rs" },
            ]
        }));
        assert_eq!(
            selected(
                &registry,
                &[
                    gone("/w/src/deep", Directory),
                    gone("/w/src", Directory),
                    gone("/w", Directory),
                    gone("/w/other", Directory),
                    gone("/v/lib/a", Directory),
                    gone("/v/lib/a/b", Directory),
                    gone("/v", Directory),
                ]
            ),
            [
                "file:///w/src:Deleted",
                "file:///w:Deleted",
                "file:///v/lib/a:Deleted",
                "file:///v:Deleted",
            ]
        );
    }

    fn event(kind: EventKind, path: &Path) -> RawEvent {
        Ok(notify::Event::new(kind).add_path(path.to_path_buf()))
    }

    fn reduced(events: Vec<RawEvent>, root: &Path) -> Vec<(String, FileChangeType)> {
        match reduce(events, root, root) {
            Batch::Changes(changes) => changes
                .into_iter()
                .map(|c| {
                    let name = c.path.strip_prefix(root).unwrap().display().to_string();
                    (name, c.change)
                })
                .collect(),
            Batch::Rescan => panic!("unexpected rescan"),
        }
    }

    #[test]
    fn changes_are_judged_by_what_exists_after_the_batch() {
        use FileChangeType::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("new.py"), "").unwrap();
        std::fs::write(root.join("edited.py"), "").unwrap();
        std::fs::write(root.join("renamed_onto.py"), "").unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();

        let changes = reduced(
            vec![
                event(EventKind::Create(CreateKind::File), &root.join("new.py")),
                event(
                    EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                    &root.join("edited.py"),
                ),
                event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
                    &root.join("renamed_onto.py"),
                ),
                // Coalesced flags on a path that no longer exists: gone wins.
                event(EventKind::Create(CreateKind::File), &root.join("gone.py")),
                event(
                    EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                    &root.join("gone.py"),
                ),
                event(EventKind::Create(CreateKind::Folder), &root.join("pkg")),
                event(
                    EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
                    &root.join("edited.py"),
                ),
            ],
            root,
        );
        assert_eq!(
            changes,
            [
                ("new.py".to_string(), Created),
                ("new.py".to_string(), Changed),
                ("edited.py".to_string(), Changed),
                ("renamed_onto.py".to_string(), Created),
                ("renamed_onto.py".to_string(), Changed),
                ("gone.py".to_string(), Deleted),
                ("pkg".to_string(), Created),
            ]
        );
    }

    #[test]
    fn a_removed_path_is_deleted_and_metadata_alone_is_no_change() {
        use FileChangeType::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("touched.py"), "").unwrap();
        let changes = reduced(
            vec![
                event(
                    EventKind::Remove(RemoveKind::File),
                    &root.join("removed.py"),
                ),
                event(
                    EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
                    &root.join("touched.py"),
                ),
            ],
            root,
        );
        assert_eq!(changes, [("removed.py".to_string(), Deleted)]);
    }

    #[test]
    fn a_path_that_went_away_is_what_its_events_said_it_was() {
        use FileChangeType::*;
        use PathKind::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("kept")).unwrap();
        let Batch::Changes(changes) = reduce(
            vec![
                event(
                    EventKind::Remove(RemoveKind::Folder),
                    &root.join("removed_dir"),
                ),
                event(
                    EventKind::Remove(RemoveKind::File),
                    &root.join("removed_file"),
                ),
                event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
                    &root.join("renamed_away"),
                ),
                event(EventKind::Create(CreateKind::Folder), &root.join("kept")),
            ],
            root,
            root,
        ) else {
            panic!("unexpected rescan");
        };
        let kinds: Vec<(String, FileChangeType, PathKind)> = changes
            .into_iter()
            .map(|c| {
                let name = c.path.strip_prefix(root).unwrap().display().to_string();
                (name, c.change, c.kind)
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("removed_dir".to_string(), Deleted, Directory),
                ("removed_file".to_string(), Deleted, File),
                ("renamed_away".to_string(), Deleted, Unknown),
                ("kept".to_string(), Created, Directory),
            ]
        );
    }

    #[test]
    fn a_path_replaced_in_one_batch_is_reported_gone_where_it_may_have_held_files() {
        use FileChangeType::*;
        use PathKind::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in [
            "renamed_away",
            "swapped",
            "remade",
            "was_file",
            "made",
            "moved_in",
        ] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        for name in [
            "was_dir",
            "saved.go",
            "rewritten.go",
            "renamed_over.go",
            "edited.go",
        ] {
            std::fs::write(root.join(name), "").unwrap();
        }
        let at = |name: &str| root.join(name);
        let renamed = |mode| EventKind::Modify(ModifyKind::Name(mode));
        let Batch::Changes(changes) = reduce(
            vec![
                // inotify: renamed away, and a directory made in its place.
                event(renamed(RenameMode::From), &at("renamed_away")),
                event(EventKind::Create(CreateKind::Folder), &at("renamed_away")),
                // FSEvents: both ends of a swap, said alike.
                event(renamed(RenameMode::Any), &at("swapped")),
                event(EventKind::Remove(RemoveKind::Folder), &at("remade")),
                event(EventKind::Create(CreateKind::Folder), &at("remade")),
                event(EventKind::Remove(RemoveKind::File), &at("was_file")),
                event(EventKind::Create(CreateKind::Folder), &at("was_file")),
                event(EventKind::Remove(RemoveKind::Folder), &at("was_dir")),
                event(EventKind::Create(CreateKind::File), &at("was_dir")),
                // Nothing left these that held what they no longer hold.
                event(EventKind::Create(CreateKind::Folder), &at("made")),
                event(renamed(RenameMode::From), &at("gen")),
                event(renamed(RenameMode::To), &at("moved_in")),
                Ok(notify::Event::new(renamed(RenameMode::Both))
                    .add_path(at("gen"))
                    .add_path(at("moved_in"))),
                // FSEvents: a file saved by renaming another onto it.
                event(renamed(RenameMode::Any), &at("saved.go")),
                event(EventKind::Remove(RemoveKind::File), &at("rewritten.go")),
                event(EventKind::Create(CreateKind::File), &at("rewritten.go")),
                event(renamed(RenameMode::From), &at("renamed_over.go")),
                event(EventKind::Create(CreateKind::File), &at("renamed_over.go")),
                event(
                    EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                    &at("edited.go"),
                ),
            ],
            root,
            root,
        ) else {
            panic!("unexpected rescan");
        };
        let kinds: Vec<(String, FileChangeType, PathKind)> = changes
            .into_iter()
            .map(|c| {
                let name = c.path.strip_prefix(root).unwrap().display().to_string();
                (name, c.change, c.kind)
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("renamed_away", Deleted, Unknown),
                ("renamed_away", Created, Directory),
                ("swapped", Deleted, Unknown),
                ("swapped", Created, Directory),
                ("remade", Deleted, Unknown),
                ("remade", Created, Directory),
                ("was_file", Deleted, Unknown),
                ("was_file", Created, Directory),
                ("was_dir", Deleted, Directory),
                ("was_dir", Created, File),
                ("was_dir", Changed, File),
                ("made", Created, Directory),
                ("gen", Deleted, Unknown),
                ("moved_in", Created, Directory),
                ("saved.go", Created, File),
                ("saved.go", Changed, File),
                ("rewritten.go", Created, File),
                ("rewritten.go", Changed, File),
                ("renamed_over.go", Created, File),
                ("renamed_over.go", Changed, File),
                ("edited.go", Changed, File),
            ]
            .map(|(name, change, kind)| (name.to_string(), change, kind))
        );
    }

    #[test]
    fn vcs_stores_and_symora_state_are_never_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "").unwrap();
        let changes = reduced(
            vec![
                event(
                    EventKind::Create(CreateKind::File),
                    &root.join(".git/objects/ab/cdef"),
                ),
                event(
                    EventKind::Create(CreateKind::File),
                    &root.join(".hg/store/x"),
                ),
                event(
                    EventKind::Create(CreateKind::File),
                    &root.join(".symora/store.db-wal"),
                ),
                event(EventKind::Create(CreateKind::File), &root.join(".git/HEAD")),
            ],
            root,
        );
        assert_eq!(
            changes,
            [
                (".git/HEAD".to_string(), FileChangeType::Created),
                (".git/HEAD".to_string(), FileChangeType::Changed),
            ]
        );
    }

    /// A file name need not be UTF-8 on Linux; such a component keeps its
    /// place, so a directory below it is not taken for one at the root.
    #[cfg(unix)]
    #[test]
    fn a_component_that_is_not_utf8_keeps_its_place() {
        use std::os::unix::ffi::OsStrExt;
        let odd = Path::new(OsStr::from_bytes(b"caf\xe9"));
        assert!(!is_excluded(&odd.join(".symora/store.db")));
        assert!(!is_excluded(&Path::new(".git").join(odd).join("objects")));
        assert!(is_excluded(&odd.join(".git/objects/ab")));
    }

    #[test]
    fn events_are_reported_under_the_root_they_were_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let watch_root = dir.path().canonicalize().unwrap();
        std::fs::write(watch_root.join("a.py"), "").unwrap();
        let alias = PathBuf::from("/alias/root");
        let batch = reduce(
            vec![
                event(EventKind::Modify(ModifyKind::Any), &watch_root.join("a.py")),
                event(
                    EventKind::Create(CreateKind::File),
                    Path::new("/elsewhere/b.py"),
                ),
            ],
            &watch_root,
            &alias,
        );
        assert_eq!(
            batch,
            Batch::Changes(vec![FileChange {
                path: alias.join("a.py"),
                change: FileChangeType::Changed,
                kind: PathKind::File,
            }])
        );
    }

    #[test]
    fn only_a_dropped_event_demands_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.py"), "").unwrap();
        let dropped =
            Ok(notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan));
        assert_eq!(reduce(vec![dropped], root, root), Batch::Rescan);

        let uncovered = Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch));
        assert_eq!(
            reduce(
                vec![
                    uncovered,
                    event(EventKind::Modify(ModifyKind::Any), &root.join("a.py"))
                ],
                root,
                root
            ),
            Batch::Changes(vec![FileChange {
                path: root.join("a.py"),
                change: FileChangeType::Changed,
                kind: PathKind::File,
            }])
        );
    }

    #[test]
    fn a_directory_that_appears_brings_its_files() {
        use FileChangeType::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut watcher, _events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
        std::fs::write(root.join("pkg/a.go"), "").unwrap();
        std::fs::write(root.join("pkg/sub/b.go"), "").unwrap();

        let mut changes = vec![change(root.join("pkg").to_str().unwrap(), Created)];
        watcher.follow(&mut changes, &watch_root, &root);
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        let names: Vec<(String, FileChangeType)> = changes
            .into_iter()
            .map(|c| {
                let name = c.path.strip_prefix(&root).unwrap().display().to_string();
                (name, c.change)
            })
            .collect();
        assert_eq!(
            names,
            [
                ("pkg".to_string(), Created),
                ("pkg/a.go".to_string(), Created),
                ("pkg/a.go".to_string(), Changed),
                ("pkg/sub/b.go".to_string(), Created),
                ("pkg/sub/b.go".to_string(), Changed),
            ]
        );
    }

    #[test]
    fn a_file_already_in_the_batch_is_still_reported_created_with_its_directory() {
        use FileChangeType::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut watcher, _events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/a.go"), "").unwrap();

        let mut changes = vec![
            change(root.join("pkg").to_str().unwrap(), Created),
            change(root.join("pkg/a.go").to_str().unwrap(), Changed),
        ];
        watcher.follow(&mut changes, &watch_root, &root);

        let file: Vec<FileChangeType> = changes
            .iter()
            .filter(|c| c.path == root.join("pkg/a.go"))
            .map(|c| c.change)
            .collect();
        assert_eq!(file, [Changed, Created]);
    }

    #[test]
    fn a_directory_renamed_away_reaches_a_watcher_for_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
        std::fs::write(root.join("pkg/p.go"), "").unwrap();
        std::fs::write(root.join("pkg/sub/q.go"), "").unwrap();
        let (mut watcher, _events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::rename(root.join("pkg"), root.join("pkg2")).unwrap();

        // A rename reports both paths, and neither as a file or a directory.
        let renamed = EventKind::Modify(ModifyKind::Name(RenameMode::Any));
        let Batch::Changes(mut changes) = reduce(
            vec![
                event(renamed, &watch_root.join("pkg")),
                event(renamed, &watch_root.join("pkg2")),
            ],
            &watch_root,
            &root,
        ) else {
            panic!("unexpected rescan");
        };
        watcher.follow(&mut changes, &watch_root, &root);

        let registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**/*.go" }]
        }));
        let under = format!("{}/", path_to_uri(&root));
        let mut events: Vec<String> = selected(&registry, &changes)
            .into_iter()
            .map(|event| event.replace(&under, ""))
            .collect();
        events.sort();
        let mut expected = vec![
            "pkg2/p.go:Changed",
            "pkg2/p.go:Created",
            "pkg2/sub/q.go:Changed",
            "pkg2/sub/q.go:Created",
            "pkg:Deleted",
        ];
        // Neither end says which it is, so the end a directory arrived at
        // may have been left as well; only on Linux do the directories
        // watched say it held nothing.
        if !cfg!(target_os = "linux") {
            expected.insert(4, "pkg2:Deleted");
        }
        assert_eq!(events, expected);
    }

    #[test]
    fn a_directory_replaced_in_one_batch_reaches_a_watcher_for_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let away = tempfile::tempdir().unwrap();
        for held in ["pkg/old.go", "pkg/keep.go", "lib/l.go"] {
            std::fs::create_dir_all(root.join(held).parent().unwrap()).unwrap();
            std::fs::write(root.join(held), "").unwrap();
        }
        std::fs::create_dir(away.path().join("gen")).unwrap();
        std::fs::write(away.path().join("gen/g.go"), "").unwrap();
        let (mut watcher, _events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        // `mv pkg out; mkdir pkg`, and a swap, `mv lib out; mv gen lib`.
        std::fs::rename(root.join("pkg"), away.path().join("pkg")).unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/keep.go"), "").unwrap();
        std::fs::write(root.join("pkg/new.go"), "").unwrap();
        std::fs::rename(root.join("lib"), away.path().join("lib")).unwrap();
        std::fs::rename(away.path().join("gen"), root.join("lib")).unwrap();

        // As inotify reports `pkg`, and FSEvents `lib` and a file it saw
        // written in the new `pkg`.
        let renamed = |mode| EventKind::Modify(ModifyKind::Name(mode));
        let Batch::Changes(mut changes) = reduce(
            vec![
                event(renamed(RenameMode::From), &watch_root.join("pkg")),
                event(
                    EventKind::Create(CreateKind::Folder),
                    &watch_root.join("pkg"),
                ),
                event(renamed(RenameMode::Any), &watch_root.join("lib")),
                event(
                    EventKind::Create(CreateKind::File),
                    &watch_root.join("pkg/new.go"),
                ),
            ],
            &watch_root,
            &root,
        ) else {
            panic!("unexpected rescan");
        };
        watcher.follow(&mut changes, &watch_root, &root);

        let registry = registry(serde_json::json!({
            "watchers": [{ "globPattern": "**/*.go" }]
        }));
        let under = format!("{}/", path_to_uri(&root));
        let events: Vec<String> = selected(&registry, &changes)
            .into_iter()
            .map(|event| event.replace(&under, ""))
            .collect();
        // Each directory is reported gone before any file it holds now.
        let (gone, files) = events.split_at(2.min(events.len()));
        assert_eq!(gone, ["pkg:Deleted", "lib:Deleted"]);
        let mut files = files.to_vec();
        files.sort();
        assert_eq!(
            files,
            [
                "lib/g.go:Changed",
                "lib/g.go:Created",
                "pkg/keep.go:Changed",
                "pkg/keep.go:Created",
                "pkg/new.go:Changed",
                "pkg/new.go:Created",
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_that_cannot_be_watched_leaves_the_rest_watched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("locked/inner")).unwrap();
        std::fs::create_dir_all(root.join("open/inner")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        if std::fs::read_dir(root.join("locked")).is_ok() {
            // Permission bits do not constrain this user (root).
            std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            return;
        }

        let started = WorkspaceWatcher::start(&root);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let (watcher, _events, _) = started.expect("one unreadable directory fails nothing");
        assert!(watcher.watched.contains(&root.join("open/inner")));
        assert!(!watcher.watched.contains(&root.join("locked")));
    }

    /// Whether an event naming `path` arrives within a few seconds.
    #[cfg(target_os = "linux")]
    fn arrives(events: &mut mpsc::UnboundedReceiver<RawEvent>, path: &Path) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            while let Ok(event) = events.try_recv() {
                if event.is_ok_and(|event| event.paths.iter().any(|p| p == path)) {
                    return true;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_recreated_at_the_same_path_is_watched_again() {
        use FileChangeType::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        let (mut watcher, mut events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::remove_dir(root.join("pkg")).unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();

        let mut changes = vec![change(root.join("pkg").to_str().unwrap(), Created)];
        watcher.follow(&mut changes, &watch_root, &root);
        std::fs::write(root.join("pkg/a.go"), "").unwrap();

        assert!(arrives(&mut events, &root.join("pkg/a.go")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_created_while_events_were_lost_is_watched_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut watcher, mut events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        // Its creation is never followed, as when that event is lost.
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();

        watcher.rewatch(&watch_root).unwrap();
        std::fs::write(root.join("pkg/sub/a.go"), "").unwrap();

        assert!(arrives(&mut events, &root.join("pkg/sub/a.go")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_replaced_while_events_were_lost_is_watched_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        let (mut watcher, mut events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        // Neither change is followed, as when those events are lost.
        std::fs::remove_dir(root.join("pkg")).unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();

        watcher.rewatch(&watch_root).unwrap();
        std::fs::write(root.join("pkg/a.go"), "").unwrap();

        assert!(arrives(&mut events, &root.join("pkg/a.go")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_renamed_directory_is_watched_at_its_new_path() {
        use FileChangeType::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("old/inner")).unwrap();
        let (mut watcher, mut events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::rename(root.join("old"), root.join("new")).unwrap();

        let mut changes = vec![
            change(root.join("old").to_str().unwrap(), Deleted),
            change(root.join("new").to_str().unwrap(), Created),
        ];
        watcher.follow(&mut changes, &watch_root, &root);
        std::fs::write(root.join("new/inner/a.go"), "").unwrap();

        assert!(arrives(&mut events, &root.join("new/inner/a.go")));
        assert!(
            !watcher
                .watched
                .iter()
                .any(|path| path.starts_with(root.join("old")))
        );
    }

    /// inotify reports a path moved away without saying what it was; the
    /// directories watched say it.
    #[cfg(target_os = "linux")]
    #[test]
    fn what_was_moved_away_is_told_by_the_directories_watched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let away = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.join("pkg")).unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        let (mut watcher, _events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::rename(root.join("pkg"), away.path().join("pkg")).unwrap();
        std::fs::rename(root.join("notes.txt"), away.path().join("notes.txt")).unwrap();

        let mut changes = vec![
            gone(root.join("pkg").to_str().unwrap(), PathKind::Unknown),
            gone(root.join("notes.txt").to_str().unwrap(), PathKind::Unknown),
        ];
        watcher.follow(&mut changes, &watch_root, &root);

        let kinds: Vec<PathKind> = changes.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [PathKind::Directory, PathKind::File]);
    }

    /// inotify says neither what left a path nor what took its place; the
    /// directories watched say which of them was a directory.
    #[cfg(target_os = "linux")]
    #[test]
    fn what_a_replaced_path_held_is_told_by_the_directories_watched() {
        use FileChangeType::*;
        use PathKind::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let away = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        std::fs::write(root.join("f"), "").unwrap();
        let (mut watcher, _events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        // A directory replaced by a file, and a file by a directory.
        std::fs::rename(root.join("d"), away.path().join("d")).unwrap();
        std::fs::write(root.join("d"), "").unwrap();
        std::fs::rename(root.join("f"), away.path().join("f")).unwrap();
        std::fs::create_dir(root.join("f")).unwrap();

        let from = EventKind::Modify(ModifyKind::Name(RenameMode::From));
        let Batch::Changes(mut changes) = reduce(
            vec![
                event(from, &root.join("d")),
                event(EventKind::Create(CreateKind::File), &root.join("d")),
                event(from, &root.join("f")),
                event(EventKind::Create(CreateKind::Folder), &root.join("f")),
            ],
            &watch_root,
            &root,
        ) else {
            panic!("unexpected rescan");
        };
        watcher.follow(&mut changes, &watch_root, &root);

        let kinds: Vec<(String, FileChangeType, PathKind)> = changes
            .into_iter()
            .map(|c| {
                let name = c.path.strip_prefix(&root).unwrap().display().to_string();
                (name, c.change, c.kind)
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("d".to_string(), Deleted, Directory),
                ("d".to_string(), Created, File),
                ("d".to_string(), Changed, File),
                ("f".to_string(), Deleted, File),
                ("f".to_string(), Created, Directory),
            ]
        );
        assert!(
            !watcher
                .watched
                .iter()
                .any(|path| path.starts_with(root.join("d")))
        );
        assert!(watcher.watched.contains(&root.join("f")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_replaced_in_one_batch_is_watched_anew() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let away = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.join("pkg/old")).unwrap();
        let (mut watcher, mut events, watch_root) = WorkspaceWatcher::start(&root).unwrap();
        std::fs::rename(root.join("pkg"), away.path().join("pkg")).unwrap();
        std::fs::create_dir_all(root.join("pkg/new")).unwrap();

        let Batch::Changes(mut changes) = reduce(
            vec![
                event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                    &root.join("pkg"),
                ),
                event(EventKind::Create(CreateKind::Folder), &root.join("pkg")),
            ],
            &watch_root,
            &root,
        ) else {
            panic!("unexpected rescan");
        };
        watcher.follow(&mut changes, &watch_root, &root);
        std::fs::write(root.join("pkg/new/a.go"), "").unwrap();

        assert!(arrives(&mut events, &root.join("pkg/new/a.go")));
        assert!(!watcher.watched.contains(&root.join("pkg/old")));
    }
}
