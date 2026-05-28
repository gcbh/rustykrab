use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::params;
use rustykrab_core::types::Conversation;
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

/// Lightweight summary of a conversation used by listing endpoints.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// CRUD operations on conversations backed by SQLite.
#[derive(Clone)]
pub struct ConversationStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl ConversationStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Create a new empty conversation and return it.
    pub fn create(&self) -> Result<Conversation, Error> {
        self.create_with_title(None)
    }

    /// Create a new empty conversation with an optional title and return it.
    pub fn create_with_title(&self, title: Option<String>) -> Result<Conversation, Error> {
        let conv = Conversation {
            id: Uuid::new_v4(),
            messages: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            title,
            summary: None,
            detected_profile: None,
            channel_source: None,
            channel_id: None,
            channel_thread_id: None,
        };
        self.save(&conv)?;
        Ok(conv)
    }

    /// Persist a conversation (insert or update).
    pub fn save(&self, conv: &Conversation) -> Result<(), Error> {
        let data = serde_json::to_string(conv)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations (id, data) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![conv.id.to_string(), data],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a conversation by ID.
    pub fn get(&self, id: Uuid) -> Result<Conversation, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT data FROM conversations WHERE id = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let data: String = stmt
            .query_row(params![id.to_string()], |row| row.get(0))
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("conversation {id}"))
                }
                other => Error::Storage(other.to_string()),
            })?;
        let conv: Conversation = serde_json::from_str(&data)?;
        Ok(conv)
    }

    /// List all conversation IDs (lightweight, doesn't deserialize messages).
    pub fn list_ids(&self) -> Result<Vec<Uuid>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM conversations")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let id_str: String = row.get(0)?;
                Ok(id_str)
            })
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut ids = Vec::new();
        for row in rows {
            let id_str = row.map_err(|e| Error::Storage(e.to_string()))?;
            let id = Uuid::parse_str(&id_str).map_err(|e| Error::Storage(e.to_string()))?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// List all conversation summaries (id, title, timestamps) ordered by
    /// `updated_at` descending. Skips entries whose stored JSON cannot be
    /// parsed instead of failing the whole list.
    pub fn list_summaries(&self) -> Result<Vec<ConversationSummary>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT data FROM conversations")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let data = row.map_err(|e| Error::Storage(e.to_string()))?;
            if let Ok(conv) = serde_json::from_str::<Conversation>(&data) {
                out.push(ConversationSummary {
                    id: conv.id,
                    title: conv.title,
                    created_at: conv.created_at,
                    updated_at: conv.updated_at,
                });
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(out)
    }

    /// Delete a conversation by ID. Returns `NotFound` if the conversation
    /// does not exist, so callers can distinguish 404 from 500.
    pub fn delete(&self, id: Uuid) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        let affected = conn
            .execute(
                "DELETE FROM conversations WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        if affected == 0 {
            return Err(Error::NotFound(format!("conversation {id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a `ConversationStore` backed by an in-memory SQLite
    /// connection with just the schema this module needs. Mirrors the
    /// pattern used in `jobs.rs` so the store tests don't need a
    /// tempdir crate.
    fn in_memory_store() -> ConversationStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE conversations (id TEXT PRIMARY KEY, data TEXT NOT NULL);")
            .unwrap();
        ConversationStore::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn create_with_title_persists_title_and_round_trips() {
        let store = in_memory_store();
        let conv = store
            .create_with_title(Some("hello".into()))
            .expect("create");
        assert_eq!(conv.title.as_deref(), Some("hello"));
        let reloaded = store.get(conv.id).expect("get");
        assert_eq!(reloaded.title.as_deref(), Some("hello"));
    }

    #[test]
    fn create_defaults_title_to_none() {
        let store = in_memory_store();
        let conv = store.create().expect("create");
        assert!(conv.title.is_none());
    }

    #[test]
    fn list_summaries_returns_entries_sorted_desc_by_updated_at() {
        let store = in_memory_store();
        let mut a = store.create_with_title(Some("a".into())).unwrap();
        let mut b = store.create_with_title(Some("b".into())).unwrap();
        // Force `b` to be older than `a`.
        b.updated_at = a.updated_at - chrono::Duration::seconds(60);
        store.save(&b).unwrap();
        a.updated_at = Utc::now();
        store.save(&a).unwrap();

        let summaries = store.list_summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[0].id, a.id,
            "newer conversation should come first"
        );
        assert_eq!(summaries[0].title.as_deref(), Some("a"));
        assert_eq!(summaries[1].id, b.id);
    }

    #[test]
    fn get_returns_not_found_for_unknown_id() {
        let store = in_memory_store();
        let err = store.get(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }
}
