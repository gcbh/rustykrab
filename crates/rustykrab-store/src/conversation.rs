use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::params;
use rustykrab_core::types::{Conversation, Message};
use rustykrab_core::Error;
use std::sync::Mutex;
use uuid::Uuid;

use crate::with_conn;

/// Lightweight summary of a conversation used by listing endpoints.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fixed-width UTC RFC3339 so `ORDER BY updated_at` on the TEXT column
/// is chronological (variable-precision timestamps don't sort
/// lexicographically).
fn fmt_ts(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Clone of the conversation with the messages stripped — this is what
/// the `conversations.data` column stores. Messages live in the
/// `messages` table, one row each. Struct literal (not `..clone()`) so
/// adding a field to `Conversation` forces a decision here.
fn meta_only(conv: &Conversation) -> Conversation {
    Conversation {
        id: conv.id,
        messages: Vec::new(),
        created_at: conv.created_at,
        updated_at: conv.updated_at,
        title: conv.title.clone(),
        summary: conv.summary.clone(),
        detected_profile: conv.detected_profile.clone(),
        channel_source: conv.channel_source.clone(),
        channel_id: conv.channel_id.clone(),
        channel_thread_id: conv.channel_thread_id.clone(),
    }
}

/// Upsert the metadata row: slimmed JSON blob plus the promoted
/// `title`/`created_at`/`updated_at` columns that `list_summaries`
/// reads without any JSON parsing.
fn upsert_meta_row(conn: &rusqlite::Connection, conv: &Conversation) -> Result<(), Error> {
    let meta = serde_json::to_string(&meta_only(conv))?;
    conn.execute(
        "INSERT INTO conversations (id, data, title, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             data = excluded.data,
             title = excluded.title,
             created_at = excluded.created_at,
             updated_at = excluded.updated_at",
        params![
            conv.id.to_string(),
            meta,
            conv.title,
            fmt_ts(&conv.created_at),
            fmt_ts(&conv.updated_at)
        ],
    )
    .map_err(|e| Error::Storage(e.to_string()))?;
    Ok(())
}

fn insert_message_row(
    conn: &rusqlite::Connection,
    conversation_id: &str,
    idx: usize,
    msg: &Message,
) -> Result<(), Error> {
    let data = serde_json::to_string(msg)?;
    conn.execute(
        "INSERT OR REPLACE INTO messages (conversation_id, idx, data) VALUES (?1, ?2, ?3)",
        params![conversation_id, idx as i64, data],
    )
    .map_err(|e| Error::Storage(e.to_string()))?;
    Ok(())
}

/// Full rewrite of one conversation: metadata row plus every message
/// row, atomically. Must run outside any open transaction.
fn write_full(conn: &rusqlite::Connection, conv: &Conversation) -> Result<(), Error> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Storage(e.to_string()))?;
    upsert_meta_row(&tx, conv)?;
    let id = conv.id.to_string();
    tx.execute(
        "DELETE FROM messages WHERE conversation_id = ?1",
        params![id],
    )
    .map_err(|e| Error::Storage(e.to_string()))?;
    for (idx, msg) in conv.messages.iter().enumerate() {
        insert_message_row(&tx, &id, idx, msg)?;
    }
    tx.commit().map_err(|e| Error::Storage(e.to_string()))
}

/// One-time migration of legacy single-blob rows into the normalized
/// schema (metadata row + one `messages` row per message).
///
/// Legacy rows are exactly those with `updated_at IS NULL`: the column
/// was just added by `run_migrations` for old databases, and every
/// write path in this module sets it. Runs in ONE transaction for the
/// whole sweep — an interrupt rolls back atomically, leaving every row
/// in its legacy blob form (no data loss, no half-migrated
/// conversations), and the NULL predicate makes a completed sweep a
/// no-op, so re-running on every startup is safe and cheap.
///
/// Rows whose JSON cannot be parsed are left untouched (still legacy),
/// mirroring the old `list_summaries` skip-on-parse-failure behavior.
pub(crate) fn migrate_legacy_blobs(conn: &rusqlite::Connection) -> Result<(), Error> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Storage(e.to_string()))?;
    let legacy: Vec<(String, String)> = {
        let mut stmt = tx
            .prepare("SELECT id, data FROM conversations WHERE updated_at IS NULL")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?
    };
    for (id, data) in legacy {
        let conv: Conversation = match serde_json::from_str(&data) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(conversation_id = %id, "skipping unparseable legacy conversation blob: {e}");
                continue;
            }
        };
        for (idx, msg) in conv.messages.iter().enumerate() {
            insert_message_row(&tx, &id, idx, msg)?;
        }
        upsert_meta_row(&tx, &conv)?;
    }
    tx.commit().map_err(|e| Error::Storage(e.to_string()))
}

