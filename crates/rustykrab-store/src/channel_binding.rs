//! Which conversation a channel address is bound to.
//!
//! Every channel needs the same relation — *an external addressing tuple
//! identifies a conversation* — and previously each one grew its own table
//! and its own store module to express it. Telegram had
//! `telegram_chat_map(chat_id, thread_id)`, Slack had
//! `slack_chat_map(team_id, channel_id, thread_ts)`, and Signal had neither,
//! so it could not restore a conversation across a restart at all.
//!
//! One table holds all of them. A new channel costs an [`ChannelAddress`]
//! variant, not a migration plus a store module.
//!
//! The addressing tuple is flattened into a single `external_key` by
//! [`ChannelAddress::external_key`] rather than by the caller. Deriving the
//! key instead of trusting each call site to spell it the same way is the
//! same discipline applied to credential names in `origin_key` — two call
//! sites that disagree about the spelling silently address different rows.

use std::sync::Arc;

use rusqlite::params;
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

use crate::with_conn;

/// Where a conversation lives on a channel.
///
/// Construct one of these rather than building a key string: the mapping
/// from tuple to key is defined once, here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelAddress {
    /// Telegram chat, optionally a forum topic. `thread_id` is `0` for
    /// non-forum chats and for the implicit "General" topic — the value
    /// Telegram itself reports, so no normalisation is needed.
    Telegram { chat_id: i64, thread_id: i64 },
    /// Slack channel, optionally a thread. `thread_ts` is the empty string
    /// for a top-level message or a DM.
    Slack {
        team_id: String,
        channel_id: String,
        thread_ts: String,
    },
    /// Signal conversation, keyed by the peer's number or group id.
    Signal { peer: String },
}

impl ChannelAddress {
    /// Channel name, stored in its own column so a channel's bindings can be
    /// listed or dropped without parsing keys.
    pub fn channel(&self) -> &'static str {
        match self {
            Self::Telegram { .. } => "telegram",
            Self::Slack { .. } => "slack",
            Self::Signal { .. } => "signal",
        }
    }

    /// The addressing tuple flattened to one key.
    ///
    /// `:` separates components. Telegram's components are integers and
    /// Slack's ids are alphanumeric, so neither can contain the separator.
    /// A Slack `thread_ts` is a numeric timestamp (`1712345678.000100`), so
    /// it cannot either; the empty string stands for "no thread" and, unlike
    /// `NULL`, compares equal to itself in a primary key.
    pub fn external_key(&self) -> String {
        match self {
            Self::Telegram { chat_id, thread_id } => format!("{chat_id}:{thread_id}"),
            Self::Slack {
                team_id,
                channel_id,
                thread_ts,
            } => format!("{team_id}:{channel_id}:{thread_ts}"),
            Self::Signal { peer } => peer.clone(),
        }
    }
}

