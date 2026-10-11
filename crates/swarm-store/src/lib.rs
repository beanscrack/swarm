//! Durable persistence for Swarm runs (SQLx + SQLite).
//!
//! The store is the single source of truth for run state. All state transitions
//! are performed through explicit, validated methods; the database enforces
//! atomic task claiming via lease tokens so concurrent claimants (including
//! multiple processes on the same database) never double-execute a task.

mod agents;
mod artifacts;
mod events;
mod findings;
mod hypotheses;
mod recovery;
mod runs;
mod tasks;
mod usage;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

/// Default lease TTL (milliseconds) for a running task.
pub const DEFAULT_LEASE_TTL_MS: i64 = 120_000;

/// Durable store rooted at a directory (SQLite database + artifact files).
pub struct Store {
    pool: SqlitePool,
    root: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the store rooted at `root`.
    pub async fn open(root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        tokio::fs::create_dir_all(root.join("artifacts"))
            .await
            .context("create store directory")?;
        let db_path = root.join("swarm.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_millis(5_000));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(16)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect_with(opts)
            .await
            .with_context(|| format!("connect sqlite at {}", root.display()))?;
        let migrator = sqlx::migrate!();
        {
            let mut conn = pool
                .acquire()
                .await
                .context("acquire connection for migration")?;
            migrator
                .run(&mut *conn)
                .await
                .context("run migrations")?;
        }
        Ok(Self { pool, root })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The artifact storage directory (created by [`Store::open`]).
    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// Closes the pool and releases resources.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Generic namespaced key-value storage (for cursors and flags).
    pub async fn kv_set(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO key_values (namespace, key, value) VALUES (?, ?, ?) \
             ON CONFLICT(namespace, key) DO UPDATE SET value = excluded.value",
        )
        .bind(namespace)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn kv_get(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM key_values WHERE namespace = ? AND key = ?")
            .bind(namespace)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.try_get::<String, _>(0).ok()))
    }
}

#[cfg(test)]
pub mod tests_util {
    use super::*;
    use tempfile::TempDir;

    /// Opens a fresh store in a temp dir for tests.
    pub async fn open_test_store() -> (Store, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).await.unwrap();
        (store, tmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn opens_and_migrates() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).await.unwrap();
        assert!(store.pool.acquire().await.is_ok());
        store.close().await;
    }
}