/// CRUD operations on conversations backed by SQLite.
///
/// All methods run their rusqlite work on tokio's blocking pool via
/// `spawn_blocking` so async workers never park on disk I/O.
///
/// Storage layout: `conversations` holds one metadata row per
/// conversation (slimmed JSON without messages, plus promoted
/// title/timestamp columns); `messages` holds one row per message keyed
/// by `(conversation_id, idx)`. Per-turn persistence appends message
/// rows instead of rewriting the whole conversation — see
/// [`ConversationStore::save_turn`].
#[derive(Clone)]
pub struct ConversationStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl ConversationStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Create a new empty conversation and return it.
    pub async fn create(&self) -> Result<Conversation, Error> {
        self.create_with_title(None).await
    }

    /// Create a new empty conversation with an optional title and return it.
    pub async fn create_with_title(&self, title: Option<String>) -> Result<Conversation, Error> {
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
        self.save(&conv).await?;
        Ok(conv)
    }

    /// Persist a conversation, rewriting every message row (insert or
    /// update). O(total messages) — use [`Self::save_turn`] for the
    /// per-turn hot path and reserve this for genuine full rewrites
    /// (creation, compaction).
    pub async fn save(&self, conv: &Conversation) -> Result<(), Error> {
        let conv = conv.clone();
        with_conn(&self.conn, move |conn| write_full(conn, &conv)).await
    }

    /// Upsert the conversation's metadata (title, timestamps, summary,
    /// channel fields) without touching its message rows.
    pub async fn save_meta(&self, conv: &Conversation) -> Result<(), Error> {
        let meta = meta_only(conv);
        with_conn(&self.conn, move |conn| upsert_meta_row(conn, &meta)).await
    }

    /// Insert (or overwrite) the single message at position `idx` of a
    /// conversation. Callers appending a turn should follow up with
    /// [`Self::save_meta`] to bump `updated_at` — or use
    /// [`Self::save_turn`], which does both atomically.
    pub async fn append_message(
        &self,
        conversation_id: Uuid,
        idx: usize,
        msg: &Message,
    ) -> Result<(), Error> {
        let msg = msg.clone();
        with_conn(&self.conn, move |conn| {
            insert_message_row(conn, &conversation_id.to_string(), idx, &msg)
        })
        .await
    }

    /// Persist the outcome of an agent turn without rewriting history.
    ///
    /// `persisted_ids` are the ids of the messages that were already
    /// stored when the conversation was loaded — capture them right
    /// after `get()`. If the in-memory conversation still starts with
    /// exactly that sequence, only the new tail is appended (plus the
    /// metadata row, and the message at idx 0, which the orchestrator
    /// rewrites in place with a fresh system prompt on every run). If
    /// the prefix no longer matches — compaction rewrote history — this
    /// falls back to a full [`Self::save`].
    ///
    /// Known in-place edit this deliberately does not persist: the
    /// agent's oversized-summary repair mutates a mid-history message
    /// without changing its id. That repair is deterministic and
    /// re-applied on every load, so losing it on the append path is
    /// harmless.
    pub async fn save_turn(
        &self,
        conv: &Conversation,
        persisted_ids: &[Uuid],
    ) -> Result<(), Error> {
        let prefix_intact = conv.messages.len() >= persisted_ids.len()
            && conv
                .messages
                .iter()
                .zip(persisted_ids.iter())
                .all(|(m, id)| m.id == *id);
        if !prefix_intact {
            return self.save(conv).await;
        }

        let meta = meta_only(conv);
        let first = if persisted_ids.is_empty() {
            None
        } else {
            conv.messages.first().cloned()
        };
        let base = persisted_ids.len();
        let tail: Vec<Message> = conv.messages[base..].to_vec();
        with_conn(&self.conn, move |conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;
            upsert_meta_row(&tx, &meta)?;
            let id = meta.id.to_string();
            if let Some(first) = &first {
                insert_message_row(&tx, &id, 0, first)?;
            }
            for (offset, msg) in tail.iter().enumerate() {
                insert_message_row(&tx, &id, base + offset, msg)?;
            }
            tx.commit().map_err(|e| Error::Storage(e.to_string()))
        })
        .await
    }

    /// Retrieve a conversation by ID, reassembling it from the metadata
    /// row plus its ordered message rows.
    pub async fn get(&self, id: Uuid) -> Result<Conversation, Error> {
        with_conn(&self.conn, move |conn| {
            let data: String = conn
                .query_row(
                    "SELECT data FROM conversations WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        Error::NotFound(format!("conversation {id}"))
                    }
                    other => Error::Storage(other.to_string()),
                })?;
            let mut conv: Conversation = serde_json::from_str(&data)?;

            let mut stmt = conn
                .prepare("SELECT data FROM messages WHERE conversation_id = ?1 ORDER BY idx")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![id.to_string()], |row| row.get::<_, String>(0))
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut messages: Vec<Message> = Vec::new();
            for row in rows {
                let msg_data = row.map_err(|e| Error::Storage(e.to_string()))?;
                messages.push(serde_json::from_str(&msg_data)?);
            }
            // A legacy row not yet migrated keeps its messages in the
            // blob and has no rows here; migrated/new rows keep them
            // only here.
            if !messages.is_empty() {
                conv.messages = messages;
            }
            Ok(conv)
        })
        .await
    }

    /// List all conversation IDs (lightweight, doesn't deserialize messages).
    pub async fn list_ids(&self) -> Result<Vec<Uuid>, Error> {
        with_conn(&self.conn, |conn| {
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
        })
        .await
    }

    /// List all conversation summaries (id, title, timestamps) ordered by
    /// `updated_at` descending — straight off the promoted columns, no
    /// JSON parsing. Rows without column values (unmigrated blobs whose
    /// JSON couldn't be parsed) are skipped instead of failing the whole
    /// list, matching the old behavior.
    pub async fn list_summaries(&self) -> Result<Vec<ConversationSummary>, Error> {
        with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, created_at, updated_at FROM conversations
                     WHERE updated_at IS NOT NULL
                     ORDER BY updated_at DESC",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                let (id, title, created_at, updated_at) =
                    row.map_err(|e| Error::Storage(e.to_string()))?;
                let (Ok(id), Some(created_at), Some(updated_at)) = (
                    Uuid::parse_str(&id),
                    created_at.as_deref().and_then(parse_ts),
                    parse_ts(&updated_at),
                ) else {
                    continue;
                };
                out.push(ConversationSummary {
                    id,
                    title,
                    created_at,
                    updated_at,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Delete a conversation (and its message rows) by ID. Returns
    /// `NotFound` if the conversation does not exist, so callers can
    /// distinguish 404 from 500.
    pub async fn delete(&self, id: Uuid) -> Result<(), Error> {
        with_conn(&self.conn, move |conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;
            tx.execute(
                "DELETE FROM messages WHERE conversation_id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            let affected = tx
                .execute(
                    "DELETE FROM conversations WHERE id = ?1",
                    params![id.to_string()],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if affected == 0 {
                // Dropping the uncommitted transaction rolls back.
                return Err(Error::NotFound(format!("conversation {id}")));
            }
            tx.commit().map_err(|e| Error::Storage(e.to_string()))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::types::{ContentBlock, MessageContent, Role, ToolCall, ToolResult};

    /// Open a `ConversationStore` backed by an in-memory SQLite
    /// connection running the real migrations, so tests exercise the
    /// production schema.
    fn in_memory_store() -> ConversationStore {
        ConversationStore::new(in_memory_conn())
    }

    fn in_memory_conn() -> Arc<Mutex<rusqlite::Connection>> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn msg(role: Role, content: MessageContent) -> Message {
        Message {
            id: Uuid::new_v4(),
            role,
            content,
            created_at: Utc::now(),
        }
    }

    /// One message of every content shape, so round-trip tests cover the
    /// full serde surface.
    fn all_shapes() -> Vec<Message> {
        vec![
            msg(Role::System, MessageContent::Text("system prompt".into())),
            msg(Role::User, MessageContent::Text("hello".into())),
            msg(
                Role::Assistant,
                MessageContent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "web_fetch".into(),
                    arguments: serde_json::json!({"url": "https://example.com"}),
                }),
            ),
            msg(
                Role::Tool,
                MessageContent::ToolResult(ToolResult {
                    call_id: "call-1".into(),
                    output: serde_json::json!({"status": 200}),
                    is_error: false,
                    images: vec![],
                }),
            ),
            msg(
                Role::Assistant,
                MessageContent::MultiToolCall(vec![
                    ToolCall {
                        id: "call-2".into(),
                        name: "a".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolCall {
                        id: "call-3".into(),
                        name: "b".into(),
                        arguments: serde_json::json!({"x": 1}),
                    },
                ]),
            ),
            msg(
                Role::User,
                MessageContent::MultiPart(vec![
                    ContentBlock::Text {
                        text: "look at this".into(),
                    },
                    ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: vec![0x89, 0x50, 0x4e, 0x47],
                    },
                ]),
            ),
        ]
    }

    fn assert_conv_eq(a: &Conversation, b: &Conversation) {
        // Conversation doesn't derive PartialEq; compare via JSON, which
        // is also the persisted representation.
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
    }

    #[tokio::test]
    async fn create_with_title_persists_title_and_round_trips() {
        let store = in_memory_store();
        let conv = store
            .create_with_title(Some("hello".into()))
            .await
            .expect("create");
        assert_eq!(conv.title.as_deref(), Some("hello"));
        let reloaded = store.get(conv.id).await.expect("get");
        assert_eq!(reloaded.title.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn create_defaults_title_to_none() {
        let store = in_memory_store();
        let conv = store.create().await.expect("create");
        assert!(conv.title.is_none());
    }

    #[tokio::test]
    async fn save_get_round_trips_every_message_shape() {
        let store = in_memory_store();
        let mut conv = store
            .create_with_title(Some("shapes".into()))
            .await
            .unwrap();
        conv.messages = all_shapes();
        conv.summary = Some("a summary".into());
        conv.detected_profile = Some("default".into());
        conv.channel_source = Some("telegram".into());
        conv.channel_id = Some("42".into());
        conv.channel_thread_id = Some("7".into());
        conv.updated_at = Utc::now();
        store.save(&conv).await.unwrap();

        let reloaded = store.get(conv.id).await.unwrap();
        assert_conv_eq(&reloaded, &conv);
    }

    #[tokio::test]
    async fn append_then_get_matches_in_memory_conversation() {
        let store = in_memory_store();
        let mut conv = store.create().await.unwrap();
        let persisted: Vec<Uuid> = conv.messages.iter().map(|m| m.id).collect();

        conv.messages = all_shapes();
        conv.updated_at = Utc::now();
        store.save_turn(&conv, &persisted).await.unwrap();
        let reloaded = store.get(conv.id).await.unwrap();
        assert_conv_eq(&reloaded, &conv);

        // Second turn: append on top of what's now persisted.
        let persisted: Vec<Uuid> = reloaded.messages.iter().map(|m| m.id).collect();
        conv.messages
            .push(msg(Role::User, MessageContent::Text("second turn".into())));
        conv.messages.push(msg(
            Role::Assistant,
            MessageContent::Text("second reply".into()),
        ));
        conv.updated_at = Utc::now();
        store.save_turn(&conv, &persisted).await.unwrap();
        let reloaded = store.get(conv.id).await.unwrap();
        assert_conv_eq(&reloaded, &conv);
        assert_eq!(reloaded.messages.len(), all_shapes().len() + 2);
    }

    #[tokio::test]
    async fn save_turn_rewrites_in_place_system_prompt_at_idx_zero() {
        let store = in_memory_store();
        let mut conv = store.create().await.unwrap();
        conv.messages = vec![
            msg(Role::System, MessageContent::Text("old prompt".into())),
            msg(Role::User, MessageContent::Text("hi".into())),
        ];
        store.save(&conv).await.unwrap();

        let persisted: Vec<Uuid> = conv.messages.iter().map(|m| m.id).collect();
        // The orchestrator rewrites the system message in place (same id).
        conv.messages[0].content = MessageContent::Text("fresh prompt".into());
        conv.messages
            .push(msg(Role::Assistant, MessageContent::Text("hello".into())));
        store.save_turn(&conv, &persisted).await.unwrap();

        let reloaded = store.get(conv.id).await.unwrap();
        assert_conv_eq(&reloaded, &conv);
        assert_eq!(reloaded.messages[0].content.as_text(), Some("fresh prompt"));
    }

    #[tokio::test]
    async fn save_turn_falls_back_to_full_rewrite_after_compaction() {
        let store = in_memory_store();
        let mut conv = store.create().await.unwrap();
        conv.messages = all_shapes();
        store.save(&conv).await.unwrap();
        let persisted: Vec<Uuid> = conv.messages.iter().map(|m| m.id).collect();

        // Simulate compaction: history collapses to a fresh summary
        // message plus the latest turn — the persisted prefix no longer
        // matches, so save_turn must fall back to a full rewrite.
        conv.messages = vec![
            msg(Role::System, MessageContent::Text("system".into())),
            msg(
                Role::Assistant,
                MessageContent::Text("summary of earlier turns".into()),
            ),
            msg(Role::User, MessageContent::Text("latest turn".into())),
        ];
        conv.summary = Some("summary of earlier turns".into());
        conv.updated_at = Utc::now();
        store.save_turn(&conv, &persisted).await.unwrap();

        let reloaded = store.get(conv.id).await.unwrap();
        assert_conv_eq(&reloaded, &conv);
        // Stale rows past the compacted history must be gone — an
        // append-only write here would leave the six original rows.
        assert_eq!(reloaded.messages.len(), 3);
    }

    #[tokio::test]
    async fn legacy_blob_rows_migrate_to_normalized_schema() {
        let conn = in_memory_conn();

        // Insert a legacy-format row raw: the whole conversation
        // (messages included) as one JSON blob, no column metadata.
        let legacy = Conversation {
            id: Uuid::new_v4(),
            messages: all_shapes(),
            created_at: Utc::now() - chrono::Duration::days(2),
            updated_at: Utc::now() - chrono::Duration::days(1),
            title: Some("legacy".into()),
            summary: Some("old summary".into()),
            detected_profile: None,
            channel_source: Some("signal".into()),
            channel_id: Some("+15550100".into()),
            channel_thread_id: None,
        };
        {
            let guard = conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO conversations (id, data) VALUES (?1, ?2)",
                    params![
                        legacy.id.to_string(),
                        serde_json::to_string(&legacy).unwrap()
                    ],
                )
                .unwrap();
            // Re-run migrations against a database that now has a legacy
            // row — this is what startup does. Run twice to prove
            // idempotence.
            crate::Store::run_migrations(&guard).unwrap();
            crate::Store::run_migrations(&guard).unwrap();

            // The blob must have been slimmed and the messages exploded.
            let slim: String = guard
                .query_row(
                    "SELECT data FROM conversations WHERE id = ?1",
                    params![legacy.id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let slim_conv: Conversation = serde_json::from_str(&slim).unwrap();
            assert!(slim_conv.messages.is_empty(), "blob still embeds messages");
            let n: i64 = guard
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                    params![legacy.id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n as usize, legacy.messages.len());
        }

        let store = ConversationStore::new(conn);
        let reloaded = store.get(legacy.id).await.unwrap();
        assert_conv_eq(&reloaded, &legacy);

        let summaries = store.list_summaries().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, legacy.id);
        assert_eq!(summaries[0].title.as_deref(), Some("legacy"));
        assert_eq!(summaries[0].created_at, legacy.created_at);
        assert_eq!(summaries[0].updated_at, legacy.updated_at);
    }

    #[tokio::test]
    async fn list_summaries_returns_entries_sorted_desc_by_updated_at() {
        let store = in_memory_store();
        let mut a = store.create_with_title(Some("a".into())).await.unwrap();
        let mut b = store.create_with_title(Some("b".into())).await.unwrap();
        // Force `b` to be older than `a`.
        b.updated_at = a.updated_at - chrono::Duration::seconds(60);
        store.save(&b).await.unwrap();
        a.updated_at = Utc::now();
        store.save(&a).await.unwrap();

        let summaries = store.list_summaries().await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[0].id, a.id,
            "newer conversation should come first"
        );
        assert_eq!(summaries[0].title.as_deref(), Some("a"));
        assert_eq!(summaries[1].id, b.id);
    }

    #[tokio::test]
    async fn save_meta_updates_metadata_without_touching_messages() {
        let store = in_memory_store();
        let mut conv = store.create().await.unwrap();
        conv.messages = vec![msg(Role::User, MessageContent::Text("hi".into()))];
        store.save(&conv).await.unwrap();

        conv.title = Some("renamed".into());
        conv.updated_at = Utc::now();
        store.save_meta(&conv).await.unwrap();

        let reloaded = store.get(conv.id).await.unwrap();
        assert_conv_eq(&reloaded, &conv);
    }

    #[tokio::test]
    async fn delete_removes_message_rows() {
        let store = in_memory_store();
        let mut conv = store.create().await.unwrap();
        conv.messages = all_shapes();
        store.save(&conv).await.unwrap();
        store.delete(conv.id).await.unwrap();
        let err = store.get(conv.id).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));

        // A new conversation reusing storage must not see orphan rows.
        let fresh = store.create().await.unwrap();
        let reloaded = store.get(fresh.id).await.unwrap();
        assert!(reloaded.messages.is_empty());
    }

    #[tokio::test]
    async fn get_returns_not_found_for_unknown_id() {
        let store = in_memory_store();
        let err = store.get(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }
}
