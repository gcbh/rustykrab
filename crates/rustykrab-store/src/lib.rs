mod channel_binding;
mod conversation;
pub mod credential_backend;
mod credential_request;
mod device;
mod dream_reports;
mod guarded;
mod jobs;
pub mod keychain;
mod outcomes;
mod projects;
mod recall_archive;
pub mod registry;
mod secret;
mod tasks;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustykrab_core::Error;
use std::sync::Mutex;
use zeroize::Zeroizing;

pub use channel_binding::{ChannelAddress, ChannelBindingStore};
pub use conversation::{ConversationStore, ConversationSummary};
pub mod pending_links;
pub use credential_request::{
    CredentialRequest, CredentialRequestStore, RequestAction, RequestNotifier, RequestedField,
};
pub use device::{Device, DeviceStore, Principal};
pub use dream_reports::{DreamReportStore, StoredReport};
pub use guarded::{GuardedSecrets, WriteOutcome};
pub use jobs::{JobRun, JobStore, ScheduledJob};
pub use outcomes::OutcomeStore;
pub use pending_links::PendingLinks;
pub use projects::{ApplyRevisionResult, ProjectStore};
pub use recall_archive::RecallArchiveStore;
pub use secret::{SecretMeta, SecretStore, WriteAuthority};
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
    /// Credential links minted this turn, waiting to be sent to the user
    /// once the agent has finished speaking. In memory only.
    pending_links: PendingLinks,
    /// Where the database lives, so a background reader can open its own
    /// connection instead of queueing behind live traffic on this one.
    db_path: PathBuf,
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
            pending_links: PendingLinks::new(),
            db_path,
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
                conversation_id TEXT NOT NULL
                    REFERENCES conversations(id) ON DELETE CASCADE,
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
                created_version TEXT,
                -- IANA zone the `schedule` string is written in. Every
                -- timestamp column above stays UTC; this records the lens
                -- those UTC instants were derived through, so each advance
                -- of next_run_at re-reads the offset from the zone database
                -- and the job holds its wall-clock time across DST.
                timezone        TEXT NOT NULL DEFAULT 'UTC'
            );

            CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_due
                ON scheduled_jobs (next_run_at)
                WHERE enabled = 1;

            CREATE TABLE IF NOT EXISTS job_runs (
                id         TEXT PRIMARY KEY,
                job_id     TEXT NOT NULL
                    REFERENCES scheduled_jobs(id) ON DELETE CASCADE,
                status     TEXT NOT NULL,
                output     TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                rustykrab_version TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_job_runs_job_id
                ON job_runs (job_id, finished_at DESC);

            -- Which conversation a channel address is bound to, for every
            -- channel. `external_key` is the channel's addressing tuple
            -- flattened by `ChannelAddress::external_key`, so the spelling is
            -- derived in one place rather than at each call site.
            --
            -- The foreign key is the point of the table: a binding that
            -- outlives its conversation makes the channel fail every later
            -- message, because the id it resolves no longer loads.
            CREATE TABLE IF NOT EXISTS channel_bindings (
                channel      TEXT NOT NULL,
                external_key TEXT NOT NULL,
                conv_id      TEXT NOT NULL
                    REFERENCES conversations(id) ON DELETE CASCADE,
                created_at   TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (channel, external_key)
            );

            -- Covers the cascade, which deletes by `conv_id`.
            CREATE INDEX IF NOT EXISTS idx_channel_bindings_conv
                ON channel_bindings (conv_id);

            CREATE TABLE IF NOT EXISTS recall_archive (
                conversation_id TEXT PRIMARY KEY
                    REFERENCES conversations(id) ON DELETE CASCADE,
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
            -- `conversation_id` here, on `scheduled_jobs` and on
            -- `credential_requests` is deliberately not a foreign key. Those
            -- rows outlive the conversation on purpose: a cron job keeps its
            -- own delivery channel and should keep firing, and a credential
            -- request is audit-relevant after the conversation that filed it
            -- is gone. The column records where the row came from, not
            -- something it depends on.
            CREATE TABLE IF NOT EXISTS delegated_tasks (
                id              TEXT PRIMARY KEY,
                message         TEXT NOT NULL,
                conversation_id TEXT,
                status          TEXT NOT NULL,
                result          TEXT,
                error           TEXT,
                principal       TEXT,
                hop_budget      INTEGER NOT NULL DEFAULT 0,
                allowed_tools   TEXT,
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

            -- Analysis passes, kept so the phase gate can be read rather
            -- than inferred. Without this the only record that the outer
            -- loop ran at all is a log line, and 'reports show real,
            -- actionable patterns' is not a question anyone can answer.
            CREATE TABLE IF NOT EXISTS dream_reports (
                id            TEXT PRIMARY KEY,
                generated_at  TEXT NOT NULL,
                readiness     TEXT NOT NULL,
                total_records INTEGER NOT NULL,
                summary       TEXT NOT NULL,
                report        TEXT NOT NULL   -- full AnalysisReport as JSON
            );

            CREATE INDEX IF NOT EXISTS idx_dream_reports_generated_at
                ON dream_reports (generated_at DESC);

            -- Conversational project planning. Revisions are immutable,
            -- content-addressed snapshots; the project row points at the
            -- current one. Request ids make both project creation and plan
            -- changes safe to retry after an interrupted response.
            CREATE TABLE IF NOT EXISTS projects (
                id                        TEXT PRIMARY KEY,
                create_request_id         TEXT NOT NULL UNIQUE,
                repository_id             TEXT,
                canonical_conversation_id TEXT,
                title                     TEXT NOT NULL,
                status                    TEXT NOT NULL,
                judgment_policy           TEXT NOT NULL,
                current_revision          TEXT,
                create_request            TEXT NOT NULL,
                data                      TEXT NOT NULL,
                created_at                TEXT NOT NULL,
                updated_at                TEXT NOT NULL,
                FOREIGN KEY (id, current_revision)
                    REFERENCES project_revisions(project_id, id)
                    DEFERRABLE INITIALLY DEFERRED
            );

            CREATE TABLE IF NOT EXISTS project_revisions (
                id                TEXT PRIMARY KEY,
                project_id        TEXT NOT NULL
                    REFERENCES projects(id) ON DELETE CASCADE,
                parent_revision   TEXT,
                sequence          INTEGER NOT NULL,
                request_id        TEXT NOT NULL,
                request_data      TEXT NOT NULL,
                author            TEXT NOT NULL,
                conversation_id   TEXT,
                source_message_id TEXT,
                summary           TEXT NOT NULL,
                project_data      TEXT NOT NULL,
                data              TEXT NOT NULL,
                created_at        TEXT NOT NULL,
                UNIQUE (project_id, sequence),
                UNIQUE (project_id, request_id),
                UNIQUE (project_id, id),
                FOREIGN KEY (project_id, parent_revision)
                    REFERENCES project_revisions(project_id, id)
            );

            CREATE INDEX IF NOT EXISTS idx_project_revisions_project
                ON project_revisions (project_id, sequence);

            -- Nodes and edges are materialized per revision for indexed
            -- inspection. Their complete typed representation, including
            -- provenance and decision/question detail, remains in `data`;
            -- the immutable revision snapshot is the reconstruction source.
            CREATE TABLE IF NOT EXISTS plan_nodes (
                revision_id TEXT NOT NULL,
                project_id  TEXT NOT NULL
                    REFERENCES projects(id) ON DELETE CASCADE,
                id          TEXT NOT NULL,
                kind        TEXT NOT NULL,
                data        TEXT NOT NULL,
                PRIMARY KEY (revision_id, id),
                FOREIGN KEY (project_id, revision_id)
                    REFERENCES project_revisions(project_id, id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_plan_nodes_project
                ON plan_nodes (project_id, id);

            CREATE TABLE IF NOT EXISTS plan_edges (
                revision_id TEXT NOT NULL,
                project_id  TEXT NOT NULL
                    REFERENCES projects(id) ON DELETE CASCADE,
                id          TEXT NOT NULL,
                from_node   TEXT NOT NULL,
                relation    TEXT NOT NULL,
                to_node     TEXT NOT NULL,
                data        TEXT NOT NULL,
                PRIMARY KEY (revision_id, id),
                FOREIGN KEY (project_id, revision_id)
                    REFERENCES project_revisions(project_id, id) ON DELETE CASCADE,
                FOREIGN KEY (revision_id, from_node)
                    REFERENCES plan_nodes(revision_id, id) ON DELETE CASCADE,
                FOREIGN KEY (revision_id, to_node)
                    REFERENCES plan_nodes(revision_id, id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_plan_edges_project
                ON plan_edges (project_id, from_node, to_node);
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

        if !existing.iter().any(|c| c == "timezone") {
            // Backfill 'UTC' rather than the operator's zone. Pre-existing
            // rows had their cron fields matched against UTC, so UTC is the
            // lens they were genuinely created under; stamping them with a
            // local zone would reinterpret them and move every live job's
            // fire time by the offset. Rewriting a job to local intent is a
            // decision for whoever owns the job, not for a migration.
            conn.execute(
                "ALTER TABLE scheduled_jobs ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC'",
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

        // Adopt the foreign keys the fresh-database DDL declares onto
        // databases created before it did. Runs after the additive column
        // ALTERs, because the rebuild copies the current column set.
        adopt_declared_foreign_keys(conn)?;

        // Fold the per-channel map tables into `channel_bindings` and drop
        // them. Idempotent — see `channel_binding::migrate_legacy_chat_maps`.
        // Runs before the blob migration so it sees the same `conversations`
        // rows the foreign key will be checked against.
        channel_binding::migrate_legacy_chat_maps(conn)?;

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
    /// Links minted but not yet delivered.
    ///
    /// Shared handle: the tool pushes, the channel that just spoke takes.
    pub fn pending_links(&self) -> PendingLinks {
        self.pending_links.clone()
    }

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

    /// Return a handle for persisted analysis passes (see `DREAMING.md`).
    pub fn dream_reports(&self) -> DreamReportStore {
        DreamReportStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for outcome-record persistence (see `DREAMING.md`).
    pub fn outcomes(&self) -> OutcomeStore {
        OutcomeStore::new(Arc::clone(&self.conn))
    }

    /// A read-only outcome handle on a connection of its own.
    ///
    /// The store is a single `Arc<Mutex<Connection>>`, so even reads
    /// serialize through it: an analysis pass aggregating tens of
    /// thousands of rows blocks live traffic for as long as it runs, WAL
    /// notwithstanding. WAL readers do not block the writer, so the
    /// background loop gets its own connection and stops competing for the
    /// shared one.
    ///
    /// Opened `SQLITE_OPEN_READ_ONLY`, so this cannot become a second
    /// writer by accident.
    pub fn outcomes_reader(&self) -> Result<OutcomeStore, Error> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        // `journal_mode` is a property of the database, not the
        // connection, and setting it from a read-only handle would fail —
        // the WAL is already in place from `open`. These are the
        // per-connection settings that matter to a batch reader.
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -16384;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(OutcomeStore::new_read_only(Arc::new(Mutex::new(conn))))
    }

    /// Return a handle for durable conversational-project planning.
    pub fn projects(&self) -> ProjectStore {
        ProjectStore::new(Arc::clone(&self.conn))
    }

    /// Return a handle for channel address → conversation bindings, for
    /// every channel. Address a row with a [`ChannelAddress`] rather than a
    /// hand-built key.
    pub fn channel_bindings(&self) -> ChannelBindingStore {
        ChannelBindingStore::new(Arc::clone(&self.conn))
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
/// Whether `table` already declares at least one foreign key.
fn has_foreign_key(conn: &rusqlite::Connection, table: &str) -> Result<bool, Error> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .map_err(|e| Error::Storage(e.to_string()))?;
    let mut rows = stmt.query([]).map_err(|e| Error::Storage(e.to_string()))?;
    let present = rows
        .next()
        .map_err(|e| Error::Storage(e.to_string()))?
        .is_some();
    Ok(present)
}

/// Give an existing table a foreign key it was created without.
///
/// SQLite cannot `ALTER TABLE ... ADD CONSTRAINT`, so the only route is the
/// rebuild the SQLite docs prescribe: create the new shape under a temporary
/// name, copy the rows, drop the old table, rename, recreate the indexes.
///
/// `keep_predicate` selects the rows that satisfy the new constraint. Rows it
/// excludes are already orphans — they name a parent that does not exist —
/// and could not be inserted into the new table even if we wanted them. They
/// are dropped, which is what enforcing the constraint means.
///
/// Foreign keys must be off for the duration: with them on, dropping the old
/// table would cascade into the rows we just copied. The pragma is a no-op
/// inside a transaction, so it is toggled outside one and restored after.
/// `PRAGMA foreign_key_check` runs before the commit, so a rebuild that
/// somehow produced a violation rolls back rather than persisting one.
fn adopt_foreign_key(
    conn: &rusqlite::Connection,
    table: &str,
    create_new: &str,
    columns: &str,
    keep_predicate: &str,
    indexes: &[&str],
) -> Result<(), Error> {
    if has_foreign_key(conn, table)? {
        return Ok(());
    }

    let fk_was_on: bool = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(|e| Error::Storage(e.to_string()))?;
    if fk_was_on {
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .map_err(|e| Error::Storage(e.to_string()))?;
    }

    let rebuild = || -> Result<(), Error> {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Storage(e.to_string()))?;
        let staging = format!("{table}_fk_rebuild");
        tx.execute_batch(create_new)
            .map_err(|e| Error::Storage(e.to_string()))?;
        tx.execute_batch(&format!(
            "INSERT INTO {staging} ({columns})
             SELECT {columns} FROM {table} WHERE {keep_predicate};
             DROP TABLE {table};
             ALTER TABLE {staging} RENAME TO {table};"
        ))
        .map_err(|e| Error::Storage(e.to_string()))?;
        for index in indexes {
            tx.execute_batch(index)
                .map_err(|e| Error::Storage(e.to_string()))?;
        }
        let violations: i64 = tx
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_foreign_key_check('{table}')"),
                [],
                |row| row.get(0),
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        if violations > 0 {
            // Dropping the uncommitted transaction rolls the rebuild back.
            return Err(Error::Storage(format!(
                "rebuilding {table} left {violations} foreign-key violations"
            )));
        }
        tx.commit().map_err(|e| Error::Storage(e.to_string()))
    };

    let outcome = rebuild();

    if fk_was_on {
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(|e| Error::Storage(e.to_string()))?;
    }
    outcome
}

/// Adopt the foreign keys the schema declares for fresh databases onto
/// databases created before they were declared.
///
/// Only the references that are genuinely ownership survive here:
/// a message, an archived recall window and a job run have no meaning
/// without their parent. The nullable back-references on `scheduled_jobs`,
/// `credential_requests` and `delegated_tasks` are left alone on purpose —
/// see the comment on their DDL.
fn adopt_declared_foreign_keys(conn: &rusqlite::Connection) -> Result<(), Error> {
    adopt_foreign_key(
        conn,
        "messages",
        "CREATE TABLE messages_fk_rebuild (
             conversation_id TEXT NOT NULL
                 REFERENCES conversations(id) ON DELETE CASCADE,
             idx             INTEGER NOT NULL,
             data            TEXT NOT NULL,
             PRIMARY KEY (conversation_id, idx)
         )",
        "conversation_id, idx, data",
        "conversation_id IN (SELECT id FROM conversations)",
        &[],
    )?;

    adopt_foreign_key(
        conn,
        "recall_archive",
        "CREATE TABLE recall_archive_fk_rebuild (
             conversation_id TEXT PRIMARY KEY
                 REFERENCES conversations(id) ON DELETE CASCADE,
             archive         TEXT NOT NULL,
             created_at      TEXT NOT NULL,
             updated_at      TEXT NOT NULL
         )",
        "conversation_id, archive, created_at, updated_at",
        "conversation_id IN (SELECT id FROM conversations)",
        &[],
    )?;

    adopt_foreign_key(
        conn,
        "job_runs",
        "CREATE TABLE job_runs_fk_rebuild (
             id         TEXT PRIMARY KEY,
             job_id     TEXT NOT NULL
                 REFERENCES scheduled_jobs(id) ON DELETE CASCADE,
             status     TEXT NOT NULL,
             output     TEXT,
             started_at TEXT NOT NULL,
             finished_at TEXT NOT NULL,
             rustykrab_version TEXT
         )",
        "id, job_id, status, output, started_at, finished_at, rustykrab_version",
        "job_id IN (SELECT id FROM scheduled_jobs)",
        &["CREATE INDEX IF NOT EXISTS idx_job_runs_job_id
               ON job_runs (job_id, finished_at DESC)"],
    )
}

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
        // Recover a poisoned lock rather than propagating the panic.
        //
        // Every database call in the process goes through here, so
        // `unwrap()` turned one panic while holding the lock into a
        // permanent outage: the mutex stayed poisoned and every subsequent
        // call panicked with it. The data is not damaged by a panic — the
        // connection is unchanged, and any transaction that was open is
        // rolled back when its guard drops — so continuing is both safe and
        // the only option that recovers.
        //
        // `RetrievalLog` and `AppState::rotate_token` already do this.
        let conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&conn)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database created before the foreign keys were declared: no
    /// constraints, and rows that a constraint would have prevented.
    fn legacy_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, data TEXT NOT NULL,
                 title TEXT, created_at TEXT, updated_at TEXT
             );
             CREATE TABLE messages (
                 conversation_id TEXT NOT NULL, idx INTEGER NOT NULL,
                 data TEXT NOT NULL, PRIMARY KEY (conversation_id, idx)
             );
             CREATE TABLE recall_archive (
                 conversation_id TEXT PRIMARY KEY, archive TEXT NOT NULL,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL
             );
             CREATE TABLE scheduled_jobs (
                 id TEXT PRIMARY KEY, schedule TEXT NOT NULL, task TEXT NOT NULL,
                 channel TEXT, chat_id TEXT, thread_id TEXT,
                 one_shot INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1,
                 next_run_at TEXT NOT NULL, last_run_at TEXT, created_at TEXT NOT NULL
             );
             CREATE TABLE job_runs (
                 id TEXT PRIMARY KEY, job_id TEXT NOT NULL, status TEXT NOT NULL,
                 output TEXT, started_at TEXT NOT NULL, finished_at TEXT NOT NULL
             );

             INSERT INTO conversations (id, data) VALUES ('live', '{}');
             INSERT INTO messages VALUES ('live', 0, '{}');
             INSERT INTO messages VALUES ('deleted-conv', 0, '{}');
             INSERT INTO recall_archive VALUES ('live', '[]', 't', 't');
             INSERT INTO recall_archive VALUES ('deleted-conv', '[]', 't', 't');
             INSERT INTO scheduled_jobs (id, schedule, task, next_run_at, created_at)
                 VALUES ('job', '0 9 * * *', 'task', '2099-01-01T00:00:00+00:00', 't');
             INSERT INTO job_runs VALUES ('run', 'job', 'ok', 'out', 't', 't');
             INSERT INTO job_runs VALUES ('orphan-run', 'deleted-job', 'ok', 'out', 't', 't');",
        )
        .unwrap();
        conn
    }

    fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn migration_adopts_the_foreign_keys_and_drops_the_orphans_they_forbid() {
        let conn = legacy_db();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        Store::run_migrations(&conn).unwrap();

        for table in ["messages", "recall_archive", "job_runs"] {
            assert!(
                has_foreign_key(&conn, table).unwrap(),
                "{table} must carry its foreign key after migration"
            );
        }
        // Rows naming a parent that does not exist cannot survive the
        // constraint, and are exactly what the constraint exists to prevent.
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM recall_archive"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM job_runs"), 1);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM job_runs WHERE id = 'run'"),
            1,
            "the run whose job still exists must be preserved"
        );
    }

    #[test]
    fn adopting_foreign_keys_preserves_the_data_and_the_index() {
        let conn = legacy_db();
        Store::run_migrations(&conn).unwrap();

        let data: String = conn
            .query_row(
                "SELECT data FROM messages WHERE conversation_id = 'live' AND idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(data, "{}");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_job_runs_job_id'"
            ),
            1,
            "the rebuild must put back the index it dropped with the table"
        );
        // And the version column the additive ALTERs added is still there.
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM pragma_table_info('job_runs')
                 WHERE name = 'rustykrab_version'"
            ),
            1
        );
    }

    #[test]
    fn adopting_foreign_keys_is_idempotent() {
        let conn = legacy_db();
        Store::run_migrations(&conn).unwrap();
        let after_first = count(&conn, "SELECT COUNT(*) FROM messages");
        Store::run_migrations(&conn).unwrap();
        Store::run_migrations(&conn).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), after_first);
    }

    #[tokio::test]
    async fn deleting_a_conversation_cascades_to_everything_it_owns() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        Store::run_migrations(&conn).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let conv = uuid::Uuid::new_v4();
        {
            let guard = conn.lock().unwrap();
            guard
                .execute(
                    "INSERT INTO conversations (id, data, created_at, updated_at)
                     VALUES (?1, '{}', 't', 't')",
                    rusqlite::params![conv.to_string()],
                )
                .unwrap();
            guard
                .execute(
                    "INSERT INTO messages VALUES (?1, 0, '{}')",
                    rusqlite::params![conv.to_string()],
                )
                .unwrap();
            guard
                .execute(
                    "INSERT INTO recall_archive VALUES (?1, '[]', 't', 't')",
                    rusqlite::params![conv.to_string()],
                )
                .unwrap();
        }

        ConversationStore::new(Arc::clone(&conn))
            .delete(conv)
            .await
            .unwrap();

        let guard = conn.lock().unwrap();
        assert_eq!(count(&guard, "SELECT COUNT(*) FROM messages"), 0);
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM recall_archive"),
            0,
            "the archive must go with the conversation, without the caller \
             having to remember to purge it"
        );
    }

    #[tokio::test]
    async fn deleting_a_job_takes_its_run_history_with_it() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        Store::run_migrations(&conn).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        {
            let guard = conn.lock().unwrap();
            guard
                .execute_batch(
                    "INSERT INTO scheduled_jobs (id, schedule, task, next_run_at, created_at)
                         VALUES ('job', '0 9 * * *', 'task', '2099-01-01T00:00:00+00:00', 't');
                     INSERT INTO job_runs (id, job_id, status, started_at, finished_at)
                         VALUES ('run', 'job', 'ok', 't', 't');",
                )
                .unwrap();
        }

        JobStore::new(Arc::clone(&conn))
            .delete_job("job")
            .await
            .unwrap();

        let guard = conn.lock().unwrap();
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM job_runs"),
            0,
            "run history must not outlive the job it belongs to"
        );
    }

    /// A panic while holding the connection lock used to end the process's
    /// ability to touch the database at all: the mutex stayed poisoned and
    /// every later call panicked with it. A recoverable fault must not
    /// become a permanent outage.
    #[tokio::test]
    async fn a_poisoned_connection_lock_does_not_end_the_process() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        Store::run_migrations(&conn).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        // Poison it the only way it can be poisoned: panic while holding it.
        let poisoner = Arc::clone(&conn);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("simulated panic while holding the connection");
        })
        .join();
        assert!(conn.is_poisoned(), "the test must actually poison the lock");

        let count: i64 = with_conn(&conn, |conn| {
            conn.query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
                .map_err(|e| Error::Storage(e.to_string()))
        })
        .await
        .expect("the store must still serve queries after a poisoned lock");
        assert_eq!(count, 0);
    }
}
