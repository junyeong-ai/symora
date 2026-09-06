use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension;

use super::db::SqliteDb;

use super::schema::*;
use super::symbols::SymbolExtractor;
use super::types::*;
use crate::error::StoreError;
use crate::infra::file_filter::{Discovery, FileFilter};
use crate::infra::file_lock::FileLock;
use crate::models::symbol::{Language, SymbolKind};

/// The languages a full index pass covers when no language filter narrows
/// it: every file of theirs — every extension the language declares — is
/// indexed for content, and those with an extractor for symbols too. Also
/// the domain an unrestricted build's `refresh_files` honors, so an edit can
/// never index a file kind a build wouldn't.
///
/// This is the domain an unscoped search asks of the index, so it is that
/// domain: a documentation or data format is reached by naming it, and the
/// working tree answers those directly.
fn indexed_languages() -> Vec<Language> {
    Language::all()
        .into_iter()
        .filter(|language| language.is_code())
        .collect()
}

/// Meta key recording that a full build completed, and what it covered.
/// Its presence IS the build-completed marker: a store without it was
/// never built (merely opening the DB for a read materializes the file,
/// so file existence proves nothing), and `refresh_files` must not grow
/// a 1-file index inside it.
const META_BUILD_SCOPE: &str = "build_scope";

/// Meta key holding the monotonic build epoch. Every operation that
/// destroys index rows advances it, and a build may publish its completion
/// marker only while the epoch it opened with still stands — so a build
/// another operation superseded can never mark the index whole.
const META_BUILD_EPOCH: &str = "build_epoch";

/// The stat of every file the last completed build discovered, reduced to one
/// value. What it answers is whether the tree still looks the way that build
/// read it, which is the difference between an index-backed zero that means
/// "nothing declares this" and one that means "nothing declared it then".
const META_TREE_FINGERPRINT: &str = "tree_fingerprint";

/// How long opening a store waits for a build to release the index before
/// giving up on replacing a database it cannot read. Long enough to cover
/// an ordinary build, short enough that a command never looks hung.
const DB_REPLACE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const LOCK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How long taking the index waits out a holder before calling it a build
/// in progress. A status probe holds the lock for a syscall rather than
/// for work, and refusing an operation on account of one would report a
/// build nothing is running.
const INDEX_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// The language scope the last completed build covered. Persisted in the
/// store's `meta` table so per-file refreshes honor the build's narrowing
/// (`search index build --lang rust` must not gain `.py` rows from an
/// edit) and so a never-built store is recognizable regardless of whether
/// the DB file exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildScope {
    /// Unrestricted build: every language [`indexed_languages`] names.
    All,
    Languages(Vec<Language>),
}

impl BuildScope {
    fn from_options(languages: &Option<Vec<Language>>) -> Self {
        match languages {
            None => Self::All,
            Some(langs) => {
                let mut langs = langs.clone();
                langs.sort_by_key(|l| l.lsp_id());
                langs.dedup();
                Self::Languages(langs)
            }
        }
    }

    /// The languages whose files a build under this scope indexes — the
    /// content search's coverage.
    fn content_languages(&self) -> Vec<Language> {
        match self {
            Self::All => indexed_languages(),
            Self::Languages(langs) => langs.clone(),
        }
    }

    /// The extension set a build under this scope discovers — also the
    /// domain a refresh honors.
    fn extensions(&self) -> Vec<&'static str> {
        self.content_languages()
            .iter()
            .flat_map(|l| l.extensions())
            .copied()
            .collect()
    }

    /// The languages a build under this scope extracts symbols for.
    /// Narrower than [`content_languages`](Self::content_languages): a scope
    /// can name a language the binary has no extractor for, and such a build
    /// indexes the files' content without ever producing a symbol row for
    /// them.
    fn languages(&self) -> Vec<Language> {
        self.content_languages()
            .into_iter()
            .filter(|l| SymbolExtractor::is_supported(*l))
            .collect()
    }

    fn meta_value(&self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Languages(langs) => langs
                .iter()
                .map(|l| l.lsp_id())
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    fn parse(value: &str) -> Self {
        if value == "all" {
            return Self::All;
        }
        Self::Languages(
            value
                .split(',')
                .filter(|s| !s.is_empty())
                .map(Language::parse_or_default)
                .collect(),
        )
    }
}

/// What opening a build window does to the rows already in the index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reindex {
    /// Keep them: the build refreshes and prunes in place.
    InPlace,
    /// Drop every row first.
    FromScratch,
}

/// The files one SQLite database occupies: the database itself and the two the
/// write-ahead log keeps beside it.
const SQLITE_SIDECARS: [&str; 3] = ["", "-wal", "-shm"];

/// A database file's companion, named the way SQLite names it — by appending
/// to the whole file name, extension included.
fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    match suffix.is_empty() {
        true => db_path.to_path_buf(),
        false => {
            let mut name = db_path.as_os_str().to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        }
    }
}

/// Move a database out of the way, all of it.
///
/// A write-ahead-log database is three files that only mean anything together:
/// a `-wal` left where the database used to be holds committed pages the
/// database no longer beside it is missing, and SQLite refuses to open the
/// fresh file that takes its place. Moving the set keeps the replacement
/// openable and the copy left behind readable.
///
/// Best effort per file: this runs to make room for a database that can be
/// opened, and a name that could not be moved is reported by the open that
/// follows rather than pre-empting it.
async fn move_database_aside(db_path: &Path, backup_path: &Path) {
    for suffix in SQLITE_SIDECARS {
        let from = sidecar_path(db_path, suffix);
        if !from.exists() {
            continue;
        }
        if let Err(e) = tokio::fs::rename(&from, sidecar_path(backup_path, suffix)).await {
            tracing::debug!("Failed to move {} aside: {e}", from.display());
        }
    }
}

