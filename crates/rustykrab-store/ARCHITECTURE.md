# rustykrab-store — Persistence

15 files, ~8,300 lines, 116 tests. Depends only on `rustykrab-core`.

Full table-by-table analysis lives in
[`docs/architecture/01-data-model.md`](../../docs/architecture/01-data-model.md).
This file covers the crate's *code* structure.

## Responsibility

Own `store.db`: conversations and messages, secrets and the credential guard,
scheduled jobs and their run history, paired devices, delegated tasks, outcome
records, channel bindings, and the recall archive.

## Shape

`Store` is a handle-factory, not a god object. It holds the connection, the
master key (in `Zeroizing`), a credential backend, and an optional notifier, and
hands out twelve narrow repository types:

```rust
store.conversations()      -> ConversationStore
store.secrets()            -> SecretStore
store.guarded_secrets()    -> GuardedSecrets
store.credential_requests()-> CredentialRequestStore
store.devices()            -> DeviceStore
store.tasks()              -> TaskStore
store.jobs()               -> JobStore
store.outcomes()           -> OutcomeStore
store.channel_bindings()   -> ChannelBindingStore
store.recall_archive()     -> RecallArchiveStore
store.pending_links()      -> PendingLinks          // in-memory
```

Each is a `Clone` newtype over `Arc<Mutex<Connection>>`. Callers depend on the
one repository they need, not on `Store`. This is the right pattern and it is
applied consistently.

## Concurrency model

Everything funnels through:

```rust
pub(crate) async fn with_conn<T, F>(conn: &Arc<Mutex<Connection>>, f: F) -> Result<T, Error>
```

which does `run_blocking(move || f(&conn.lock().unwrap()))`. One connection, one
mutex, all access serialised, dispatched to the blocking pool. WAL is enabled
but its concurrent-reader benefit is unreachable.

For a single-user daemon that is a reasonable simplification — it should be
stated as one.

Poisoning is handled. Every lock on the connection recovers rather than
propagating the panic, which took two passes: `with_conn` first, then the
eight sites that bypass it (`SecretStore` locks directly inside its own
`run_blocking` closures; `RecallArchiveStore` locks directly because
`RecallPersistence` is synchronous). Test fixtures deliberately keep
`.unwrap()`.

## Migrations

`Store::run_migrations` runs one `execute_batch` of `CREATE TABLE IF NOT
EXISTS` + `CREATE INDEX IF NOT EXISTS`, then four blocks of the pattern:

```rust
let existing = PRAGMA table_info(t);
for (column, ddl) in [...] { if !existing.contains(column) { conn.execute(ddl) } }
```

then `conversation::migrate_legacy_blobs`, which explodes pre-normalisation
whole-conversation blobs into `conversations` + `messages` rows, in one
transaction, keyed on `updated_at IS NULL` so a completed sweep is a no-op.

**Good:** idempotent, additive, restart-safe, and the comments explain why old
rows are left with NULL rather than back-filled with "now" or with the current
version — back-filling would assert an attribution that was never observed.
That reasoning is applied consistently across four separate columns.

Since the first pass, migrations also **adopt foreign keys** onto existing
tables via the rebuild SQLite requires — create the new shape, copy the rows
that satisfy it, drop, rename, restore indexes — guarded on
`PRAGMA foreign_key_list` so it runs once, with foreign keys off for the
duration and `foreign_key_check` before the commit. And
`migrate_legacy_chat_maps` folds the two per-channel tables into
`channel_bindings` and drops them.

**Weak:** still no schema-version table. Answering "what shape is this
database" requires reading the migration function and mentally replaying every
guard — and that function has grown. Legacy-schema tests now exist for jobs,
conversations, chat maps and the FK adoption, which is most of the surface.

## Notable subsystems

**Credential guard** (`credential_request.rs` 1,338 lines, `guarded.rs`,
`secret.rs`, `secret_versions`, `secret_audit`). The agent cannot silently
overwrite an existing credential — it files a request carrying the target's
version at filing time, and a stale approval is rejected. Superseded values are
retained encrypted so a mistaken delete is recoverable. Every operation appends
to an audit log. One-time fulfil links are stored only as hashes. This is the
most carefully built part of the crate and it shows.

**`credential_backend.rs`** is a proper inversion: `CredentialBackend` with
keychain / encrypted-file / env implementations, so live credential values never
have to live in the database.

**`RequestNotifier`** exists so the store can tell someone a request was filed
without depending on the push/APNs layer. Correct direction.

## Observations

- The two per-channel map stores are gone, replaced by `ChannelBindingStore`
  over one table with a cascading foreign key. Signal gains persistence it
  never had.
- Deletion is the database's job now: messages, recall archive and channel
  bindings cascade from `conversations`; run history cascades from
  `scheduled_jobs`. Callers no longer maintain a mental list.
- The remaining hand-written DDL in test fixtures is the deliberate kind —
  simulating legacy schemas to exercise the migration path.
- Three direct `env::var` reads (`RUSTYKRAB_MASTER_KEY`,
  `RUSTYKRAB_DISABLE_KEYCHAIN`, `RUSTYKRAB_APNS_ENVIRONMENT`) that would be
  better passed in by the composition root.
