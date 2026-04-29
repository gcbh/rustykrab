use std::sync::Arc;

use rusqlite::params;
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

/// Maps Slack `(team_id, channel_id, thread_ts)` triples to conversation UUIDs.
///
/// The `thread_ts` column stores an empty string `""` to mean "no thread"
/// (a top-level message in a channel, or a DM). SQLite's `UNIQUE` treats
/// two `NULL`s as distinct, so we use `""` as the sentinel rather than
/// `NULL` to ensure each `(team, channel)` pair has at most one
/// "no-thread" conversation.
#[derive(Clone)]
pub struct SlackChatMapStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SlackChatMapStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Look up the conversation UUID for a `(team_id, channel_id, thread_ts)`
    /// triple. `thread_ts` is the Slack thread timestamp, or `""` for a
    /// top-level / DM conversation. Returns `None` if no mapping exists.
    pub fn lookup(
        &self,
        team_id: &str,
        channel_id: &str,
        thread_ts: &str,
    ) -> Result<Option<Uuid>, Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT conv_id FROM slack_chat_map
                 WHERE team_id = ?1 AND channel_id = ?2 AND thread_ts = ?3",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;

        match stmt.query_row(params![team_id, channel_id, thread_ts], |row| {
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

    /// Insert or update the mapping for a `(team_id, channel_id, thread_ts)` triple.
    pub fn upsert(
        &self,
        team_id: &str,
        channel_id: &str,
        thread_ts: &str,
        conv_id: Uuid,
    ) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO slack_chat_map (team_id, channel_id, thread_ts, conv_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(team_id, channel_id, thread_ts) DO UPDATE SET conv_id = excluded.conv_id",
            params![team_id, channel_id, thread_ts, conv_id.to_string()],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Remove the mapping for a `(team_id, channel_id, thread_ts)` triple
    /// (e.g. on `/reset`).
    pub fn remove(&self, team_id: &str, channel_id: &str, thread_ts: &str) -> Result<(), Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM slack_chat_map
             WHERE team_id = ?1 AND channel_id = ?2 AND thread_ts = ?3",
            params![team_id, channel_id, thread_ts],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_store() -> SlackChatMapStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE slack_chat_map (
                 team_id    TEXT NOT NULL,
                 channel_id TEXT NOT NULL,
                 thread_ts  TEXT NOT NULL DEFAULT '',
                 conv_id    TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 UNIQUE(team_id, channel_id, thread_ts)
             );",
        )
        .unwrap();
        SlackChatMapStore::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn upsert_and_lookup_thread() {
        let store = in_memory_store();
        let id = Uuid::new_v4();
        store.upsert("T1", "C1", "1700000000.000100", id).unwrap();
        assert_eq!(
            store.lookup("T1", "C1", "1700000000.000100").unwrap(),
            Some(id)
        );
        // A different thread in the same channel is a different conversation.
        assert_eq!(store.lookup("T1", "C1", "other.0001").unwrap(), None);
    }

    #[test]
    fn empty_thread_ts_is_distinct_from_threads() {
        let store = in_memory_store();
        let dm = Uuid::new_v4();
        let thread = Uuid::new_v4();
        store.upsert("T1", "D1", "", dm).unwrap();
        store
            .upsert("T1", "D1", "1700000000.000100", thread)
            .unwrap();
        assert_eq!(store.lookup("T1", "D1", "").unwrap(), Some(dm));
        assert_eq!(
            store.lookup("T1", "D1", "1700000000.000100").unwrap(),
            Some(thread)
        );
    }

    #[test]
    fn upsert_overwrites_existing_mapping() {
        let store = in_memory_store();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        store.upsert("T1", "C1", "ts1", first).unwrap();
        store.upsert("T1", "C1", "ts1", second).unwrap();
        assert_eq!(store.lookup("T1", "C1", "ts1").unwrap(), Some(second));
    }

    #[test]
    fn remove_clears_mapping() {
        let store = in_memory_store();
        let id = Uuid::new_v4();
        store.upsert("T1", "C1", "ts1", id).unwrap();
        store.remove("T1", "C1", "ts1").unwrap();
        assert_eq!(store.lookup("T1", "C1", "ts1").unwrap(), None);
    }
}