fn read_meta(conn: &rusqlite::Connection, key: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// A fingerprint of the tree a build read: the path, size, and modification
/// time of every file it discovered, in one stable order.
///
/// Taken before any of those files is opened. A file written while the build
/// runs then carries a modification time past the one recorded here, so the
/// next comparison reports the index behind rather than current — the one
/// direction this must never get wrong, since a false "current" publishes a
/// confident zero over content nothing read.
///
/// A file the walk found and a stat cannot describe compares equal to itself
/// and to nothing else. It is unreadable, so the build already recorded it in
/// `unread_paths` and every answer drawn from the index is a lower bound on
/// its account; saying the tree moved as well would name a second fact that
/// no rebuild can clear.
fn tree_fingerprint(root: &Path, files: &[PathBuf]) -> String {
    let mut entries: Vec<String> = files
        .iter()
        .map(|path| {
            let name = path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string();
            match file_stamp(path) {
                Some((size, modified)) => format!("{name}\u{1}{size}\u{1}{modified}"),
                None => format!("{name}\u{1}?"),
            }
        })
        .collect();
    entries.sort();
    format!(
        "{:016x}",
        crate::infra::hash_content(&entries.join("\u{2}"))
    )
}

/// A file's size and modification time in nanoseconds, or `None` where the
/// filesystem cannot express one of them.
fn file_stamp(path: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((meta.len(), modified))
}

fn write_meta(conn: &rusqlite::Connection, key: &str, value: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// What the last completed build covers, and how completely.
///
/// The two facts are written in one transaction and only mean anything
/// together: a scope naming a language whose files the walk could not enter
/// covers that language in part, so a reader that takes the scope without the
/// paths reads a partial index as a whole one.
struct CompletedBuild {
    scope: BuildScope,
    /// Paths the build could not read — a file it could not open or a
    /// directory it could not enter. Their symbols and text are absent from
    /// an index whose scope names their language, so anything counted out of
    /// this build is a lower bound while any remain.
    unread_paths: Vec<UnreadPath>,
}

/// The last completed build, or `None` when none has ever completed against
/// this store. Read inside whatever transaction needs it: the marker is the
/// readiness answer, and a copy of it held anywhere else would be stale the
/// moment another process built, cleared, or crashed.
fn read_completed_build(
    conn: &rusqlite::Connection,
) -> Result<Option<CompletedBuild>, rusqlite::Error> {
    let Some(scope) = read_meta(conn, META_BUILD_SCOPE)?
        .as_deref()
        .map(BuildScope::parse)
    else {
        return Ok(None);
    };
    Ok(Some(CompletedBuild {
        scope,
        unread_paths: read_unread_paths(conn)?,
    }))
}

/// The paths the last completed build could not read, in one stable order so
/// two readings of the same index answer alike.
fn read_unread_paths(conn: &rusqlite::Connection) -> Result<Vec<UnreadPath>, rusqlite::Error> {
    conn.prepare("SELECT path, is_file FROM unread_paths ORDER BY path")?
        .query_map([], |row| {
            Ok(UnreadPath {
                path: row.get(0)?,
                is_file: row.get(1)?,
            })
        })?
        .collect()
}

fn read_build_epoch(conn: &rusqlite::Connection) -> Result<i64, rusqlite::Error> {
    Ok(read_meta(conn, META_BUILD_EPOCH)?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

pub struct Store {
    db: SqliteDb,
    project_root: PathBuf,
    config: StoreConfig,
}

impl Store {
    /// On-disk location of the index for a project — the single source of
    /// truth for the path, shared by `open` and callers that only need to
    /// know whether an index exists.
    pub fn db_path(project_root: &Path) -> PathBuf {
        project_root.join(".symora").join("store.db")
    }

    /// The file whose lock decides who may rewrite this index: a build
    /// holds it exclusively for its whole destructive window, a per-file
    /// refresh holds it shared, and replacing the database itself waits
    /// for it.
    fn lock_path(project_root: &Path) -> PathBuf {
        project_root.join(".symora").join("index.lock")
    }

    pub async fn open(project_root: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        let db_path = Self::db_path(project_root);
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(StoreError::Io)?;
        }

        let db = match Self::try_open_db(&db_path).await {
            Ok(db) => db,
            // A clean schema upgrade is an expected migration, not corruption:
            // the rebuild action is identical, but classify it honestly so the
            // log doesn't cry "corrupted" on every version bump.
            Err(StoreError::SchemaMismatch { found, expected }) => {
                tracing::info!(
                    "Store schema changed (db v{found} -> v{expected}), rebuilding index: {}",
                    db_path.display()
                );
                Self::replace_db(project_root, &db_path).await?
            }
            // Only a file that cannot be read as a database is replaced.
            // A busy, locked, or unreadable-for-other-reasons store is
            // intact, and destroying it would turn a passing contention
            // into permanent loss.
            Err(StoreError::Corrupt(reason)) => {
                tracing::warn!(
                    "Store database is unusable, recreating: {}: {reason}",
                    db_path.display()
                );
                Self::replace_db(project_root, &db_path).await?
            }
            Err(e) => return Err(e),
        };

        Ok(Self {
            db,
            project_root: project_root.to_path_buf(),
            config,
        })
    }

    async fn try_open_db(db_path: &Path) -> Result<SqliteDb, StoreError> {
        let db = SqliteDb::open(db_path).await?;

        let version: i32 = db
            .call(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0)))
            .await?;

        if version != 0 && version != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }

        db.execute(INIT_SCHEMA).await?;
        db.call(|conn| {
            conn.query_row("SELECT 1", [], |_r| Ok(()))?;
            Ok(())
        })
        .await?;
        Ok(db)
    }

    /// Swap in a fresh database for one this binary cannot use. Holding
    /// the build lock is the point: replacing the file under a running
    /// build would leave that build writing to a database no one else can
    /// reach, and whoever takes the lock second finds the replacement
    /// already made and simply opens it. The wait polls rather than blocks
    /// so that giving up actually gives up — a blocking acquire could not
    /// be called back, and would sit on a thread long after the caller was
    /// told the index was busy.
    async fn replace_db(project_root: &Path, db_path: &Path) -> Result<SqliteDb, StoreError> {
        let lock_path = Self::lock_path(project_root);
        let deadline = tokio::time::Instant::now() + DB_REPLACE_LOCK_WAIT;
        let _lock = loop {
            if let Some(lock) = FileLock::exclusive(&lock_path)? {
                break lock;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(StoreError::Database(
                    "timed out waiting for an index build to finish before rebuilding the index"
                        .to_string(),
                ));
            }
            tokio::time::sleep(LOCK_POLL_INTERVAL).await;
        };

        // The recheck answers one question — did someone else already
        // replace it — and classifies what it finds exactly as the first
        // attempt did. A writer that ignores this lock would otherwise get
        // an intact database renamed out from under it.
        match Self::try_open_db(db_path).await {
            Ok(db) => Ok(db),
            Err(StoreError::Corrupt(_) | StoreError::SchemaMismatch { .. }) => {
                Self::recover_db(db_path).await
            }
            Err(e) => Err(e),
        }
    }

    /// Replace a database this binary cannot use, keeping the old one beside
    /// it under `.bak`.
    ///
    /// A WAL database is three files, and they only mean anything together: a
    /// `-wal` left behind belongs to the database that was moved away, and
    /// SQLite refuses the fresh file it now sits next to. So the set moves as
    /// one — which also leaves the backup openable rather than half of a
    /// database whose committed pages went elsewhere.
    async fn recover_db(db_path: &Path) -> Result<SqliteDb, StoreError> {
        if db_path.exists() {
            move_database_aside(db_path, &db_path.with_extension("db.bak")).await;
        }
        let db = SqliteDb::open(db_path).await?;
        db.execute(INIT_SCHEMA).await?;
        Ok(db)
    }

    /// Search the symbols the index covers, within the domain asked for.
    /// The answer names the languages it speaks for — the last completed
    /// build's extractor scope, narrowed by an explicit `--lang` — and holds
    /// rows for exactly those, enforced in the query: rows a narrowed build
    /// left behind cannot answer for a language it no longer covers, and the
    /// caller's live read of the remainder can neither duplicate a row nor
    /// miss a language.
    pub async fn search_symbols(
        &self,
        query: &str,
        limit: usize,
        kind_filter: Option<SymbolKind>,
        language: Option<Language>,
    ) -> Result<SearchPage<SymbolSearchResult>, StoreError> {
        let query = query.to_string();
        let limit = limit as i64;
        let kind_str = kind_filter.map(|k| k.to_string());
        let requested = language;

        let answer = self
            .db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let Some(build) = read_completed_build(&tx)? else {
                    return Ok(None);
                };
                let covered: Vec<Language> = match requested {
                    Some(language) => build
                        .scope
                        .languages()
                        .into_iter()
                        .filter(|indexed| *indexed == language)
                        .collect(),
                    None => build.scope.languages(),
                };
                if covered.is_empty() {
                    return Ok(Some((
                        SearchPage {
                            total: 0,
                            rows: Vec::new(),
                            stale_files: Vec::new(),
                            covered,
                            unread_paths: build.unread_paths.clone(),
                        },
                        Vec::new(),
                    )));
                }

                let lang_ids: Vec<String> =
                    covered.iter().map(|l| l.lsp_id().to_string()).collect();
                let sql = build_symbol_search_query(kind_str.is_some(), lang_ids.len());
                let mut stmt = tx.prepare(&sql)?;
                let params: Vec<rusqlite::types::Value> =
                    std::iter::once(rusqlite::types::Value::from(query))
                        .chain(std::iter::once(rusqlite::types::Value::from(limit)))
                        .chain(kind_str.map(rusqlite::types::Value::from))
                        .chain(lang_ids.into_iter().map(rusqlite::types::Value::from))
                        .collect();
                let rows = stmt.query(rusqlite::params_from_iter(params))?;

                let mut total = 0usize;
                let rows: Vec<SymbolSearchResult> = rows
                    .mapped(|r| {
                        Ok((
                            r.get::<_, i64>(7)? as usize,
                            SymbolSearchResult {
                                name: r.get(0)?,
                                name_path: r.get(1)?,
                                kind: SymbolKind::parse_or_default(&r.get::<_, String>(2)?),
                                line: r.get::<_, i32>(3)? as u32,
                                column: r.get::<_, i32>(4)? as u32,
                                file: PathBuf::from(r.get::<_, String>(5)?),
                                container: r.get(6)?,
                                score: r.get(8)?,
                            },
                        ))
                    })
                    .filter_map(|r| match r {
                        Ok((row_total, v)) => {
                            total = row_total;
                            Some(v)
                        }
                        Err(e) => {
                            tracing::debug!("Error parsing symbol search row: {}", e);
                            None
                        }
                    })
                    .collect();

                // Snapshot the indexed hash of each backing file in the SAME
                // connection closure as the row query, so a concurrent
                // reindex cannot land between reading the rows and reading
                // the hashes they were derived from.
                let snapshot = indexed_hashes(&tx, rows.iter().map(|r| &r.file))?;
                drop(stmt);
                tx.commit()?;
                let page = SearchPage {
                    total,
                    rows,
                    stale_files: Vec::new(),
                    covered,
                    unread_paths: build.unread_paths.clone(),
                };
                Ok(Some((page, snapshot)))
            })
            .await?;

        let (mut page, snapshot) = match answer {
            Some(answer) => answer,
            None => return Err(self.no_completed_build()),
        };
        page.stale_files = changed_backing_files(snapshot).await;
        Ok(page)
    }

    /// Why the index has no completed build to answer from. A build owns the
    /// index for its whole duration — `begin_build` clears the completion
    /// marker and `publish_build` restores it — so an absent marker is two
    /// different states, and collapsing them makes a rebuilding index read
    /// as one that was never built.
    fn no_completed_build(&self) -> StoreError {
        if self.build_in_progress() {
            StoreError::Rebuilding
        } else {
            StoreError::NotInitialized
        }
    }

    /// Search the content the index covers, within the domain asked for.
    /// The answer names the languages it speaks for — the requested set
    /// intersected with the last completed build's scope — and holds rows
    /// for exactly those, so the caller's live read of the remainder can
    /// neither duplicate a row nor miss a language. Scope, rows, and the
    /// hashes behind them come from one snapshot, so a rebuild racing this
    /// call cannot land between them.
    pub async fn search_content(
        &self,
        query: &str,
        limit: usize,
        languages: &[Language],
    ) -> Result<SearchPage<ContentSearchResult>, StoreError> {
        let requested = languages.to_vec();
        let query = query.to_string();
        let limit = limit as i64;
        // The trigram pre-filter needs >= 3 chars; shorter queries fall back to
        // the LIKE-only scan (the deterministic threshold, not a guess).
        let use_fts = query.chars().count() >= FTS_MIN_QUERY_CHARS;

        let answer = self
            .db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let Some(build) = read_completed_build(&tx)? else {
                    return Ok(None);
                };
                let covered: Vec<Language> = build
                    .scope
                    .content_languages()
                    .into_iter()
                    .filter(|language| requested.contains(language))
                    .collect();
                if covered.is_empty() {
                    return Ok(Some((
                        SearchPage {
                            total: 0,
                            rows: Vec::new(),
                            stale_files: Vec::new(),
                            covered,
                            unread_paths: build.unread_paths.clone(),
                        },
                        Vec::new(),
                    )));
                }

                let lang_ids: Vec<String> =
                    covered.iter().map(|l| l.lsp_id().to_string()).collect();
                let sql = build_content_search_query(lang_ids.len(), use_fts);
                let mut stmt = tx.prepare(&sql)?;
                let params: Vec<rusqlite::types::Value> =
                    std::iter::once(rusqlite::types::Value::from(query))
                        .chain(std::iter::once(rusqlite::types::Value::from(limit)))
                        .chain(lang_ids.into_iter().map(rusqlite::types::Value::from))
                        .collect();
                let rows = stmt.query(rusqlite::params_from_iter(params))?;

                let mut total = 0usize;
                let rows: Vec<ContentSearchResult> = rows
                    .mapped(|r| {
                        Ok((
                            r.get::<_, i64>(4)? as usize,
                            ContentSearchResult {
                                content: r.get(0)?,
                                line: r.get::<_, i32>(1)? as u32,
                                file: PathBuf::from(r.get::<_, String>(2)?),
                                score: r.get(5)?,
                            },
                        ))
                    })
                    .filter_map(|r| match r {
                        Ok((row_total, v)) => {
                            total = row_total;
                            Some(v)
                        }
                        Err(e) => {
                            tracing::debug!("Error parsing content search row: {}", e);
                            None
                        }
                    })
                    .collect();

                let snapshot = indexed_hashes(&tx, rows.iter().map(|r| &r.file))?;
                drop(stmt);
                tx.commit()?;
                let page = SearchPage {
                    total,
                    rows,
                    stale_files: Vec::new(),
                    covered,
                    unread_paths: build.unread_paths.clone(),
                };
                Ok(Some((page, snapshot)))
            })
            .await?;

        let (mut page, snapshot) = match answer {
            Some(answer) => answer,
            None => return Err(self.no_completed_build()),
        };
        page.stale_files = changed_backing_files(snapshot).await;
        Ok(page)
    }

    /// Bring just-edited files' rows in line with the bytes on disk:
    /// re-extract while a file exists and the last build's scope covers
    /// it, drop its rows when it doesn't. This is the edit flow's
    /// endpoint — a write becomes searchable immediately instead of
    /// leaving a hole until the next `index build`.
    ///
    /// A store with no completed build is left untouched: opening the DB
    /// for a read already materializes the file, so the build marker —
    /// not file existence — decides "never built", and an edit must not
    /// grow a 1-file index that would then answer authoritatively.
    ///
    /// Failures keep the old rows (the next read sees them flagged
    /// `stale`) and propagate to the caller, which logs the disclosed
    /// warn — the edit itself already succeeded and stays successful.
    pub async fn refresh_files(&self, paths: &[PathBuf]) -> Result<(), StoreError> {
        // A build owning the index cannot be joined, and its own pass will
        // not cover an edit made after it read the file — so the refresh is
        // refused rather than dropped, and the caller discloses it exactly
        // as it discloses any other refresh that could not run.
        let Some(_lock) = FileLock::shared(&Self::lock_path(&self.project_root))? else {
            return Err(StoreError::AlreadyIndexing);
        };
        let Some(scope) = self.build_scope().await? else {
            tracing::debug!("Skipping index refresh: the index was never built");
            return Ok(());
        };
        // One ignore-rules build for the whole batch — multi-file
        // operations (rename, actions apply) refresh many files at once.
        let filter = FileFilter::new(&self.project_root);
        let extensions = scope.extensions();

        let mut first_err: Option<StoreError> = None;
        let mut settled: Vec<String> = Vec::new();
        for path in paths {
            let result = if !Self::is_indexable(path, &extensions, &filter) {
                self.remove_file_rows(path).await
            } else {
                // Rows come out only for a file the tree definitively no
                // longer has. An existence probe that FAILS answers nothing —
                // a directory that will not open reads exactly like a deleted
                // file — and deleting durable rows on an unknown is the one
                // outcome that cannot be undone by retrying.
                match tokio::fs::try_exists(path).await {
                    Ok(true) => self.index_file(path).await,
                    Ok(false) => self.remove_file_rows(path).await,
                    Err(e) => Err(StoreError::Io(e)),
                }
            };
            match result {
                Ok(()) => settled.push(path.display().to_string()),
                Err(e) => {
                    tracing::debug!("Failed to refresh {} in index: {e}", path.display());
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        // The per-file failure is what the caller acts on, so it outranks a
        // failure to update the bookkeeping beside it.
        let forgotten = self.forget_unread_paths(settled).await;
        match first_err {
            Some(e) => Err(e),
            None => forgotten,
        }
    }

    /// Drop from the build's shortfall the paths a refresh has just settled.
    ///
    /// The shortfall says these paths are absent from the index. A refresh
    /// that read one — or established that the tree no longer holds it — has
    /// made that false for that path, and a shortfall outliving the failure it
    /// describes sends a reader to rebuild over a hole that is not there. What
    /// the refresh did not settle stays, because nothing about it changed.
    async fn forget_unread_paths(&self, paths: Vec<String>) -> Result<(), StoreError> {
        if paths.is_empty() {
            return Ok(());
        }
        self.db
            .call(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare("DELETE FROM unread_paths WHERE path = ?1")?;
                    for path in &paths {
                        stmt.execute(rusqlite::params![path])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    /// Whether the last build would cover this file: an extension inside
    /// the recorded build scope, not excluded by the project's ignore
    /// rules.
    fn is_indexable(path: &Path, scope_extensions: &[&str], filter: &FileFilter) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| scope_extensions.contains(&ext))
            && !filter.is_ignored(path)
    }

    /// The languages this index answers authoritatively for.
    ///
    /// Empty until a build completes, which is the same statement the store
    /// makes by reporting `NotInitialized` to a read: nothing here has been
    /// covered, so nothing here can be trusted as complete.
    pub async fn indexed_languages(&self) -> Result<Vec<Language>, StoreError> {
        Ok(self
            .build_scope()
            .await?
            .map(|scope| scope.languages())
            .unwrap_or_default())
    }

    /// The languages whose files the last completed build indexed for
    /// content — a content search's coverage, wider than
    /// [`indexed_languages`](Self::indexed_languages) by the languages the
    /// index holds text for without a symbol extractor.
    async fn build_scope(&self) -> Result<Option<BuildScope>, StoreError> {
        Ok(self
            .db
            .call(|conn| read_completed_build(conn))
            .await?
            .map(|build| build.scope))
    }

    /// Open the window in which an index is rebuilt: the completion marker
    /// comes off and the epoch advances, in one transaction, before the
    /// first row is touched. The returned epoch is the right to publish
    /// completion — [`publish_build`](Self::publish_build) spends it — so
    /// an index is marked whole only by the operation that last owned it,
    /// and a build interrupted, cleared, or overtaken leaves a store that
    /// reads as never built rather than as one whose marker outlives its
    /// rows.
    ///
    /// The index lock is what keeps two builds from overlapping; the
    /// epoch is what keeps a marker honest when the lock did not hold —
    /// a lock file deleted between two runs, a filesystem where the
    /// advisory lock does not carry. It guarantees that a completion
    /// marker was written by the operation that last owned the index, not
    /// that rows were written by only one: the lock owns that, and where
    /// the lock is gone an overtaken build's rows can survive under
    /// another's marker until the next build prunes them.
    async fn begin_build(&self, reindex: Reindex) -> Result<i64, StoreError> {
        self.db
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let epoch = read_build_epoch(&tx)? + 1;
                // The shortfall qualifies the completion marker, so it is
                // cleared with it: paths left behind would outlive the claim
                // they were about, and `clear` would leave some for an index
                // that no longer holds anything.
                tx.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    rusqlite::params![META_BUILD_SCOPE],
                )?;
                tx.execute("DELETE FROM unread_paths", [])?;
                if reindex == Reindex::FromScratch {
                    tx.execute("DELETE FROM content_lines", [])?;
                    // The text index derives from those rows, and its delete
                    // trigger records one removal per row — the right shape
                    // for a file, and a whole index's worth of tombstones
                    // when the table is emptied. Discarding it wholesale is
                    // what leaves nothing behind: tombstones are rows like
                    // any other, so an index cleared without this keeps most
                    // of its size while holding nothing.
                    tx.execute(
                        "INSERT INTO content_lines_fts(content_lines_fts) VALUES ('delete-all')",
                        [],
                    )?;
                    tx.execute("DELETE FROM symbols", [])?;
                    tx.execute("DELETE FROM files", [])?;
                }
                write_meta(&tx, META_BUILD_EPOCH, &epoch.to_string())?;
                tx.commit()?;
                Ok(epoch)
            })
            .await
    }

    async fn publish_build(
        &self,
        epoch: i64,
        scope: &BuildScope,
        fingerprint: String,
        unread_paths: Vec<UnreadPath>,
    ) -> Result<(), StoreError> {
        let value = scope.meta_value();
        let owned = self
            .db
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let owned = read_build_epoch(&tx)? == epoch;
                if owned {
                    write_meta(&tx, META_BUILD_SCOPE, &value)?;
                    write_meta(&tx, META_TREE_FINGERPRINT, &fingerprint)?;
                    // The rows qualify exactly the marker written beside them,
                    // so they are replaced wholesale in the same transaction:
                    // what an earlier build could not read says nothing about
                    // what this one could.
                    tx.execute("DELETE FROM unread_paths", [])?;
                    let mut stmt =
                        tx.prepare("INSERT INTO unread_paths (path, is_file) VALUES (?1, ?2)")?;
                    for unread in &unread_paths {
                        stmt.execute(rusqlite::params![unread.path, unread.is_file])?;
                    }
                    drop(stmt);
                }
                tx.commit()?;
                Ok(owned)
            })
            .await?;
        owned.then_some(()).ok_or(StoreError::AlreadyIndexing)
    }

    async fn remove_file_rows(&self, path: &Path) -> Result<(), StoreError> {
        let path_str = path.display().to_string();
        self.db
            .call(move |conn| {
                // Only "there is no such row" means there is nothing to
                // delete. Every other failure is the store refusing, and
                // reporting it as a completed removal would let a caller
                // record the path as settled while its rows are still there.
                let file_id = match conn.query_row(
                    "SELECT id FROM files WHERE path = ?1",
                    rusqlite::params![&path_str],
                    |r| r.get::<_, i64>(0),
                ) {
                    Ok(id) => Some(id),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(e) => return Err(e),
                };

                // One file's rows span three tables; a failure partway would
                // leave the index holding half a file, which no later run
                // would notice as anything but a whole one.
                if let Some(fid) = file_id {
                    let tx = conn.unchecked_transaction()?;
                    delete_file_and_data(&tx, fid)?;
                    tx.commit()?;
                }
                Ok(())
            })
            .await
    }

    /// Empty the index. A cleared store reads as never built, so a stray
    /// per-file refresh cannot start regrowing a partial index inside it,
    /// and a build this clear interrupted cannot mark what it left behind.
    pub async fn clear(&self) -> Result<(), StoreError> {
        let _lock = self.own_index().await?;
        self.begin_build(Reindex::FromScratch).await?;
        if let Err(e) = self.db.execute("VACUUM;").await {
            tracing::debug!("Failed to reclaim index space: {}", e);
        }
        Ok(())
    }

    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        self.db.execute("PRAGMA wal_checkpoint(TRUNCATE);").await
    }

    /// Hand the pages a build freed back to the filesystem. A build rewrites
    /// the rows it deleted, and the pages in between are the store's to
    /// return: kept, they would leave the index's size a record of how often
    /// it was rebuilt rather than of what it holds, and the disk a rebuild
    /// borrowed would never come back. The operation that rewrites the index
    /// is the one that settles its storage — a per-file refresh leaves its
    /// pages for the next build, which pays the relocation once instead of on
    /// every edit.
    ///
    /// Stepped to exhaustion, because the pragma yields a row per page it
    /// moves: run as a plain statement it gives back one page and leaves the
    /// rest, which is the shape of a build that reclaims nothing at all.
    async fn reclaim_free_pages(&self) -> Result<(), StoreError> {
        self.db
            .call(|conn| {
                let mut reclaim = conn.prepare("PRAGMA incremental_vacuum")?;
                let mut pages = reclaim.query([])?;
                while pages.next()?.is_some() {}
                Ok(())
            })
            .await
    }

    /// The index's scope, row counts, and size, read in one transaction so
    /// they describe the same committed state — a rebuild committing
    /// mid-read cannot make the report name one build's languages beside
    /// another build's counts. `is_indexing` answers the question the
    /// counts raise — could a build have been moving these numbers while
    /// they were read — so the lock is inspected on both sides of the
    /// snapshot and either sighting is the answer. Reading it once could
    /// only ever be true of an instant: before the snapshot it misses a
    /// build that starts during it, after the snapshot it misses one that
    /// ends during it, and each miss pairs "no build in progress" with a
    /// build's own half-written rows. The size is the database's
    /// own page count times page size — the logical size of the index,
    /// which every connection observing this state reports identically. The
    /// main file's length would not: under WAL, pages committed since the
    /// last checkpoint live in the log, so that number depends on when the
    /// last checkpoint ran rather than on what the index holds.
    pub async fn stats(&self) -> Result<IndexStats, StoreError> {
        let owned_before = self.build_in_progress();
        let mut stats = self
            .db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let build = read_completed_build(&tx)?;
                let languages = build
                    .as_ref()
                    .map(|build| build.scope.languages())
                    .unwrap_or_default();
                let unread_paths = build.map(|build| build.unread_paths).unwrap_or_default();
                let count_of = |table: &str| {
                    tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap_or(0) as usize
                };
                let pragma = |name: &str| {
                    tx.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0))
                        .unwrap_or(0) as u64
                };
                Ok(IndexStats {
                    file_count: count_of("files"),
                    symbol_count: count_of("symbols"),
                    content_line_count: count_of("content_lines"),
                    index_size_bytes: pragma("page_count") * pragma("page_size"),
                    last_indexed: tx
                        .query_row("SELECT COALESCE(MAX(indexed_at), 0) FROM files", [], |r| {
                            r.get::<_, i64>(0)
                        })
                        .unwrap_or(0) as u64,
                    is_indexing: false,
                    languages,
                    unread_paths,
                })
            })
            .await?;
        stats.is_indexing = owned_before || self.build_in_progress();
        Ok(stats)
    }

    pub async fn index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        let _lock = self.own_index().await?;
        self.do_index(options).await
    }

    /// Whether a build owns the index right now — a fact any process can read,
    /// so a status query sees a daemon's build as readily as its own.
    ///
    /// A lock this process cannot take leaves a reader exactly where a platform
    /// without locking does: unable to observe a build, and unable to be one.
    /// Both answer "none in progress" rather than failing the read that asked,
    /// because a tree whose `.symora` is not writable is the ordinary case —
    /// another user's checkout, a read-only mount, a restored cache — and a
    /// reader there wants the index reported, not an I/O error.
    fn build_in_progress(&self) -> bool {
        FileLock::shared(&Self::lock_path(&self.project_root)).is_ok_and(|holder| holder.is_none())
    }

    /// Take the index for an operation that rewrites it. Another such
    /// operation holds the lock for as long as it works, and is reported
    /// as one; anything else holding it is momentary and waited out.
    async fn own_index(&self) -> Result<FileLock, StoreError> {
        let lock_path = Self::lock_path(&self.project_root);
        let deadline = tokio::time::Instant::now() + INDEX_LOCK_WAIT;
        loop {
            if let Some(lock) = FileLock::exclusive(&lock_path)? {
                return Ok(lock);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(StoreError::AlreadyIndexing);
            }
            tokio::time::sleep(LOCK_POLL_INTERVAL).await;
        }
    }

    async fn do_index(&self, options: IndexOptions) -> Result<IndexStats, StoreError> {
        let filter = FileFilter::new(&self.project_root);
        let scope = BuildScope::from_options(&options.languages);
        let extensions = scope.extensions();

        // A scope naming no language covers no file, and every step below reads
        // an empty set as its opposite: the walker takes an empty extension
        // list for "every extension", and the prune takes an empty discovery
        // for "the tree is empty" and deletes the whole index. Refused here so
        // neither reading can be reached, and so a caller that meant a scope
        // learns it named none rather than finding an emptied store behind a
        // completion marker.
        if extensions.is_empty() {
            return Err(StoreError::EmptyScope);
        }
        let Discovery {
            files,
            unreadable: unreadable_dirs,
        } = filter.discover_files(&extensions);
        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let fingerprint = tree_fingerprint(&self.project_root, &files);

        let epoch = self
            .begin_build(if options.force {
                Reindex::FromScratch
            } else {
                Reindex::InPlace
            })
            .await?;
        // Pruning reads "absent from the walk" as "gone from the tree", which
        // holds only where the walk went. It is scoped to that rather than
        // refused outright: a single directory the walk could not enter would
        // otherwise keep every deleted file's rows alive across the whole
        // project, which leaves the index answering for files that are gone.
        if !options.force {
            let hidden: Vec<PathBuf> = unreadable_dirs.clone();
            self.prune_deleted_files(&discovered_paths, &hidden).await?;
        }

        // Each future acquires its own permit when polled, so the semaphore
        // gates the fan-out: futures are created up-front, permits taken and
        // released as `join_all` drives them. Acquiring before the future is
        // built would instead reserve every permit eagerly and stall once the
        // file count crossed the concurrency cap.
        let concurrency = self.config.index_concurrency.max(1);
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
        let tasks: Vec<_> = files
            .iter()
            .map(|file| {
                let sem = std::sync::Arc::clone(&semaphore);
                async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    self.index_file(file).await
                }
            })
            .collect();
        // `join_all` preserves the order it was given, so each verdict pairs
        // with the file it is about — which is what lets a path the build
        // could not read be named, and later repaired one at a time.
        // What the WALK could not enter is recorded as a non-file: it may be a
        // directory, and nothing can tell now, so the safe reading is that it
        // could hold any language. What a READ could not open is a file by
        // construction, and its name settles which language it kept out.
        let mut unread_paths: Vec<UnreadPath> = unreadable_dirs
            .iter()
            .map(|path| UnreadPath {
                path: path.display().to_string(),
                is_file: false,
            })
            .collect();
        for (file, result) in files.iter().zip(futures::future::join_all(tasks).await) {
            match result {
                Ok(()) => {}
                // The only `io::Error` a file's indexing can raise is the
                // read of the file itself, which is the hole this records.
                Err(StoreError::Io(e)) => {
                    unread_paths.push(UnreadPath {
                        path: file.display().to_string(),
                        is_file: true,
                    });
                    tracing::warn!("Failed to read file for indexing: {}", e);
                }
                // The store refused the write. Recording it as an unreadable
                // path would blame the filesystem for a store failure, and
                // publishing afterwards would claim a scope whose rows were
                // never written — so the build fails as what it is.
                Err(e) => return Err(e),
            }
        }
        unread_paths.sort_by(|a, b| a.path.cmp(&b.path));
        unread_paths.dedup_by(|a, b| a.path == b.path);

        self.publish_build(epoch, &scope, fingerprint, unread_paths)
            .await?;
        let _ = self.reclaim_free_pages().await;
        let _ = self.checkpoint().await;
        self.stats().await
    }

    /// Whether the tree still looks the way the last completed build read it.
    ///
    /// The walk is the build's own — same scope, so a language the build never
    /// covered cannot make the index look behind — and the comparison is
    /// stat-only: no file is opened. `false` wherever the question cannot be
    /// settled: no build has completed, one is running now, or this binary
    /// wrote no fingerprint. An index reported current is one whose zero can
    /// be published without a language server to confirm it, so every
    /// unsettled case has to fall on this side.
    ///
    /// A build is the only thing that records a fingerprint. A per-file
    /// refresh deliberately does not: it brings the files it was given in
    /// line, which is not the whole-tree correspondence this claims.
    pub async fn tree_is_current(&self) -> Result<bool, StoreError> {
        if self.build_in_progress() {
            return Ok(false);
        }
        let recorded = self
            .db
            .call(|conn| {
                Ok(read_completed_build(conn)?.zip(read_meta(conn, META_TREE_FINGERPRINT)?))
            })
            .await?;
        let Some((build, fingerprint)) = recorded else {
            return Ok(false);
        };
        let root = self.project_root.clone();
        let current = tokio::task::spawn_blocking(move || {
            let extensions = build.scope.extensions();
            let Discovery { files, .. } = FileFilter::new(&root).discover_files(&extensions);
            tree_fingerprint(&root, &files)
        })
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(current == fingerprint)
    }

    async fn index_file(&self, path: &Path) -> Result<(), StoreError> {
        // A file over the configured ceiling is outside the search's domain,
        // the same verdict the AST and language-server readers give it. Read
        // before the bytes are: a generated data file is megabytes on one
        // line, and parsing it to decide costs more than every file that
        // belongs in the index put together.
        if let Ok(meta) = tokio::fs::metadata(path).await
            && meta.len() > self.config.max_file_size_bytes
        {
            tracing::debug!(
                "Dropping file over search.max_file_size_mb ({}MB): {}",
                meta.len() / 1024 / 1024,
                path.display()
            );
            return self.remove_file_rows(path).await;
        }

        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            // The tree settled this path: nothing is there, or what is there
            // is not text. Rows a textual past left must go, or the index
            // keeps serving lines the scan no longer sees.
            Err(e) if !crate::infra::hides_text(&e) => {
                tracing::debug!("Dropping non-text file {}: {}", path.display(), e);
                return self.remove_file_rows(path).await;
            }
            // The file is there and this build could not read it. The
            // scope about to be published would claim a language whose
            // files it did not all see, so the failure is carried up and
            // counted rather than logged away.
            Err(e) => return Err(StoreError::Io(e)),
        };
        // A NUL byte marks the file as binary (git's own test), and binary
        // content is outside the search domain on BOTH backends — SQLite's
        // LENGTH() also stops at NUL, so admitting such a row would give
        // the index and the scan different ideas of the same line. The
        // verdict is definitive, so any rows from a textual past go too.
        if content.contains('\0') {
            tracing::debug!("Dropping binary file {}", path.display());
            return self.remove_file_rows(path).await;
        }

        let content_hash = crate::infra::hash_content(&content) as i64;

        let language = Language::from_path(path);
        let lang_str = match language {
            Language::Unknown => None,
            l => Some(l.lsp_id().to_string()),
        };
        let file_path = path.display().to_string();

        // Skip files whose content and language already match the index,
        // before paying for extraction or a write.
        if self.is_current(&file_path, content_hash, &lang_str).await? {
            return Ok(());
        }

        let symbols = SymbolExtractor::shared().extract(path, &content, language);
        let content_lines: Vec<(i32, String)> = if self.config.index_content {
            content
                .lines()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(|(i, line)| ((i + 1) as i32, line.to_string()))
                .collect()
        } else {
            Vec::new()
        };
        let now = now_unix() as i64;

        // The file row and every row derived from it are written in one
        // transaction. A failed insert rolls back the content-hash stamp, so
        // a file is never recorded as current with missing symbols — it is
        // simply re-indexed on the next pass instead of silently disappearing.
        self.db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let file_id: i64 = tx.query_row(
                    "INSERT INTO files (path, content_hash, language, indexed_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(path) DO UPDATE SET
                         content_hash = excluded.content_hash,
                         language = excluded.language,
                         indexed_at = excluded.indexed_at
                     RETURNING id",
                    rusqlite::params![file_path, content_hash, lang_str, now],
                    |r| r.get(0),
                )?;
                delete_file_related_data(&tx, file_id)?;
                {
                    // A duplicate (line, col) is a benign extractor overlap
                    // (container and leaf at the same position); drop the
                    // extra rather than aborting the whole file.
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO symbols (file_id, name, name_path, kind, container, line, col) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )?;
                    for s in &symbols {
                        stmt.execute(rusqlite::params![
                            file_id,
                            s.name,
                            s.name_path,
                            s.kind.to_string(),
                            s.container,
                            s.location.line as i32,
                            s.location.column as i32
                        ])?;
                    }
                }
                if !content_lines.is_empty() {
                    let mut stmt = tx.prepare(
                        "INSERT INTO content_lines (file_id, line_num, content) VALUES (?1, ?2, ?3)",
                    )?;
                    for (line_num, line) in &content_lines {
                        stmt.execute(rusqlite::params![file_id, line_num, line])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    /// True when the file's content hash and language already match the
    /// stored row, so identical content is never re-extracted and any change
    /// to the bytes is always re-indexed. The content hash is the currency
    /// key, which keeps even same-size, rapid edits from slipping through.
    async fn is_current(
        &self,
        file_path: &str,
        content_hash: i64,
        lang_str: &Option<String>,
    ) -> Result<bool, StoreError> {
        let file_path = file_path.to_string();
        let lang_str = lang_str.clone();
        self.db
            .call(move |conn| {
                let existing = conn
                    .query_row(
                        "SELECT content_hash, language FROM files WHERE path = ?1",
                        rusqlite::params![file_path],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
                    )
                    .optional()?;
                Ok(match existing {
                    Some((existing_hash, existing_lang)) => {
                        existing_hash == content_hash && existing_lang == lang_str
                    }
                    None => false,
                })
            })
            .await
    }

    async fn prune_deleted_files(
        &self,
        discovered_paths: &std::collections::HashSet<String>,
        unwalked: &[PathBuf],
    ) -> Result<(), StoreError> {
        let known_files: Vec<(i64, String)> = self
            .db
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT id, path FROM files")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows.filter_map(Result::ok).collect::<Vec<_>>())
            })
            .await?;

        // Two ways a row is stale, and both must be DEFINITE. A path missing
        // from the walk that produced `discovered_paths` is genuinely gone —
        // unless it sits under one the walk could not enter, where "not
        // discovered" says nothing about the tree and deleting would destroy
        // durable rows on a permission change. The second check catches a file
        // deleted since that walk, and only a probe that answers counts:
        // `exists()` folds a metadata error into "absent", which would delete
        // rows because a permission changed under a file that is still there.
        let stale: Vec<i64> = known_files
            .into_iter()
            .filter(|(_, path)| {
                let walked = !unwalked
                    .iter()
                    .any(|unwalked| Path::new(path).starts_with(unwalked));
                (walked && !discovered_paths.contains(path))
                    || matches!(Path::new(path).try_exists(), Ok(false))
            })
            .map(|(id, _)| id)
            .collect();

        if stale.is_empty() {
            return Ok(());
        }

        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                for id in stale {
                    delete_file_and_data(&tx, id)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;

        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Snapshot the indexed `content_hash` of each distinct file backing a
/// search page, read in the same connection closure as the rows themselves
/// so the pair is atomic against a concurrent reindex. A file absent from
/// the index maps to `None`.
fn indexed_hashes<'a>(
    conn: &rusqlite::Connection,
    files: impl Iterator<Item = &'a PathBuf>,
) -> Result<Vec<(String, Option<i64>)>, rusqlite::Error> {
    let mut distinct: Vec<String> = files.map(|p| p.display().to_string()).collect();
    distinct.sort_unstable();
    distinct.dedup();

    let mut stmt = conn.prepare("SELECT content_hash FROM files WHERE path = ?1")?;
    distinct
        .into_iter()
        .map(|path| {
            let stored: Option<i64> = stmt
                .query_row(rusqlite::params![path], |r| r.get(0))
                .optional()?;
            Ok((path, stored))
        })
        .collect()
}

/// The files in the snapshot that no longer match the indexed hash their rows
/// were served under — rewritten, deleted, or unreadable since `index()` ran.
/// Biased toward false positives on purpose: a spurious stale banner is
/// harmless, stale rows presented as current are not.
///
/// Named rather than counted, because a caller that filters the page has to
/// narrow the question to the files its answer kept: a page holding one stale
/// row and one fresh one says nothing about an answer that emitted only the
/// fresh one. Cost is one disk read per distinct backing file, bounded by the
/// page size — the same read the fresh case always paid.
async fn changed_backing_files(snapshot: Vec<(String, Option<i64>)>) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, indexed_hash) in snapshot {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                if indexed_hash != Some(crate::infra::hash_content(&content) as i64) {
                    changed.push(path);
                }
            }
            // Deleted or unreadable: the row is no longer backed by disk.
            Err(_) => changed.push(path),
        }
    }
    changed
}