/// Maps channel addresses to conversation UUIDs.
///
/// The `conv_id` column carries `REFERENCES conversations(id) ON DELETE
/// CASCADE`, so deleting a conversation drops its bindings with it. Before
/// that constraint existed, deleting a conversation through the API left the
/// binding behind, and every later message on that channel resolved an id
/// that no longer loaded — the channel answered "internal error" forever.
#[derive(Clone)]
pub struct ChannelBindingStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl ChannelBindingStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// The conversation bound to `address`, or `None` if it has none.
    pub async fn lookup(&self, address: &ChannelAddress) -> Result<Option<Uuid>, Error> {
        let channel = address.channel();
        let key = address.external_key();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT conv_id FROM channel_bindings
                     WHERE channel = ?1 AND external_key = ?2",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            match stmt.query_row(params![channel, key], |row| row.get::<_, String>(0)) {
                Ok(id_str) => Uuid::parse_str(&id_str)
                    .map(Some)
                    .map_err(|e| Error::Storage(format!("invalid conv_id UUID: {e}"))),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(Error::Storage(e.to_string())),
            }
        })
        .await
    }

    /// Bind `address` to `conv_id`, replacing any existing binding.
    pub async fn bind(&self, address: &ChannelAddress, conv_id: Uuid) -> Result<(), Error> {
        let channel = address.channel();
        let key = address.external_key();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO channel_bindings (channel, external_key, conv_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(channel, external_key) DO UPDATE SET conv_id = excluded.conv_id",
                params![channel, key, conv_id.to_string()],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Drop the binding for `address` (e.g. on `/reset`).
    pub async fn unbind(&self, address: &ChannelAddress) -> Result<(), Error> {
        let channel = address.channel();
        let key = address.external_key();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "DELETE FROM channel_bindings WHERE channel = ?1 AND external_key = ?2",
                params![channel, key],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }
}

/// Fold the per-channel map tables into `channel_bindings` and drop them.
///
/// Idempotent: the tables are gone after the first run, and the guard skips
/// the work when they are.
///
/// Bindings whose conversation no longer exists are **not** carried over.
/// They cannot be — the new column has a foreign key — and they are exactly
/// the rows that were causing the channel to fail on every message, so
/// dropping them is the repair rather than a loss.
pub(crate) fn migrate_legacy_chat_maps(conn: &rusqlite::Connection) -> Result<(), Error> {
    let legacy: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name IN ('telegram_chat_map', 'slack_chat_map')",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?
    };
    if legacy.is_empty() {
        return Ok(());
    }

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Storage(e.to_string()))?;

    if legacy.iter().any(|t| t == "telegram_chat_map") {
        tx.execute_batch(
            "INSERT OR REPLACE INTO channel_bindings (channel, external_key, conv_id, created_at)
             SELECT 'telegram', chat_id || ':' || thread_id, conv_id,
                    COALESCE(created_at, datetime('now'))
             FROM telegram_chat_map
             WHERE conv_id IN (SELECT id FROM conversations);
             DROP TABLE telegram_chat_map;",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
    }

    if legacy.iter().any(|t| t == "slack_chat_map") {
        tx.execute_batch(
            "INSERT OR REPLACE INTO channel_bindings (channel, external_key, conv_id, created_at)
             SELECT 'slack', team_id || ':' || channel_id || ':' || thread_ts, conv_id,
                    COALESCE(created_at, datetime('now'))
             FROM slack_chat_map
             WHERE conv_id IN (SELECT id FROM conversations);
             DROP TABLE slack_chat_map;",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
    }

    tx.commit().map_err(|e| Error::Storage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store on a real migrated schema with foreign keys enforced, as
    /// `Store::open` configures them. A fixture that skipped the pragma
    /// would pass while the cascade this table exists for did nothing.
    fn store() -> (ChannelBindingStore, Arc<Mutex<rusqlite::Connection>>) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        (ChannelBindingStore::new(Arc::clone(&conn)), conn)
    }

    fn insert_conversation(conn: &Arc<Mutex<rusqlite::Connection>>, id: Uuid) {
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO conversations (id, data, created_at, updated_at)
                 VALUES (?1, '{}', '2026-01-01T00:00:00.000000000Z',
                         '2026-01-01T00:00:00.000000000Z')",
                params![id.to_string()],
            )
            .unwrap();
    }

    fn binding_count(conn: &Arc<Mutex<rusqlite::Connection>>) -> i64 {
        conn.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM channel_bindings", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn addresses_do_not_collide_across_channels() {
        // Same key text, different channel: distinct rows, because `channel`
        // is part of the primary key.
        let tg = ChannelAddress::Telegram {
            chat_id: 1,
            thread_id: 2,
        };
        let sig = ChannelAddress::Signal { peer: "1:2".into() };
        assert_eq!(tg.external_key(), sig.external_key());
        assert_ne!(tg.channel(), sig.channel());
    }

    #[test]
    fn slack_no_thread_is_distinct_from_a_thread() {
        let top = ChannelAddress::Slack {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            thread_ts: String::new(),
        };
        let threaded = ChannelAddress::Slack {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            thread_ts: "1712345678.000100".into(),
        };
        assert_ne!(top.external_key(), threaded.external_key());
    }

    #[tokio::test]
    async fn bind_lookup_unbind_round_trip() {
        let (store, conn) = store();
        let conv = Uuid::new_v4();
        insert_conversation(&conn, conv);
        let addr = ChannelAddress::Telegram {
            chat_id: 42,
            thread_id: 7,
        };

        assert_eq!(store.lookup(&addr).await.unwrap(), None);
        store.bind(&addr, conv).await.unwrap();
        assert_eq!(store.lookup(&addr).await.unwrap(), Some(conv));
        store.unbind(&addr).await.unwrap();
        assert_eq!(store.lookup(&addr).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rebinding_replaces_rather_than_duplicates() {
        let (store, conn) = store();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        insert_conversation(&conn, first);
        insert_conversation(&conn, second);
        let addr = ChannelAddress::Slack {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            thread_ts: String::new(),
        };

        store.bind(&addr, first).await.unwrap();
        store.bind(&addr, second).await.unwrap();

        assert_eq!(store.lookup(&addr).await.unwrap(), Some(second));
        assert_eq!(binding_count(&conn), 1);
    }

    #[tokio::test]
    async fn deleting_a_conversation_drops_its_bindings() {
        // The bug this table exists to prevent: a binding that outlives its
        // conversation makes every later message on that channel resolve an
        // id that no longer loads.
        let (store, conn) = store();
        let conv = Uuid::new_v4();
        insert_conversation(&conn, conv);
        let addr = ChannelAddress::Telegram {
            chat_id: 42,
            thread_id: 0,
        };
        store.bind(&addr, conv).await.unwrap();

        crate::ConversationStore::new(Arc::clone(&conn))
            .delete(conv)
            .await
            .unwrap();

        assert_eq!(
            store.lookup(&addr).await.unwrap(),
            None,
            "the cascade must remove the binding with the conversation"
        );
    }

    #[tokio::test]
    async fn binding_a_missing_conversation_is_refused() {
        let (store, _conn) = store();
        let addr = ChannelAddress::Signal {
            peer: "+15550100".into(),
        };
        assert!(
            store.bind(&addr, Uuid::new_v4()).await.is_err(),
            "the foreign key must refuse a binding to a conversation that does not exist"
        );
    }

    #[test]
    fn migration_folds_both_legacy_tables_and_drops_stale_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        // A database written by a build that predates `channel_bindings`.
        let live = Uuid::new_v4();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, data TEXT NOT NULL,
                 title TEXT, created_at TEXT, updated_at TEXT
             );
             CREATE TABLE telegram_chat_map (
                 chat_id INTEGER NOT NULL, thread_id INTEGER NOT NULL DEFAULT 0,
                 conv_id TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 UNIQUE(chat_id, thread_id)
             );
             CREATE TABLE slack_chat_map (
                 team_id TEXT NOT NULL, channel_id TEXT NOT NULL,
                 thread_ts TEXT NOT NULL DEFAULT '', conv_id TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 UNIQUE(team_id, channel_id, thread_ts)
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations (id, data) VALUES (?1, '{}')",
            params![live.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO telegram_chat_map (chat_id, thread_id, conv_id) VALUES (9, 3, ?1)",
            params![live.to_string()],
        )
        .unwrap();
        // A binding left behind by a conversation deleted through the API —
        // exactly the row that used to break the channel.
        conn.execute(
            "INSERT INTO telegram_chat_map (chat_id, thread_id, conv_id) VALUES (10, 0, ?1)",
            params![Uuid::new_v4().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO slack_chat_map (team_id, channel_id, thread_ts, conv_id)
             VALUES ('T1', 'C1', '', ?1)",
            params![live.to_string()],
        )
        .unwrap();

        crate::Store::run_migrations(&conn).unwrap();

        let rows: Vec<(String, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT channel, external_key, conv_id FROM channel_bindings ORDER BY 1, 2",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            rows,
            vec![
                ("slack".to_string(), "T1:C1:".to_string(), live.to_string()),
                ("telegram".to_string(), "9:3".to_string(), live.to_string()),
            ],
            "live bindings carry over; the one naming a deleted conversation does not"
        );

        let legacy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('telegram_chat_map', 'slack_chat_map')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 0, "the legacy tables must be gone");

        // Idempotent: a second pass is a no-op, not an error.
        crate::Store::run_migrations(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM channel_bindings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn migration_preserves_the_key_the_lookup_will_use() {
        // The migration writes keys with SQL string concatenation while
        // lookups build them in Rust. If the two ever disagree, every
        // migrated conversation silently starts over as a new one.
        assert_eq!(
            ChannelAddress::Telegram {
                chat_id: 9,
                thread_id: 3
            }
            .external_key(),
            "9:3"
        );
        assert_eq!(
            ChannelAddress::Slack {
                team_id: "T1".into(),
                channel_id: "C1".into(),
                thread_ts: String::new(),
            }
            .external_key(),
            "T1:C1:"
        );
    }
}
