use std::path::Path;

use tokio_rusqlite::Connection;

pub use tokio_rusqlite::rusqlite;

use crate::error::StoreError;

pub struct SqliteDb {
    conn: Connection,
}

impl SqliteDb {
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(Self { conn })
    }

    pub async fn call<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        self.conn
            .call(f)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))
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
