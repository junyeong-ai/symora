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
//!   before shutdown completes.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use tokio::sync::oneshot;

use crate::error::StoreError;

/// How long a statement waits for another process's write to finish before
/// reporting contention. The store is shared by a daemon and direct runs,
/// so contention is ordinary and waiting is the answer.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type Job = Box<dyn FnOnce(&mut rusqlite::Connection) + Send>;

pub struct SqliteDb {
    sender: Option<mpsc::Sender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SqliteDb {
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let path = path.to_path_buf();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), StoreError>>();
        let (sender, receiver) = mpsc::channel::<Job>();

        let worker = thread::Builder::new()
            .name("symora-sqlite".to_string())
            .spawn(move || {
                let mut conn = match rusqlite::Connection::open(&path).and_then(|conn| {
                    // Before any statement can contend: another process
                    // writing is a reason to wait, and a connection without
                    // this would call that failure instead.
                    conn.busy_timeout(BUSY_TIMEOUT)?;
                    Ok(conn)
                }) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(StoreError::Database(e.to_string())));
                        return;
                    }
                };

                while let Ok(job) = receiver.recv() {
                    job(&mut conn);
                }

                // Channel closed: all submitted work is done. Close the
                // connection so the file (and its WAL) is finalized.
                let _ = conn.close();
            })
            .map_err(|e| StoreError::Database(format!("Failed to spawn SQLite thread: {e}")))?;

        ready_rx
            .await
            .map_err(|_| StoreError::Database("SQLite thread exited during open".to_string()))??;

        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    pub async fn call<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, StoreError>>();

        let job: Job = Box::new(move |conn| {
            // AssertUnwindSafe: the closure runs exactly once and on panic
            // any open transaction is rolled back below, so the connection
            // observes no torn state.
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| f(conn)));
            let result = match outcome {
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
            };
            let _ = tx.send(result);
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
