//! The client half of `workspace/didChangeWatchedFiles`: servers register the
//! files they care about, and changes on disk under the workspace are
//! forwarded to them. A server that registers watchers stops watching the
//! disk itself, so without this half it answers from the tree it saw at
//! startup.

use std::collections::HashMap;
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

/// A recursive watch on the workspace root. Dropping it stops the watch and
/// closes the event channel.
pub struct WorkspaceWatcher {
    _watcher: RecommendedWatcher,
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
        let mut watcher = RecommendedWatcher::new(
            move |event: RawEvent| {
                let _ = sender.send(event);
            },
            // A link out of the project would widen the watch to whatever it
            // points at; the tree the servers were given is the project's.
            notify::Config::default().with_follow_symlinks(false),
        )?;
        watcher.watch(&watch_root, RecursiveMode::Recursive)?;
        Ok((Self { _watcher: watcher }, events, watch_root))
    }
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
        let event = match event {
            Ok(event) if !event.need_rescan() => event,
            Ok(_) => return Batch::Rescan,
            Err(e) => {
                tracing::warn!("File watcher error: {e}");
                return Batch::Rescan;
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
    fn a_dropped_event_or_a_watcher_error_demands_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dropped =
            Ok(notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan));
        assert_eq!(reduce(vec![dropped], root, root), Batch::Rescan);
        let failed = Err(notify::Error::generic("inotify queue overflow"));
        assert_eq!(reduce(vec![failed], root, root), Batch::Rescan);
    }
}
