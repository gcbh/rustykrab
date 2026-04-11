mod conversation;
pub mod keychain;
mod secret;

use std::path::Path;
use std::sync::Arc;

use rustykrab_core::Error;
use std::sync::Mutex;
use zeroize::Zeroizing;

pub use conversation::ConversationStore;
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
            ",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
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

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<(), Error> {
        // WAL mode checkpoints automatically; explicit checkpoint for shutdown.
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
