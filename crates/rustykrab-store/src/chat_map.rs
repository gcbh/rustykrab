use std::sync::Arc;

use rusqlite::params;
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

/// Maps Telegram `(chat_id, thread_id)` pairs to conversation UUIDs.
///
/// The `thread_id` column uses `0` for non-forum chats (or the implicit
/// "General" topic). The `UNIQUE(chat_id, thread_id)` constraint doubles
/// as a composite index, so lookups by both columns are fast.
#[derive(Clone)]
pub struct ChatMapStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl ChatMapStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Look up the conversation UUID for a `(chat_id, thread_id)` pair.
    /// Returns `None` if no mapping exists yet.
    pub fn lookup(&self, chat_id: i64, thread_id: i64) -> Result<Option<Uuid>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT conv_id FROM telegram_chat_map
                 WHERE chat_id = ?1 AND thread_id = ?2",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        match stmt.query_row(params![chat_id, thread_id], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        }) {
            Ok(id_str) => {
                let id = Uuid::parse_str(&id_str)
                    .map_err(|e| Error::Storage(format!("invalid conv_id UUID: {e}")))?;
                Ok(Some(id))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }

    /// Insert or update the mapping for a `(chat_id, thread_id)` pair.
    pub fn upsert(&self, chat_id: i64, thread_id: i64, conv_id: Uuid) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO telegram_chat_map (chat_id, thread_id, conv_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id, thread_id) DO UPDATE SET conv_id = excluded.conv_id",
            params![chat_id, thread_id, conv_id.to_string()],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Remove the mapping for a `(chat_id, thread_id)` pair (e.g. on `/reset`).
    pub fn remove(&self, chat_id: i64, thread_id: i64) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM telegram_chat_map WHERE chat_id = ?1 AND thread_id = ?2",
            params![chat_id, thread_id],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