fn delete_file_related_data(
    conn: &rusqlite::Connection,
    file_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM symbols WHERE file_id = ?1",
        rusqlite::params![file_id],
    )?;
    conn.execute(
        "DELETE FROM content_lines WHERE file_id = ?1",
        rusqlite::params![file_id],
    )?;
    Ok(())
}

fn delete_file_and_data(conn: &rusqlite::Connection, file_id: i64) -> Result<(), rusqlite::Error> {
    delete_file_related_data(conn, file_id)?;
    conn.execute(
        "DELETE FROM files WHERE id = ?1",
        rusqlite::params![file_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refresh removes a file's rows for exactly one reason: the tree no
    /// longer has the file. An existence probe that FAILS is not that reason —
    /// a directory that will not open reads like a deletion — and rows are
    /// durable, so the wrong call here destroys indexed data that no retry
    /// brings back.
    #[cfg(unix)]
    /// A file over the ceiling is outside the search's domain, so the index
    /// holds no rows for it — and a build that meets one after a smaller past
    /// drops the rows that past left, or a search keeps answering from
    /// content the domain no longer covers.
    #[tokio::test]
    async fn a_file_over_the_ceiling_is_outside_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let small = root.join("small.py");
        let large = root.join("large.py");
        std::fs::write(&small, "def kept():\n    return 1\n").unwrap();
        std::fs::write(&large, "def dropped():\n    return 1\n").unwrap();

        let store = Store::open(
            root,
            StoreConfig {
                max_file_size_bytes: 1024,
                ..StoreConfig::default()
            },
        )
        .await
        .unwrap();
        let both = store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(both.file_count, 2);

        std::fs::write(&large, "x".repeat(4096)).unwrap();
        let after = store
            .index(IndexOptions {
                force: true,
                languages: None,
            })
            .await
            .unwrap();
        assert_eq!(after.file_count, 1, "the oversized file keeps no rows");
    }

    #[tokio::test]
    async fn a_refresh_that_cannot_tell_whether_a_file_exists_keeps_its_rows() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        let before = store.search_symbols("alpha", 10, None, None).await.unwrap();
        assert_eq!(before.total, 1);

        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000)).unwrap();
        let probe_still_answers = tokio::fs::try_exists(&file).await.is_ok();
        let refreshed = store.refresh_files(std::slice::from_ref(&file)).await;
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();

        if probe_still_answers {
            // Running with a user the mode bits do not constrain (root), so
            // there is no unknown to preserve rows through.
            return;
        }
        assert!(
            matches!(refreshed, Err(StoreError::Io(_))),
            "an unreadable path is surfaced, not silently treated as a deletion"
        );
        let after = store.search_symbols("alpha", 10, None, None).await.unwrap();
        assert_eq!(
            after.total, 1,
            "rows survive a refresh that could not learn whether the file is gone"
        );
    }

    /// A directory the walk could not enter says nothing about the tree
    /// outside it, so pruning is scoped to where the walk went rather than
    /// refused outright. Refusing it leaves every deleted file's rows alive
    /// across the whole project on account of one blocked directory, and the
    /// index then answers for files that are gone — while its shortfall calls
    /// the count a lower bound.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_blocked_directory_does_not_keep_deleted_rows_alive_elsewhere() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let doomed = root.join("doomed.rs");
        tokio::fs::write(&doomed, "fn doomed() {}\n").await.unwrap();
        tokio::fs::write(root.join("kept.rs"), "fn kept() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(
            store
                .search_symbols("doomed", 10, None, None)
                .await
                .unwrap()
                .total,
            1
        );

        // The file goes, and a directory unrelated to it becomes unreadable
        // between one build and the next.
        tokio::fs::remove_file(&doomed).await.unwrap();
        let blocked = root.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        tokio::fs::write(blocked.join("b.rs"), "fn beta() {}\n")
            .await
            .unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mode_bites = std::fs::read_dir(&blocked).is_err();

        store.index(IndexOptions::default()).await.unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        if !mode_bites {
            return;
        }
        assert_eq!(
            store
                .search_symbols("doomed", 10, None, None)
                .await
                .unwrap()
                .total,
            0,
            "the walk reached this file's directory, so its absence is a deletion"
        );
        assert_eq!(
            store
                .search_symbols("kept", 10, None, None)
                .await
                .unwrap()
                .total,
            1,
            "and nothing the walk did see was taken with it"
        );
    }

    /// The other half of the same rule. Inside a directory the walk could not
    /// enter, "absent from the walk" is not evidence of anything, and rows are
    /// durable: deleting them because a permission changed is the one outcome
    /// no retry undoes.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_blocked_directory_keeps_the_rows_of_what_it_hides() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let blocked = root.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        tokio::fs::write(blocked.join("hidden.rs"), "fn hidden() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(
            store
                .search_symbols("hidden", 10, None, None)
                .await
                .unwrap()
                .total,
            1
        );

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mode_bites = std::fs::read_dir(&blocked).is_err();
        store.index(IndexOptions::default()).await.unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        if !mode_bites {
            return;
        }
        assert_eq!(
            store
                .search_symbols("hidden", 10, None, None)
                .await
                .unwrap()
                .total,
            1,
            "a permission change is not a deletion"
        );
    }

    /// A shortfall states that paths are absent from the index. A refresh that
    /// reads one has made that false for that path, and a statement that
    /// outlives the failure it describes sends a reader to rebuild over a hole
    /// that is no longer there — the fourth way a disclosure goes wrong, a past
    /// fact standing in for a present one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refresh_settles_the_path_it_repaired_out_of_the_builds_shortfall() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("a.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        let blocked = root.join("b.rs");
        tokio::fs::write(&blocked, "fn beta() {}\n").await.unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Probed against the tree, never read off the output under test: a
        // guard that consults the answer would take a defect for the
        // environment and pass silently.
        let mode_bites = std::fs::read_to_string(&blocked).is_err();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        let built = store.stats().await.unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();

        if !mode_bites {
            return;
        }
        assert_eq!(
            built.unread_paths,
            vec![UnreadPath {
                path: blocked.display().to_string(),
                is_file: true,
            }],
            "a file the build could not open is recorded as one, so its name settles its language"
        );

        store
            .refresh_files(std::slice::from_ref(&blocked))
            .await
            .unwrap();
        assert!(
            store.stats().await.unwrap().unread_paths.is_empty(),
            "the path the refresh read is no longer one the index is missing"
        );
        assert_eq!(
            store
                .search_symbols("beta", 10, None, None)
                .await
                .unwrap()
                .total,
            1,
            "and its symbols are there to be found"
        );
    }

    /// The fact a symbol search leans on when it keeps a live workspace-symbol
    /// row for a file whose extension names no language: such a file yields no
    /// row at all, so the index and a live answer can never count it twice.
    /// Asserted against a real build rather than against a predicate, because
    /// the guarantee is a property of the whole path — discovery is by the
    /// scope's extensions, and a name that matches none of them never reaches
    /// extraction — and only a build exercises that path end to end.
    #[tokio::test]
    async fn a_file_whose_extension_names_no_language_yields_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("runner"), "#!/bin/sh\nalpha() { :; }\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        let page = store.search_symbols("alpha", 10, None, None).await.unwrap();
        assert_eq!(page.total, 1);
        assert!(
            page.rows.iter().all(|row| row.file.ends_with("lib.rs")),
            "an unrecognised path must contribute no symbol row: {:?}",
            page.rows.iter().map(|r| &r.file).collect::<Vec<_>>()
        );
        assert_eq!(
            store.stats().await.unwrap().file_count,
            1,
            "the file was never discovered, so it has no row of any kind"
        );
    }

    /// A scope that names no language covers no file, and every step below
    /// reads an empty set as its opposite: the walker takes an empty extension
    /// list for "every extension", and the prune takes an empty discovery for
    /// "the tree is empty". Acted on, it would empty the index and publish a
    /// completion marker over nothing — after which every refresh routes to a
    /// row removal and the store stays empty behind a marker claiming a build.
    /// So it is refused, and what was already indexed is still there.
    #[tokio::test]
    async fn a_scope_that_names_no_language_is_refused_rather_than_acted_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(store.stats().await.unwrap().file_count, 1);

        let refused = store
            .index(IndexOptions {
                force: false,
                languages: Some(Vec::new()),
            })
            .await;
        assert!(
            matches!(refused, Err(StoreError::EmptyScope)),
            "a scope covering nothing is an input error, not an empty build: {refused:?}"
        );
        assert_eq!(
            store.stats().await.unwrap().file_count,
            1,
            "and it leaves the index it could not have covered exactly as it was"
        );
        assert_eq!(
            store
                .search_symbols("alpha", 10, None, None)
                .await
                .unwrap()
                .total,
            1,
            "still answering, still marked complete"
        );
    }

    async fn total_matches(store: &Store, query: &str) -> usize {
        store
            .search_symbols(query, 50, None, None)
            .await
            .unwrap()
            .total
    }

    /// Run the content-search SQL directly with a chosen `use_fts`, returning
    /// comparable (content, line, path, score) rows — so a test can assert the
    /// FTS pre-filter is set-identical to the LIKE-only scan.
    async fn content_rows(store: &Store, query: &str, use_fts: bool) -> Vec<(String, i64, f64)> {
        let q = query.to_string();
        store
            .db
            .call(move |conn| {
                let sql = build_content_search_query(0, use_fts);
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![q, 10_000_i64], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, f64>(5)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }

    async fn fts_row_count(store: &Store) -> i64 {
        store
            .db
            .call(|conn| conn.query_row("SELECT count(*) FROM content_lines_fts", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    async fn content_row_count(store: &Store) -> i64 {
        store
            .db
            .call(|conn| conn.query_row("SELECT count(*) FROM content_lines", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    /// A LIMIT under a score tie must keep a well-defined row. Both search
    /// queries order by path and position after score, so which equal-score
    /// row survives the cap is a property of the data — never of the physical
    /// row order a rebuild or a different query plan happens to produce.
    #[tokio::test]
    async fn tied_scores_keep_the_path_ascending_row_under_a_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for file in ["b.rs", "a.rs"] {
            tokio::fs::write(root.join(file), "fn same_probe() {}\n")
                .await
                .unwrap();
        }
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        let symbols = store
            .search_symbols("same_probe", 1, None, None)
            .await
            .unwrap();
        assert_eq!(symbols.total, 2);
        assert_eq!(symbols.rows.len(), 1);
        assert!(symbols.rows[0].file.ends_with("a.rs"));

        let content = store
            .search_content("same_probe", 1, &[Language::Rust])
            .await
            .unwrap();
        assert_eq!(content.total, 2);
        assert_eq!(content.rows.len(), 1);
        assert!(content.rows[0].file.ends_with("a.rs"));
    }

    /// The FTS trigram pre-filter must return exactly the same rows, scores, and
    /// order as the LIKE-only scan for every >= 3-char query, including
    /// FTS-syntax characters, mixed case, and non-ASCII text. Any divergence
    /// means the index path silently disagrees with its own authority, so the
    /// set-equality check below is the gate that keeps them honest.
    #[tokio::test]
    async fn fts_prefilter_is_set_identical_to_like_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(
            root.join("corpus.rs"),
            "fn foo_bar() {}\n\
             let Foo = 1;\n\
             const BAR_BAZ = 2;\n\
             let value = cafe_latte();\n\
             let accented = \"café\";\n\
             fn 안녕하세요() {}\n\
             // punctuation a:b and x-y and (z) here\n\
             match thing { OR_AND_NOT => 0 }\n\
             let repeated = foo_bar_foo();\n\
             let near_miss = fooXbar();\n\
             let pct = \"100%off\";\n\
             let pct_miss = \"100Xoff\";\n",
        )
        .await
        .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // Every probe is >= 3 chars (the production FTS threshold): real
        // substrings across case, snake_case, accents, CJK, FTS-syntax chars,
        // and a guaranteed non-match.
        let probes = [
            "foo",
            "Foo",
            "FOO",
            "foo_bar",
            "bar_baz",
            "value",
            "café",
            "안녕하",
            "NOT",
            "a:b",
            "x-y",
            "(z)",
            "OR_",
            "100%off",
            "zzz_nomatch",
        ];
        for q in probes {
            let fts = content_rows(&store, q, true).await;
            let like = content_rows(&store, q, false).await;
            assert_eq!(
                fts, like,
                "FTS pre-filter diverged from LIKE-only for {q:?}"
            );
        }

        // Literal-substring correctness: `_` and `%` are content, never LIKE
        // wildcards, so a query must NOT match a line where the metachar
        // position holds a different character. The corpus seeds near-misses
        // (`fooXbar`, `100Xoff`) that an unescaped wildcard LIKE would wrongly
        // catch — this proves the ESCAPE on both the FTS and LIKE-only paths.
        let has = |rows: &[(String, i64, f64)], needle: &str| {
            rows.iter().any(|(c, _, _)| c.contains(needle))
        };
        for use_fts in [true, false] {
            let underscore = content_rows(&store, "foo_bar", use_fts).await;
            assert!(
                has(&underscore, "foo_bar"),
                "literal foo_bar must match (use_fts={use_fts})"
            );
            assert!(
                !has(&underscore, "fooXbar"),
                "`_` must not act as a wildcard (use_fts={use_fts})"
            );

            let percent = content_rows(&store, "100%off", use_fts).await;
            assert!(
                has(&percent, "100%off"),
                "literal 100%off must match (use_fts={use_fts})"
            );
            assert!(
                !has(&percent, "100Xoff"),
                "`%` must not act as a wildcard (use_fts={use_fts})"
            );
        }
    }

    /// The FTS index is external-content and trigger-maintained, so it must
    /// stay row-for-row in sync with content_lines through both the per-file
    /// delete and the bulk clear path.
    #[tokio::test]
    async fn fts_index_stays_in_sync_with_content_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\nfn beta() {}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        assert!(content_row_count(&store).await > 0);
        assert_eq!(fts_row_count(&store).await, content_row_count(&store).await);

        // Delete the file's rows via refresh — triggers must purge FTS too.
        tokio::fs::remove_file(&file).await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(content_row_count(&store).await, 0);
        assert_eq!(
            fts_row_count(&store).await,
            0,
            "FTS rows orphaned after delete"
        );
    }

    /// Sub-3-char queries have no trigrams, so production routes them to the
    /// The SQL relevance TRIM strips the same whitespace set the scan
    /// scorer strips — tab and space, `char(9,32)` — so a tab-indented
    /// line scores 1.0 from the index exactly as it does from a tree
    /// scan, and the two backends stay rank-identical.
    #[tokio::test]
    async fn a_tab_indented_line_scores_as_the_scan_scores_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn probe() {\n\ttab_probe();\n}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        let page = store
            .search_content("tab_probe", 10, &[Language::Rust])
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].score, 1.0);
    }

    /// Readiness is the durable marker, not a fact any handle caches: a
    /// store opened before a build was made elsewhere answers from that
    /// build, and one whose build is still in flight answers from nothing.
    /// The daemon and a direct run are two such handles.
    #[tokio::test]
    async fn readiness_follows_the_store_not_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn handle_probe() {}\n")
            .await
            .unwrap();

        let reader = Store::open(root, StoreConfig::default()).await.unwrap();
        assert!(matches!(
            reader
                .search_content("handle_probe", 10, &[Language::Rust])
                .await,
            Err(StoreError::NotInitialized)
        ));

        let builder = Store::open(root, StoreConfig::default()).await.unwrap();
        builder.index(IndexOptions::default()).await.unwrap();

        let page = reader
            .search_content("handle_probe", 10, &[Language::Rust])
            .await
            .unwrap();
        assert_eq!(page.total, 1);

        reader.begin_build(Reindex::InPlace).await.unwrap();
        assert!(matches!(
            builder
                .search_content("handle_probe", 10, &[Language::Rust])
                .await,
            Err(StoreError::NotInitialized)
        ));
        assert!(matches!(
            builder.search_symbols("handle_probe", 10, None, None).await,
            Err(StoreError::NotInitialized)
        ));
    }

    /// Replacing the database is reserved for a file that cannot be read
    /// as one. A store that merely cannot be opened right now — another
    /// process is writing it — is intact, and replacing it would turn a
    /// passing contention into permanent loss.
    #[tokio::test]
    async fn only_a_file_that_is_not_a_database_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn recover_probe() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        drop(store);

        let db_path = Store::db_path(root);
        tokio::fs::write(&db_path, b"not a database at all")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        let backup = db_path.with_extension("db.bak");
        assert!(backup.exists());
        assert!(store.build_scope().await.unwrap().is_none());
        // The replacement is usable, which is what the stale sidecars prevented.
        store.index(IndexOptions::default()).await.unwrap();
    }

    /// A write-ahead-log database is three files. Moving only the one named
    /// `.db` leaves a `-wal` holding committed pages next to a database that
    /// no longer has them, which SQLite reads as an I/O error on the
    /// replacement — so the store cannot be rebuilt at exactly the moment a
    /// schema change says it must be.
    #[tokio::test]
    async fn a_database_moved_aside_takes_its_write_ahead_log_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.db");
        let backup_path = db_path.with_extension("db.bak");
        for suffix in SQLITE_SIDECARS {
            tokio::fs::write(sidecar_path(&db_path, suffix), suffix.as_bytes())
                .await
                .unwrap();
        }

        move_database_aside(&db_path, &backup_path).await;

        for suffix in SQLITE_SIDECARS {
            assert!(
                !sidecar_path(&db_path, suffix).exists(),
                "`{suffix}` was left where the replacement is about to be created"
            );
            assert_eq!(
                tokio::fs::read(sidecar_path(&backup_path, suffix))
                    .await
                    .ok()
                    .as_deref(),
                Some(suffix.as_bytes()),
                "`{suffix}` did not travel with the database it completes"
            );
        }
    }

    /// The classification the replacement decision rests on: only SQLite's
    /// own verdict that the file is not a database sends it down the
    /// destructive path, and a locked store is a retryable condition
    /// rather than a broken one.
    #[test]
    fn store_errors_carry_what_the_caller_must_decide_on() {
        let sqlite = |code: rusqlite::ErrorCode| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code,
                    extended_code: 0,
                },
                None,
            )
        };
        assert!(matches!(
            super::super::db::classify(sqlite(rusqlite::ErrorCode::NotADatabase)),
            StoreError::Corrupt(_)
        ));
        assert!(matches!(
            super::super::db::classify(sqlite(rusqlite::ErrorCode::DatabaseCorrupt)),
            StoreError::Corrupt(_)
        ));
        assert!(matches!(
            super::super::db::classify(sqlite(rusqlite::ErrorCode::DatabaseBusy)),
            StoreError::Busy
        ));
        assert!(matches!(
            super::super::db::classify(sqlite(rusqlite::ErrorCode::DatabaseLocked)),
            StoreError::Busy
        ));
        assert!(matches!(
            super::super::db::classify(sqlite(rusqlite::ErrorCode::PermissionDenied)),
            StoreError::Database(_)
        ));
    }

    /// One build owns the index at a time, across processes as well as
    /// within one: a second build meeting a held lock is refused rather
    /// than left to prune and write rows under the first one's feet.
    #[tokio::test]
    async fn a_second_build_is_refused_while_one_owns_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn lock_probe() {}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();

        let held = FileLock::exclusive(&Store::lock_path(root)).expect("uncontended");
        assert!(matches!(
            store.index(IndexOptions::default()).await,
            Err(StoreError::AlreadyIndexing)
        ));
        assert!(matches!(
            store.clear().await,
            Err(StoreError::AlreadyIndexing)
        ));
        drop(held);

        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(store.build_scope().await.unwrap(), Some(BuildScope::All));
    }

    /// A build only marks the index whole while it still owns the epoch it
    /// opened with. Anything that destroys rows in between — a clear, a
    /// later build — takes that ownership, so the loser leaves a store that
    /// reads as never built instead of one marked over rows it lost.
    #[tokio::test]
    async fn a_superseded_build_cannot_mark_the_index_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn epoch_probe() {}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();

        let overtaken = store.begin_build(Reindex::InPlace).await.unwrap();
        store.clear().await.unwrap();
        assert!(matches!(
            store
                .publish_build(overtaken, &BuildScope::All, String::new(), Vec::new())
                .await,
            Err(StoreError::AlreadyIndexing)
        ));
        assert!(store.build_scope().await.unwrap().is_none());

        let owned = store.begin_build(Reindex::InPlace).await.unwrap();
        store
            .publish_build(owned, &BuildScope::All, String::new(), Vec::new())
            .await
            .unwrap();
        assert_eq!(store.build_scope().await.unwrap(), Some(BuildScope::All));
    }

    /// The index answers for the requested languages it actually covers,
    /// and says which those are. Rows a narrower build left outside its
    /// scope stay invisible, so the caller's live read of the remainder can
    /// never double-serve a file.
    #[tokio::test]
    async fn a_narrowed_build_answers_only_within_the_scope_it_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn scope_probe() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("app.py"), "def scope_probe(): pass\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        store
            .index(IndexOptions {
                force: false,
                languages: Some(vec![Language::Rust]),
            })
            .await
            .unwrap();

        let page = store
            .search_content("scope_probe", 10, &[Language::Rust, Language::Python])
            .await
            .unwrap();
        assert_eq!(page.covered, vec![Language::Rust]);
        assert_eq!(page.total, 1);
        assert!(page.rows[0].file.ends_with("lib.rs"));

        let uncovered = store
            .search_content("scope_probe", 10, &[Language::Python])
            .await
            .unwrap();
        assert!(uncovered.covered.is_empty());
        assert_eq!(uncovered.total, 0);
    }

    /// LIKE-only path; the result must equal the LIKE-only scan (a non-empty
    /// 2-char substring still returns its matches, not a silent zero).
    #[tokio::test]
    async fn short_queries_use_like_only_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn fox() {}\nlet ok = 1;\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // "fn" is 2 chars: search_content must gate it onto the LIKE path and
        // still find the line, identical to the LIKE-only scan.
        let via_api = store
            .search_content("fn", 50, &[Language::Rust])
            .await
            .unwrap();
        let like_only = content_rows(&store, "fn", false).await;
        assert_eq!(via_api.total, like_only.len());
        assert!(via_api.total > 0);
    }

    #[tokio::test]
    async fn reindex_tracks_content_not_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 1);
        assert_eq!(total_matches(&store, "beta").await, 0);

        // Rewrite with different content. Even where the filesystem preserves
        // the modification time, the content hash differs, so the stored
        // symbol set is replaced rather than left stale.
        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 0);
        assert_eq!(total_matches(&store, "beta").await, 1);

        // Re-indexing identical content is a no-op: the same hash skips
        // extraction, so the symbol count stays put with no duplicate rows.
        let before = store.stats().await.unwrap().symbol_count;
        store.index(IndexOptions::default()).await.unwrap();
        let after = store.stats().await.unwrap().symbol_count;
        assert_eq!(before, after);
        assert_eq!(total_matches(&store, "beta").await, 1);
    }

    #[tokio::test]
    async fn refresh_file_reindexes_edited_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // The edit flow: write new bytes, then refresh that one file. The
        // old rows are replaced — not just dropped — so a search finds the
        // new content immediately, with no stale banner.
        tokio::fs::write(&file, "fn beta() {}\n").await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 0);
        let page = store.search_symbols("beta", 50, None, None).await.unwrap();
        assert_eq!(page.total, 1);
        assert!(!page.stale());
    }

    #[tokio::test]
    async fn search_symbols_language_filter_scopes_to_one_language() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "pub fn shared() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("mod.py"), "def shared():\n    pass\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // Unfiltered, the name spans both languages.
        let all = store
            .search_symbols("shared", 50, None, None)
            .await
            .unwrap();
        assert_eq!(all.rows.len(), 2);

        // A language filter scopes the index to exactly that language — no
        // cross-language leakage.
        let rust = store
            .search_symbols("shared", 50, None, Some(Language::Rust))
            .await
            .unwrap();
        assert_eq!(rust.rows.len(), 1);
        assert!(rust.rows[0].file.ends_with("lib.rs"));

        let py = store
            .search_symbols("shared", 50, None, Some(Language::Python))
            .await
            .unwrap();
        assert_eq!(py.rows.len(), 1);
        assert!(py.rows[0].file.ends_with("mod.py"));
    }

    #[tokio::test]
    async fn refresh_file_drops_rows_for_a_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        tokio::fs::remove_file(&file).await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 0);
        assert_eq!(store.stats().await.unwrap().file_count, 0);
    }

    /// `refresh_file` honors the same domain as a build pass: a file kind
    /// the indexer never covers, or a path under an ignored component,
    /// must not gain rows just because it was edited.
    #[tokio::test]
    async fn refresh_file_skips_files_a_build_would_not_cover() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        let baseline = store.stats().await.unwrap().file_count;

        let notes = root.join("notes.txt");
        tokio::fs::write(&notes, "fn fake() {}\n").await.unwrap();
        store.refresh_files(&[notes]).await.unwrap();

        let generated = root.join("target").join("gen.rs");
        tokio::fs::create_dir_all(generated.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&generated, "fn generated() {}\n")
            .await
            .unwrap();
        store.refresh_files(&[generated]).await.unwrap();

        assert_eq!(store.stats().await.unwrap().file_count, baseline);
        assert_eq!(total_matches(&store, "generated").await, 0);
    }

    /// A narrowed build's scope is durable: after `--lang rust`, editing
    /// a `.py` file must add nothing — the build excluded that language,
    /// and a refresh honoring the full extension table would contradict
    /// the recorded build domain.
    #[tokio::test]
    async fn refresh_honors_the_builds_language_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        let script = root.join("tool.py");
        tokio::fs::write(&script, "def beta(): pass\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store
            .index(IndexOptions {
                force: false,
                languages: Some(vec![Language::Rust]),
            })
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "alpha").await, 1);
        assert_eq!(total_matches(&store, "beta").await, 0);

        // Edit the .py file: the rust-scoped index must not gain rows.
        tokio::fs::write(&script, "def beta_v2(): pass\n")
            .await
            .unwrap();
        store
            .refresh_files(std::slice::from_ref(&script))
            .await
            .unwrap();
        assert_eq!(total_matches(&store, "beta_v2").await, 0);

        // An in-scope edit still refreshes.
        tokio::fs::write(root.join("lib.rs"), "fn gamma() {}\n")
            .await
            .unwrap();
        store.refresh_files(&[root.join("lib.rs")]).await.unwrap();
        assert_eq!(total_matches(&store, "gamma").await, 1);
    }

    /// An unrestricted build records the unrestricted scope, so an edit
    /// to any indexed-extension file refreshes — including one created
    /// after the build.
    #[tokio::test]
    async fn full_build_scope_covers_every_indexed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        let script = root.join("tool.py");
        tokio::fs::write(&script, "def beta(): pass\n")
            .await
            .unwrap();
        store.refresh_files(&[script]).await.unwrap();
        assert_eq!(total_matches(&store, "beta").await, 1);
    }

    /// A store that was merely OPENED (any read does this) has no build
    /// marker: a refresh must leave it empty — never grow a partial index
    /// that would then answer authoritatively. `index clear` returns the
    /// store to the same never-built state.
    #[tokio::test]
    async fn refresh_is_inert_without_a_completed_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(store.stats().await.unwrap().symbol_count, 0);

        // Build, then clear: the marker must clear with the rows.
        store.index(IndexOptions::default()).await.unwrap();
        store.clear().await.unwrap();
        store
            .refresh_files(std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(store.stats().await.unwrap().symbol_count, 0);
    }

    /// The build scope round-trips through its meta representation.
    #[test]
    fn build_scope_meta_value_round_trips() {
        let all = BuildScope::All;
        assert_eq!(BuildScope::parse(&all.meta_value()), all);

        let narrowed = BuildScope::from_options(&Some(vec![Language::Python, Language::Rust]));
        let parsed = BuildScope::parse(&narrowed.meta_value());
        assert_eq!(parsed, narrowed);
        let exts = parsed.extensions();
        assert!(exts.contains(&"rs") && exts.contains(&"py"));
        assert!(!exts.contains(&"go"));
    }

    #[tokio::test]
    async fn search_reports_stale_after_external_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert!(
            !store
                .search_symbols("alpha", 50, None, None)
                .await
                .unwrap()
                .stale()
        );

        // An edit the store never saw (external tool, git checkout): the old
        // rows still match the query but must carry the stale marker…
        tokio::fs::write(&file, "fn alpha() { changed() }\n")
            .await
            .unwrap();
        let page = store.search_symbols("alpha", 50, None, None).await.unwrap();
        assert_eq!(page.total, 1);
        assert!(page.stale());

        // …until the next index pass clears it.
        store.index(IndexOptions::default()).await.unwrap();
        assert!(
            !store
                .search_symbols("alpha", 50, None, None)
                .await
                .unwrap()
                .stale()
        );
    }

    #[tokio::test]
    async fn search_reports_stale_when_matched_file_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");

        tokio::fs::write(&file, "fn alpha() {}\n").await.unwrap();
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        tokio::fs::remove_file(&file).await.unwrap();
        assert!(
            store
                .search_symbols("alpha", 50, None, None)
                .await
                .unwrap()
                .stale()
        );
    }

    #[tokio::test]
    async fn stale_only_considers_files_matched_by_the_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("a.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("b.rs"), "fn beta() {}\n")
            .await
            .unwrap();

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();

        // Rewriting an unrelated file must not flag pages it doesn't back —
        // and a byte-identical rewrite isn't stale at all (content hash, not
        // mtime, is the currency key).
        tokio::fs::write(root.join("b.rs"), "fn beta_v2() {}\n")
            .await
            .unwrap();
        assert!(
            !store
                .search_symbols("alpha", 50, None, None)
                .await
                .unwrap()
                .stale()
        );
        tokio::fs::write(root.join("a.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        assert!(
            !store
                .search_symbols("alpha", 50, None, None)
                .await
                .unwrap()
                .stale()
        );
    }

    /// A build rewrites the rows it deleted, and the pages in between belong
    /// back with the filesystem. Kept, they make the index's size a record of
    /// how many times it was rebuilt rather than of what it holds, and the
    /// disk a rebuild borrowed never comes back — on a tree under ordinary
    /// churn the freed pages outgrow the live ones.
    #[tokio::test]
    async fn a_build_hands_back_the_pages_it_freed() {
        async fn free_pages(store: &Store) -> i64 {
            store
                .db
                .call(|conn| conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)))
                .await
                .unwrap()
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for module in 0..24 {
            let mut source = String::new();
            for item in 0..40 {
                source.push_str(&format!(
                    "pub fn sample_{module}_{item}(alpha: i32) -> i32 {{ alpha + {item} }}\n"
                ));
            }
            tokio::fs::write(root.join(format!("mod_{module}.rs")), source)
                .await
                .unwrap();
        }

        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        assert_eq!(
            store
                .db
                .call(|conn| conn.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0)))
                .await
                .unwrap(),
            2,
            "the store is created in incremental auto-vacuum mode, which is the only \
             moment SQLite lets that be chosen"
        );

        store.index(IndexOptions::default()).await.unwrap();
        store
            .index(IndexOptions {
                force: true,
                languages: None,
            })
            .await
            .unwrap();
        assert_eq!(
            free_pages(&store).await,
            0,
            "a build leaves no page allocated to nothing"
        );

        // A zero would say nothing about a tree too small to free a page in the
        // first place, so the same release the rebuild performs is made here by
        // hand: it has to leave pages behind for the rebuild to have reclaimed.
        store
            .db
            .call(|conn| {
                conn.execute_batch(
                    "DELETE FROM content_lines; DELETE FROM symbols; DELETE FROM files;",
                )
            })
            .await
            .unwrap();
        assert!(
            free_pages(&store).await > 0,
            "the fixture has to be one whose rebuild frees pages"
        );
    }

    /// Clearing an index is the one operation whose whole result is the space
    /// it gives back. The text index is derived data whose delete trigger
    /// records a removal per row, so emptying the table it reads leaves a
    /// tombstone for every line — and an index that holds nothing keeps most
    /// of the disk of one that held everything.
    #[tokio::test]
    async fn a_cleared_index_holds_no_more_disk_than_one_never_built() {
        async fn pages(store: &Store) -> i64 {
            store
                .db
                .call(|conn| conn.query_row("PRAGMA page_count", [], |r| r.get(0)))
                .await
                .unwrap()
        }

        let untouched = tempfile::tempdir().unwrap();
        let fresh = Store::open(untouched.path(), StoreConfig::default())
            .await
            .unwrap();
        let never_built = pages(&fresh).await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for module in 0..24 {
            let mut source = String::new();
            for item in 0..40 {
                source.push_str(&format!(
                    "pub fn sample_{module}_{item}(alpha: i32) -> i32 {{ alpha + {item} }}\n"
                ));
            }
            tokio::fs::write(root.join(format!("mod_{module}.rs")), source)
                .await
                .unwrap();
        }
        let store = Store::open(root, StoreConfig::default()).await.unwrap();
        store.index(IndexOptions::default()).await.unwrap();
        assert!(
            pages(&store).await > never_built,
            "the fixture has to be one whose index outgrows an empty store"
        );

        store.clear().await.unwrap();
        assert!(
            pages(&store).await <= never_built,
            "a cleared index keeps no more pages than a store that never held anything"
        );

        // And what it gave back it can take again: the text index is derived,
        // so discarding it wholesale must leave a rebuild finding everything.
        store.index(IndexOptions::default()).await.unwrap();
        assert_eq!(
            store
                .search_content("alpha + 7", 50, &[Language::Rust])
                .await
                .unwrap()
                .total,
            24,
            "a rebuild after a clear searches the text it re-indexed"
        );
    }
}
