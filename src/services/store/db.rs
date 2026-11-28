//! Async adapter over a dedicated SQLite owner thread.
//!
//! A SQLite connection wants exactly one owner. One OS thread owns the
//! `rusqlite::Connection` and executes closures sent over a channel —
//! serialization is the point: write ordering needs no locks and matches
//! SQLite's single-writer model. The store is product reliability, not a
//! cache, so the adapter guarantees:
//!
//! - a panicking closure never wedges the connection (caught, rolled
//!   back, surfaced as `StoreError`);
//! - dropping `SqliteDb` drains already-submitted work, closes the
//!   connection, and joins the thread, so the on-disk file is finalized
//!   before shutdown completes — unless the store was abandoned first, which
//!   rolls that work back instead and leaves the log uncheckpointed for the
//!   next connection (`SqliteDb::abandon`).

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use tokio::sync::oneshot;

use crate::error::StoreError;

/// How long a statement waits for another process's write to finish before
/// reporting contention. The store is shared by a daemon and direct runs,
/// so contention is ordinary and waiting is the answer.
pub(super) const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type Job = Box<dyn FnOnce(&mut rusqlite::Connection) + Send>;

pub struct SqliteDb {
    sender: Option<mpsc::Sender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
    interrupt: rusqlite::InterruptHandle,
    /// Set by [`SqliteDb::abandon`]; the worker runs no job once it is.
    abandoned: Arc<AtomicBool>,
}

impl SqliteDb {
    /// A connection that may write, creating the file if there is none.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with(path, rusqlite::OpenFlags::default()).await
    }

    /// A connection that only reads a file that already exists. Under WAL it
    /// reads the last committed state without waiting for a writer, whether
    /// that writer is another process or a connection of this one.
    pub async fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        Self::open_with(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .await
    }

    async fn open_with(path: &Path, flags: rusqlite::OpenFlags) -> Result<Self, StoreError> {
        let path = path.to_path_buf();
        let (ready_tx, ready_rx) =
            oneshot::channel::<Result<rusqlite::InterruptHandle, StoreError>>();
        let (sender, receiver) = mpsc::channel::<Job>();
        let abandoned = Arc::new(AtomicBool::new(false));
        let skip = Arc::clone(&abandoned);

        let worker = thread::Builder::new()
            .name("symora-sqlite".to_string())
            .spawn(move || {
                let opened = rusqlite::Connection::open_with_flags(&path, flags);
                let mut conn = match opened.and_then(|conn| {
                    // Before any statement can contend: another process
                    // writing is a reason to wait, and a connection without
                    // this would call that failure instead.
                    conn.busy_timeout(BUSY_TIMEOUT)?;
                    Ok(conn)
                }) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(conn.get_interrupt_handle()));
                        conn
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(StoreError::Database(e.to_string())));
                        return;
                    }
                };

                while let Ok(job) = receiver.recv() {
                    // Work an abandoned store was left with is dropped unrun.
                    if !skip.load(Ordering::Acquire) {
                        job(&mut conn);
                    }
                }

                // Channel closed: all submitted work is done. Close the
                // connection so the file (and its WAL) is finalized — except
                // on an abandoned store, which leaves the log as it is. The
                // checkpoint SQLite runs as the last connection closes copies
                // the log into the file, the work the store was abandoned to
                // spare; what the log holds is committed, and the next
                // connection reads it there.
                if skip.load(Ordering::Acquire) {
                    let _ = conn.set_db_config(
                        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                        true,
                    );
                }
                let _ = conn.close();
            })
            .map_err(|e| StoreError::Database(format!("Failed to spawn SQLite thread: {e}")))?;

        let interrupt = ready_rx
            .await
            .map_err(|_| StoreError::Database("SQLite thread exited during open".to_string()))??;

        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            interrupt,
            abandoned,
        })
    }

    /// Give up the work this connection holds rather than finish it: the job
    /// queued is dropped unrun, and the statement running is interrupted
    /// until its job stops, so its transaction rolls back. For a store closing
    /// with work nobody awaits any more; every call after it fails.
    pub fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
        self.interrupt.interrupt();
    }

    /// Run `f` once the connection has run every job given to it so far,
    /// without waiting for it, and log what it fails with. On a store
    /// abandoned first it is dropped unrun, so whatever it owns is released
    /// either way.
    pub fn submit<F>(&self, f: F)
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<(), rusqlite::Error> + Send + 'static,
    {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Box::new(move |conn| {
                if let Err(e) = guarded(conn, f) {
                    tracing::debug!("Queued store work failed: {e}");
                }
            }));
        }
    }

    pub async fn call<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, StoreError>>();

        let job: Job = Box::new(move |conn| {
            let _ = tx.send(guarded(conn, f));
        });

        self.sender
            .as_ref()
            .expect("sender lives until drop")
            .send(job)
            .map_err(|_| StoreError::Database("SQLite worker stopped".to_string()))?;

        rx.await
            .map_err(|_| StoreError::Database("SQLite worker dropped the request".to_string()))?
    }

    pub async fn execute(&self, sql: &str) -> Result<(), StoreError> {
        let sql = sql.to_string();
        self.call(move |conn| {
            conn.execute_batch(&sql)?;
            Ok(())
        })
        .await
    }
}

