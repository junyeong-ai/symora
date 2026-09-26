//! The client half of `workspace/didChangeWatchedFiles`: servers register the
//! files they care about, and changes on disk under the workspace are
//! forwarded to them. A server that registers watchers stops watching the
//! disk itself, so without this half it answers from the tree it saw at
//! startup.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
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
    kinds: u8,
}

impl FileWatcher {
    fn wants(&self, change: &FileChange) -> bool {
        if self.kinds & change.change.watch_kind() == 0 {
            return false;
        }
        match &self.base {
            Some(base) => change
                .path
                .strip_prefix(base)
                .is_ok_and(|relative| self.glob.is_match(relative)),
            None => self.glob.is_match(&change.path),
        }
    }
}

impl WatchRegistry {
    /// Record a `workspace/didChangeWatchedFiles` registration. A watcher
    /// whose glob does not compile can never match, so it is dropped.
    pub fn register(&mut self, registration: &Registration) {
        let Some(options) = registration.register_options.clone().and_then(|options| {
            serde_json::from_value::<DidChangeWatchedFilesRegistrationOptions>(options).ok()
        }) else {
            tracing::debug!(
                "Watched-files registration {} has no watchers",
                registration.id
            );
            return;
        };
        let watchers = options
            .watchers
            .into_iter()
            .filter_map(|watcher| {
                let (base, pattern) = match watcher.glob_pattern {
                    GlobPattern::Absolute(pattern) => (None, pattern),
                    GlobPattern::Relative(relative) => {
                        (Some(uri_to_path(relative.base_uri.uri())), relative.pattern)
                    }
                };
                let glob = GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .build()
                    .inspect_err(|e| tracing::debug!("Ignoring watcher glob {pattern:?}: {e}"))
                    .ok()?
                    .compile_matcher();
                Some(FileWatcher {
                    base,
                    glob,
                    kinds: watcher.kind.unwrap_or(7),
                })
            })
            .collect();
        self.registrations.insert(registration.id.clone(), watchers);
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
    /// file written in between is seen by one or the other.
    pub fn follow(&mut self, changes: &mut Vec<FileChange>, watch_root: &Path, root: &Path) {
        let mut reported: HashSet<PathBuf> = changes.iter().map(|c| c.path.clone()).collect();
        let mut found = Vec::new();
        for change in changes.iter() {
            let Ok(relative) = change.path.strip_prefix(root) else {
                continue;
            };
            let path = watch_root.join(relative);
            if change.change == FileChangeType::Deleted {
                self.retract(&path);
            } else if change.change == FileChangeType::Created
                && path.symlink_metadata().is_ok_and(|meta| meta.is_dir())
            {
                // A directory created where one was removed is a new one.
                self.retract(&path);
                self.extend(&path, watch_root);
                for file in files_under(&path, watch_root) {
                    let path = root.join(file.strip_prefix(watch_root).unwrap_or(&file));
                    if reported.insert(path.clone()) {
                        found.push(FileChange {
                            path: path.clone(),
                            change: FileChangeType::Created,
                        });
                        found.push(FileChange {
                            path,
                            change: FileChangeType::Changed,
                        });
                    }
                }
            }
        }
        changes.extend(found);
    }

    #[cfg(not(target_os = "linux"))]
    fn watch_root(&mut self, watch_root: &Path) -> notify::Result<()> {
        self.watcher.watch(watch_root, RecursiveMode::Recursive)
    }

    #[cfg(not(target_os = "linux"))]
    fn extend(&mut self, _dir: &Path, _watch_root: &Path) {}

    #[cfg(not(target_os = "linux"))]
    fn retract(&mut self, _dir: &Path) {}

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

    /// Drop the watches at and below `dir`, which is gone from that path.
    #[cfg(target_os = "linux")]
    fn retract(&mut self, dir: &Path) {
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
    removed: bool,
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
/// already tracking that path.
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
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) => |s| s.appeared = true,
            EventKind::Remove(_) => |s| s.removed = true,
            EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Access(_)
            | EventKind::Other => {
                continue;
            }
            EventKind::Modify(_) | EventKind::Any => |s| s.modified = true,
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
        }
    }

    let mut changes = Vec::new();
    for relative in order {
        let touched = seen[&relative];
        let path = root.join(&relative);
        match watch_root.join(&relative).symlink_metadata() {
            Ok(meta) => {
                if touched.appeared || touched.removed {
                    changes.push(FileChange {
                        path: path.clone(),
                        change: FileChangeType::Created,
                    });
                }
                if !meta.is_dir() {
                    changes.push(FileChange {
                        path,
                        change: FileChangeType::Changed,
                    });
                }
            }
            Err(_) => changes.push(FileChange {
                path,
                change: FileChangeType::Deleted,
            }),
        }
    }
    Batch::Changes(changes)
}

fn is_excluded(relative: &Path) -> bool {
    let parts: Vec<&str> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    parts.first() == Some(&STATE_DIR)
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
        registry.register(&Registration {
            id: "r1".to_string(),
            method: WATCHED_FILES_METHOD.to_string(),
            register_options: Some(options),
        });
        registry
    }

    fn change(path: &str, change: FileChangeType) -> FileChange {
        FileChange {
            path: PathBuf::from(path),
            change,
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
}
