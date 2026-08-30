mod chat_map;
mod conversation;
pub mod credential_backend;
mod credential_request;
mod device;
mod guarded;
mod jobs;
pub mod keychain;
mod outcomes;
mod recall_archive;
pub mod registry;
mod secret;
mod slack_chat_map;
mod tasks;

use std::path::Path;
use std::sync::Arc;

use rustykrab_core::Error;
use std::sync::Mutex;
use zeroize::Zeroizing;

pub use chat_map::ChatMapStore;
pub use conversation::{ConversationStore, ConversationSummary};
pub use credential_request::{
    CredentialRequest, CredentialRequestStore, RequestAction, RequestNotifier, RequestedField,
};
pub use device::{Device, DeviceStore, Principal};
pub use guarded::{GuardedSecrets, WriteOutcome};
pub use jobs::{JobRun, JobStore, ScheduledJob};
pub use outcomes::OutcomeStore;
pub use recall_archive::RecallArchiveStore;
pub use secret::{SecretMeta, SecretStore, WriteAuthority};
pub use slack_chat_map::SlackChatMapStore;
pub use tasks::{DelegatedTask, TaskStatus, TaskStore};

/// Top-level database handle wrapping a SQLite connection.
///
/// The master key is wrapped in `Zeroizing` so it is securely erased
/// from memory when the Store is dropped.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<rusqlite::Connection>>,
    master_key: Zeroizing<Vec<u8>>,
    /// Told when the agent files a credential change, so the user can be
    /// notified. `None` when push isn't configured, which is normal.
    request_notifier: Option<Arc<dyn credential_request::RequestNotifier>>,
    /// Where live credential values are kept. Never the database.
    credential_backend: Arc<dyn credential_backend::CredentialBackend>,
}

