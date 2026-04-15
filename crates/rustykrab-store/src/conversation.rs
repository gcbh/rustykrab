use std::sync::Arc;

use chrono::Utc;
use rusqlite::params;
use rustykrab_core::types::Conversation;
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

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
        let conv = Conversation {
            id: Uuid::new_v4(),
            messages: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
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