/// Run one job on the connection. A panicking closure is caught and its open
/// transaction rolled back, so the connection observes no torn state
/// (`AssertUnwindSafe`: the closure runs exactly once).
fn guarded<F, R>(conn: &mut rusqlite::Connection, f: F) -> Result<R, StoreError>
where
    F: FnOnce(&mut rusqlite::Connection) -> Result<R, rusqlite::Error>,
{
    match std::panic::catch_unwind(AssertUnwindSafe(|| f(conn))) {
        Ok(r) => r.map_err(classify),
        Err(panic) => {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            Err(StoreError::Database(format!(
                "SQLite closure panicked: {}",
                panic_message(&*panic)
            )))
        }
    }
}

/// SQLite says which failures mean the file is not a database and which
/// mean someone else is writing it. The first is the only one whose
/// recovery is to replace the file; the second is ordinary in a store a
/// daemon and direct runs share, and its remedy is to try again.
pub(super) fn classify(error: rusqlite::Error) -> StoreError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            StoreError::Corrupt(error.to_string())
        }
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            StoreError::Busy
        }
        _ => StoreError::Database(error.to_string()),
    }
}

impl Drop for SqliteDb {
    fn drop(&mut self) {
        // Closing the channel ends the worker loop after the already-queued
        // jobs run; joining makes the close deterministic. The wait is
        // bounded by work submitted before drop — nothing new can enqueue.
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            // An interrupt lands only on a statement already running, and a
            // job runs its statements one after another, so an abandoned
            // store's job is interrupted until it stops.
            while self.abandoned.load(Ordering::Acquire) && !worker.is_finished() {
                self.interrupt.interrupt();
                thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = worker.join();
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s
    } else {
        "unknown panic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn drop_finalizes_the_database_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");

        let db = SqliteDb::open(&path).await.unwrap();
        db.execute("PRAGMA journal_mode = WAL; CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .await
            .unwrap();
        db.call(|conn| {
            conn.execute("INSERT INTO t (id) VALUES (1)", [])?;
            Ok(())
        })
        .await
        .unwrap();
        drop(db);

        // Reopen: the file must be a valid database with the row intact.
        let db = SqliteDb::open(&path).await.unwrap();
        let count = db
            .call(|conn| conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0)))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    /// An abandoned store closes without finishing its work: the statement
    /// running is interrupted and its transaction rolls back, and the job
    /// queued behind it never runs. Closed without being abandoned, the store
    /// would run the statement to its end, which here takes far longer than
    /// the close is given.
    #[tokio::test]
    async fn an_abandoned_store_closes_without_finishing_its_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let db = SqliteDb::open(&path).await.unwrap();
        db.execute("PRAGMA journal_mode = WAL; CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .await
            .unwrap();

        // Each call is handed to the connection when first polled, and left
        // there when the caller stops waiting for it.
        let running = db.call(|conn| {
            let tx = conn.transaction()?;
            tx.execute("INSERT INTO t (id) VALUES (1)", [])?;
            tx.query_row(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
                 SELECT count(*) FROM n",
                [],
                |r| r.get::<_, i64>(0),
            )?;
            tx.commit()
        });
        let queued = db.call(|conn| conn.execute("INSERT INTO t (id) VALUES (2)", []).map(drop));
        let _ = tokio::time::timeout(Duration::from_millis(100), running).await;
        let _ = tokio::time::timeout(Duration::from_millis(1), queued).await;

        db.abandon();
        let (closed, close) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(db);
            let _ = closed.send(());
        });
        close
            .recv_timeout(Duration::from_secs(10))
            .expect("the abandoned store closed without finishing its work");

        let db = SqliteDb::open(&path).await.unwrap();
        let count = db
            .call(|conn| conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0)))
            .await
            .unwrap();
        assert_eq!(count, 0, "neither write may survive the abandonment");
    }

    #[tokio::test]
    async fn panicking_closure_surfaces_an_error_and_does_not_wedge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");

        let db = SqliteDb::open(&path).await.unwrap();
        let result: Result<(), _> = db.call(|_conn| panic!("boom")).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");

        // The connection must keep serving after the panic.
        let answer = db
            .call(|conn| conn.query_row("SELECT 42", [], |r| r.get::<_, i64>(0)))
            .await
            .unwrap();
        assert_eq!(answer, 42);
    }

    #[tokio::test]
    async fn panic_inside_transaction_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");

        let db = SqliteDb::open(&path).await.unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .await
            .unwrap();

        let _ = db
            .call(|conn| {
                conn.execute_batch("BEGIN")?;
                conn.execute("INSERT INTO t (id) VALUES (1)", [])?;
                panic!("mid-transaction");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;

        // The torn transaction was rolled back; the connection is in
        // autocommit and the partial write is gone.
        let count = db
            .call(|conn| conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0)))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