impl Store {
    /// Open (or create) a store at the given directory path.
    ///
    /// `master_key` is used to encrypt secrets at rest. It should be
    /// sourced from the OS keychain or an environment variable — never
    /// stored alongside the database.
    pub fn open(path: impl AsRef<Path>, master_key: Vec<u8>) -> Result<Self, Error> {
        std::fs::create_dir_all(path.as_ref()).map_err(|e| {
            Error::Storage(format!(
                "cannot create store directory {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let db_path = path.as_ref().join("store.db");
        let conn =
            rusqlite::Connection::open(&db_path).map_err(|e| Error::Storage(e.to_string()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -65536;
             PRAGMA mmap_size = 268435456;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        Self::run_migrations(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            master_key: Zeroizing::new(master_key),
            request_notifier: None,
            credential_backend: credential_backend::default_backend(),
        })
    }

    pub(crate) fn run_migrations(conn: &rusqlite::Connection) -> Result<(), Error> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS conversations (
                id         TEXT PRIMARY KEY,
                data       TEXT NOT NULL,
                title      TEXT,
                created_at TEXT,
                updated_at TEXT
            );

            CREATE TABLE IF NOT EXISTS messages (
                conversation_id TEXT NOT NULL,
                idx             INTEGER NOT NULL,
                data            TEXT NOT NULL,
                PRIMARY KEY (conversation_id, idx)
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
                thread_id       TEXT,
                one_shot        INTEGER NOT NULL DEFAULT 0,
                enabled         INTEGER NOT NULL DEFAULT 1,
                next_run_at     TEXT NOT NULL,
                last_run_at     TEXT,
                created_at      TEXT NOT NULL,
                conversation_id TEXT,
                created_version TEXT
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
                finished_at TEXT NOT NULL,
                rustykrab_version TEXT
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

            CREATE TABLE IF NOT EXISTS slack_chat_map (
                team_id    TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                thread_ts  TEXT NOT NULL DEFAULT '',
                conv_id    TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(team_id, channel_id, thread_ts)
            );

            CREATE TABLE IF NOT EXISTS recall_archive (
                conversation_id TEXT PRIMARY KEY,
                archive         TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            -- Credential guard (docs/plans/apollo-ios-and-credential-guard.md).
            -- An agent-authored change to an existing credential lands here
            -- instead of being applied, and the user resolves it.
            CREATE TABLE IF NOT EXISTS credential_requests (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                action          TEXT NOT NULL,      -- 'update' | 'delete'
                proposed_data   BLOB,               -- encrypted; NULL for delete
                reason          TEXT,
                conversation_id TEXT,
                status          TEXT NOT NULL DEFAULT 'pending',
                created_at      INTEGER NOT NULL,
                decided_at      INTEGER,
                decided_by      TEXT,
                -- Version of the target when the request was filed, so a
                -- stale approval cannot clobber a newer user edit.
                target_version  INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_credential_requests_pending
                ON credential_requests (name) WHERE status = 'pending';

            -- Superseded values, kept encrypted, so an approved change or a
            -- mistaken delete is recoverable.
            CREATE TABLE IF NOT EXISTS secret_versions (
                name        TEXT NOT NULL,
                version     INTEGER NOT NULL,
                data        BLOB NOT NULL,
                replaced_at INTEGER NOT NULL,
                replaced_by TEXT NOT NULL,
                PRIMARY KEY (name, version)
            );

            CREATE TABLE IF NOT EXISTS secret_audit (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL,
                op         TEXT NOT NULL,   -- create|overwrite|delete|approve|deny
                authority  TEXT NOT NULL,
                request_id TEXT,
                at         INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_secret_audit_name
                ON secret_audit (name, at DESC);

            -- Paired devices. Tokens are stored only as SHA-256 hashes, so
            -- reading this table yields nothing that can authenticate.
            -- Revoked rows are kept so audit entries naming them resolve.
            CREATE TABLE IF NOT EXISTS devices (
                id           TEXT PRIMARY KEY,
                name         TEXT NOT NULL,
                token_hash   BLOB NOT NULL,
                created_at   INTEGER NOT NULL,
                last_seen_at INTEGER,
                revoked_at   INTEGER,
                push_token   TEXT
            );

            -- Tasks a peer node delegated to this instance. The caller
            -- holds only the id, so the row is the authoritative record of
            -- a delegation's progress and result.
            CREATE TABLE IF NOT EXISTS delegated_tasks (
                id              TEXT PRIMARY KEY,
                message         TEXT NOT NULL,
                conversation_id TEXT,
                status          TEXT NOT NULL,
                result          TEXT,
                error           TEXT,
                principal       TEXT,
                hop_budget      INTEGER NOT NULL DEFAULT 0,
                trace_id        TEXT,
                created_at      TEXT NOT NULL,
                started_at      TEXT,
                finished_at     TEXT
            );

            -- The worker's only hot query is the oldest queued task.
            CREATE INDEX IF NOT EXISTS idx_delegated_tasks_queue
                ON delegated_tasks (status, created_at);

            -- One-time pairing codes: hashed, short-lived, single use.
            CREATE TABLE IF NOT EXISTS pairing_codes (
                code_hash  BLOB PRIMARY KEY,
                expires_at INTEGER NOT NULL,
                used_at    INTEGER
            );
            -- Outcome instrumentation (see DREAMING.md). Written once,
            -- never updated: per-artifact tallies are derived by
            -- aggregating these rows, so scoring can change without
            -- losing the underlying evidence.
            CREATE TABLE IF NOT EXISTS outcome_records (
                id              TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                session_id      TEXT NOT NULL,
                recorded_at     TEXT NOT NULL,
                verdict         TEXT NOT NULL
                    CHECK (verdict IN ('success','failure','ambiguous')),
                signal          TEXT NOT NULL
                    CHECK (signal IN ('verifiable','explicit','implicit','judge')),
                confidence      REAL NOT NULL DEFAULT 1.0,
                detail          TEXT,
                tool_calls      INTEGER NOT NULL DEFAULT 0,
                tool_failures   INTEGER NOT NULL DEFAULT 0,
                iterations      INTEGER NOT NULL DEFAULT 0,
                compactions     INTEGER NOT NULL DEFAULT 0,
                rustykrab_version TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_outcome_records_recorded_at
                ON outcome_records (recorded_at DESC);

            CREATE INDEX IF NOT EXISTS idx_outcome_records_conversation
                ON outcome_records (conversation_id, recorded_at DESC);

            -- Credit assignment: which artifacts were in play for a turn.
            -- Without this an outcome says only 'that turn went badly',
            -- which is not actionable.
            CREATE TABLE IF NOT EXISTS outcome_attributions (
                record_id TEXT NOT NULL
                    REFERENCES outcome_records(id) ON DELETE CASCADE,
                kind      TEXT NOT NULL
                    CHECK (kind IN ('skill','memory','tool')),
                target_id TEXT NOT NULL,
                PRIMARY KEY (record_id, kind, target_id)
            );

            CREATE INDEX IF NOT EXISTS idx_outcome_attributions_target
                ON outcome_attributions (kind, target_id);
            ",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        // Additive migrations for pre-existing databases. `PRAGMA table_info`
        // lists current columns; only ALTER if a column is missing.
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
        if !existing.iter().any(|c| c == "thread_id") {
            conn.execute("ALTER TABLE scheduled_jobs ADD COLUMN thread_id TEXT", [])
                .map_err(|e| Error::Storage(e.to_string()))?;
        }
        if !existing.iter().any(|c| c == "created_version") {
            conn.execute(
                "ALTER TABLE scheduled_jobs ADD COLUMN created_version TEXT",
                [],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        }

        // `service` and `fields` arrived with fulfil requests, where the
        // agent asks the user for a credential instead of proposing one.
        // Rows predating them are update/delete requests, which have no
        // fields to render, so NULL is the correct value and there is
        // nothing to back-fill.
        let mut stmt = conn
            .prepare("PRAGMA table_info(credential_requests)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| Error::Storage(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for (column, ddl) in [
            (
                "service",
                "ALTER TABLE credential_requests ADD COLUMN service TEXT",
            ),
            (
                "fields",
                "ALTER TABLE credential_requests ADD COLUMN fields TEXT",
            ),
            // A one-time link the user opens to answer a fulfil request.
            // Only the hash is stored: the token is shown once, in the
            // message the agent sends, and is unrecoverable from the
            // database afterwards.
            (
                "link_token_hash",
                "ALTER TABLE credential_requests ADD COLUMN link_token_hash TEXT",
            ),
            (
                "link_expires_at",
                "ALTER TABLE credential_requests ADD COLUMN link_expires_at INTEGER",
            ),
        ] {
            if !existing.iter().any(|c| c == column) {
                conn.execute(ddl, [])
                    .map_err(|e| Error::Storage(e.to_string()))?;
            }
        }

        // Versioning columns on `secrets`. Rows written before the guard
        // existed keep NULL timestamps — back-filling them with "now" would
        // claim every old credential was created at upgrade time — and read
        // as version 1.
        let mut stmt = conn
            .prepare("PRAGMA table_info(secrets)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;
        drop(stmt);
        for (column, ddl) in [
            (
                "created_at",
                "ALTER TABLE secrets ADD COLUMN created_at INTEGER",
            ),
            (
                "updated_at",
                "ALTER TABLE secrets ADD COLUMN updated_at INTEGER",
            ),
            (
                "version",
                "ALTER TABLE secrets ADD COLUMN version INTEGER NOT NULL DEFAULT 1",
            ),
        ] {
            if !existing.iter().any(|c| c == column) {
                conn.execute(ddl, [])
                    .map_err(|e| Error::Storage(e.to_string()))?;
            }
        }

        // `job_runs.rustykrab_version` records which build executed each run.
        // Rows written before this column existed stay NULL rather than being
        // back-filled with the current version, which would misattribute them.
        let mut stmt = conn
            .prepare("PRAGMA table_info(job_runs)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;
        drop(stmt);
        if !existing.iter().any(|c| c == "rustykrab_version") {
            conn.execute("ALTER TABLE job_runs ADD COLUMN rustykrab_version TEXT", [])
                .map_err(|e| Error::Storage(e.to_string()))?;
        }

        // Databases created before conversations were normalized have a
        // two-column `conversations` table; add the promoted metadata
        // columns so `list_summaries` never has to parse JSON.
        let mut stmt = conn
            .prepare("PRAGMA table_info(conversations)")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| Error::Storage(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Storage(e.to_string()))?;
        drop(stmt);
        for col in ["title", "created_at", "updated_at"] {
            if !existing.iter().any(|c| c == col) {
                conn.execute(
                    &format!("ALTER TABLE conversations ADD COLUMN {col} TEXT"),
                    [],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            }
        }
        // Index created after the ALTERs so it exists on both fresh and
        // upgraded databases.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_conversations_updated_at
                 ON conversations (updated_at DESC);",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        // Explode legacy whole-conversation blobs into the normalized
        // schema. Idempotent — see `conversation::migrate_legacy_blobs`.
        conversation::migrate_legacy_blobs(conn)?;

        Ok(())
    }

    /// Return a handle for conversation operations.
    pub fn conversations(&self) -> ConversationStore {
        ConversationStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for encrypted secret operations.
    /// Choose where live credential values are kept.
    ///
    /// Defaults to the platform's own store (the Keychain on macOS,
    /// nothing elsewhere). Tests and harnesses pass
    /// [`credential_backend::MemoryBackend`] so they never touch the real
    /// one — an early version of this work deposited a developer's Gmail
    /// app password into their login keychain from a unit test.
    pub fn with_credential_backend(
        mut self,
        backend: Arc<dyn credential_backend::CredentialBackend>,
    ) -> Self {
        self.credential_backend = backend;
        self
    }

    pub fn secrets(&self) -> SecretStore {
        SecretStore::new(
            Arc::clone(&self.conn),
            self.master_key.clone(),
            Arc::clone(&self.credential_backend),
        )
    }

    /// Attach a notifier that is told whenever the agent files a change.
    pub fn with_request_notifier(
        mut self,
        notifier: Arc<dyn credential_request::RequestNotifier>,
    ) -> Self {
        self.request_notifier = Some(notifier);
        self
    }

    /// Handle for the credential-change requests the agent files and the
    /// user resolves.
    pub fn credential_requests(&self) -> CredentialRequestStore {
        let requests = CredentialRequestStore::new(Arc::clone(&self.conn), self.secrets());
        match &self.request_notifier {
            Some(notifier) => requests.with_notifier(Arc::clone(notifier)),
            None => requests,
        }
    }

    /// The agent-facing view of credential storage: create-only, with
    /// overwrites and deletes queued for approval. Tools receive this
    /// rather than [`SecretStore`], so the guard cannot be bypassed by
    /// forgetting a check.
    pub fn guarded_secrets(&self) -> GuardedSecrets {
        GuardedSecrets::new(self.secrets(), self.credential_requests())
    }

    /// Handle for paired devices and pairing codes.
    pub fn devices(&self) -> DeviceStore {
        DeviceStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for the delegated-task queue drained by the
    /// node worker (`rustykrab_gateway::tasks`).
    pub fn tasks(&self) -> TaskStore {
        TaskStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for scheduled-job operations.
    pub fn jobs(&self) -> JobStore {
        JobStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for outcome-record persistence (see `DREAMING.md`).
    pub fn outcomes(&self) -> OutcomeStore {
        OutcomeStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for Telegram chat/thread → conversation mapping.
    pub fn chat_map(&self) -> ChatMapStore {
        ChatMapStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for Slack (team, channel, thread) → conversation mapping.
    pub fn slack_chat_map(&self) -> SlackChatMapStore {
        SlackChatMapStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for the durable recall archive (compaction-displaced
    /// history). Implements `RecallPersistence` so it can back a
    /// `RecallStore`.
    pub fn recall_archive(&self) -> RecallArchiveStore {
        RecallArchiveStore::new(Arc::clone(&self.conn))
    }

    /// Flush all pending writes to disk.
    pub async fn flush(&self) -> Result<(), Error> {
        // WAL mode checkpoints automatically; explicit checkpoint for shutdown.
        with_conn(&self.conn, |conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(|e| Error::Storage(e.to_string()))
        })
        .await
    }
}

/// Run a blocking closure on tokio's blocking pool, flattening the join
/// error into a storage error. Keeps rusqlite work (and other CPU-heavy
/// tasks such as Argon2 key derivation) off the async worker threads.
pub(crate) async fn run_blocking<T, F>(f: F) -> Result<T, Error>
where
    F: FnOnce() -> Result<T, Error> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Storage(format!("join error: {e}")))?
}

/// Helper: run a blocking closure on the shared connection inside
/// `spawn_blocking`, mirroring `SqliteMemoryStorage::with_conn` in
/// rustykrab-memory. The mutex is locked on the blocking thread so async
/// workers never park on disk I/O or the connection lock.
pub(crate) async fn with_conn<T, F>(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    f: F,
) -> Result<T, Error>
where
    F: FnOnce(&rusqlite::Connection) -> Result<T, Error> + Send + 'static,
    T: Send + 'static,
{
    let conn = Arc::clone(conn);
    run_blocking(move || {
        let conn = conn.lock().unwrap();
        f(&conn)
    })
    .await
}
