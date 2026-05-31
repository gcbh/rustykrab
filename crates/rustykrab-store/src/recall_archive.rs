use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use rustykrab_core::recall::RecallPersistence;
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

/// SQLite-backed durable store for compaction-displaced recall archives,
/// keyed by conversation id.
///
/// Implements [`RecallPersistence`] so it can be injected into a
/// `RecallStore` as its write-through backing layer, letting the recall
/// archive survive process restarts.
#[derive(Clone, Debug)]
pub struct RecallArchiveStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl RecallArchiveStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Insert or replace the archive text for a conversation. `created_at`
    /// is preserved on update; only `updated_at` advances.
    pub fn upsert(&self, conversation_id: Uuid, archive: &str) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO recall_archive (conversation_id, archive, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(conversation_id) DO UPDATE SET
                 archive = excluded.archive,
                 updated_at = excluded.updated_at",
            params![conversation_id.to_string(), archive, now],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Fetch the archive text for a conversation, or `None` if absent.
    pub fn get(&self, conversation_id: Uuid) -> Result<Option<String>, Error> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT archive FROM recall_archive WHERE conversation_id = ?1",
            params![conversation_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| Error::Storage(e.to_string()))
    }

    /// Delete the archive for a conversation. Idempotent — deleting a
    /// missing row is not an error.
    pub fn delete(&self, conversation_id: Uuid) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM recall_archive WHERE conversation_id = ?1",
            params![conversation_id.to_string()],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}

/// Best-effort adapter: persistence failures are logged and swallowed so
/// they never break the agent loop. The in-memory `RecallStore` cache
/// remains authoritative for the live session even if a write is dropped.
impl RecallPersistence for RecallArchiveStore {
    fn load(&self, conversation_id: Uuid) -> Option<String> {
        match self.get(conversation_id) {
            Ok(archive) => archive,
            Err(e) => {
                tracing::warn!(error = %e, %conversation_id, "failed to load recall archive");
                None
            }
        }
    }

    fn upsert(&self, conversation_id: Uuid, archive: &str) {
        if let Err(e) = RecallArchiveStore::upsert(self, conversation_id, archive) {
            tracing::warn!(error = %e, %conversation_id, "failed to persist recall archive");
        }
    }

    fn delete(&self, conversation_id: Uuid) {
        if let Err(e) = RecallArchiveStore::delete(self, conversation_id) {
            tracing::warn!(error = %e, %conversation_id, "failed to delete recall archive");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_store() -> RecallArchiveStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE recall_archive (
                conversation_id TEXT PRIMARY KEY,
                archive         TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );",
        )
        .unwrap();
        RecallArchiveStore::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let store = in_memory_store();
        let conv = Uuid::new_v4();
        store.upsert(conv, "hello").unwrap();
        assert_eq!(store.get(conv).unwrap().as_deref(), Some("hello"));
    }

    #[test]
    fn upsert_replaces_existing() {
        let store = in_memory_store();
        let conv = Uuid::new_v4();
        store.upsert(conv, "first").unwrap();
        store.upsert(conv, "second").unwrap();
        assert_eq!(store.get(conv).unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn get_missing_returns_none() {
        let store = in_memory_store();
        assert_eq!(store.get(Uuid::new_v4()).unwrap(), None);
    }

    #[test]
    fn delete_is_idempotent() {
        let store = in_memory_store();
        let conv = Uuid::new_v4();
        store.upsert(conv, "x").unwrap();
        store.delete(conv).unwrap();
        store.delete(conv).unwrap();
        assert_eq!(store.get(conv).unwrap(), None);
    }

    #[test]
    fn upsert_preserves_created_at_on_update() {
        let store = in_memory_store();
        let conv = Uuid::new_v4();
        store.upsert(conv, "first").unwrap();
        let created: String = {
            let conn = store.conn.lock().unwrap();
            conn.query_row(
                "SELECT created_at FROM recall_archive WHERE conversation_id = ?1",
                params![conv.to_string()],
                |row| row.get(0),
            )
            .unwrap()
        };
        store.upsert(conv, "second").unwrap();
        let created_after: String = {
            let conn = store.conn.lock().unwrap();
            conn.query_row(
                "SELECT created_at FROM recall_archive WHERE conversation_id = ?1",
                params![conv.to_string()],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(created, created_after);
    }
}
