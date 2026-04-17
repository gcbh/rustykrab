mod chat_map;
mod conversation;
mod jobs;
pub mod keychain;
pub mod registry;
mod secret;

use std::path::Path;
use std::sync::Arc;

use rustykrab_core::Error;
use std::sync::Mutex;
use zeroize::Zeroizing;

pub use chat_map::ChatMapStore;
pub use conversation::ConversationStore;
pub use jobs::{JobRun, JobStore, ScheduledJob};
pub use secret::SecretStore;

/// Top-level database handle wrapping a SQLite connection.
///
/// The master key is wrapped in `Zeroizing` so it is securely erased
/// from memory when the Store is dropped.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<rusqlite::Connection>>,
    master_key: Zeroizing<Vec<u8>>,
}

impl Store {
    /// Open (or create) a store at the given directory path.
    ///
    /// `master_key` is used to encrypt secrets at rest. It should be
    /// sourced from the OS keychain or an environment variable — never
    /// stored alongside the database.
    pub fn open(path: impl AsRef<Path>, master_key: Vec<u8>) -> Result<Self, Error> {
        let db_path = path.as_ref().join("store.db");
        let conn =
            rusqlite::Connection::open(&db_path).map_err(|e| Error::Storage(e.to_string()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        Self::run_migrations(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            master_key: Zeroizing::new(master_key),
        })
    }

    fn run_migrations(conn: &rusqlite::Connection) -> Result<(), Error> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS conversations (
                id   TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS secrets (
                name TEXT PRIMARY KEY,
                data BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS scheduled_jobs (
                id              TEXT PRIMARY KEY,
                schedule        TEXT NOT NULL,
                task            TEXT NOT NULL,
                channel         TEXT,
                chat_id         TEXT,
                one_shot        INTEGER NOT NULL DEFAULT 0,
                enabled         INTEGER NOT NULL DEFAULT 1,
                next_run_at     TEXT NOT NULL,
                last_run_at     TEXT,
                created_at      TEXT NOT NULL,
                conversation_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_due
                ON scheduled_jobs (next_run_at)
                WHERE enabled = 1;

            CREATE TABLE IF NOT EXISTS job_runs (
                id         TEXT PRIMARY KEY,
                job_id     TEXT NOT NULL,
                status     TEXT NOT NULL,
                output     TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_job_runs_job_id
                ON job_runs (job_id, finished_at DESC);

            CREATE TABLE IF NOT EXISTS telegram_chat_map (
                chat_id    INTEGER NOT NULL,
                thread_id  INTEGER NOT NULL DEFAULT 0,
                conv_id    TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(chat_id, thread_id)
            );
            ",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        // Additive migration for pre-existing databases. PRAGMA table_info
        // lists current columns; only ALTER if conversation_id is missing.
        let mut stmt = conn
            .prepare("PRAGMA table_info(scheduled_jobs)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;
        drop(stmt);
        if !existing.iter().any(|c| c == "conversation_id") {
            conn.execute(
                "ALTER TABLE scheduled_jobs ADD COLUMN conversation_id TEXT",
                [],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        }

        Ok(())
    }

    /// Return a handle for conversation operations.
    pub fn conversations(&self) -> ConversationStore {
        ConversationStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for encrypted secret operations.
    pub fn secrets(&self) -> SecretStore {
        SecretStore::new(Arc::clone(&self.conn), self.master_key.clone())
    }

    /// Return a handle for scheduled-job operations.
    pub fn jobs(&self) -> JobStore {
        JobStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for Telegram chat/thread → conversation mapping.
    pub fn chat_map(&self) -> ChatMapStore {
        ChatMapStore::new(Arc::clone(&self.conn))
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<(), Error> {
        // WAL mode checkpoints automatically; explicit checkpoint for shutdown.
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
