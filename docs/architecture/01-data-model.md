# Data Model

Two SQLite databases, opened independently, never joined.

| File | Owner | Tables |
|---|---|---|
| `<data_dir>/db/store.db` | `rustykrab-store` | 20 tables + 14 indexes |
| `<data_dir>/memory.db` | `rustykrab-memory` | 4 tables + 1 FTS5 virtual table + 9 indexes |

DDL is idempotent (`CREATE TABLE IF NOT EXISTS`) inside
`Store::run_migrations` / `SqliteMemoryStorage::run_migrations`, followed by
additive `ALTER TABLE` blocks guarded by `PRAGMA table_info`. There is no
version table and no down-migration; the schema is whatever the union of every
guarded ALTER produces.

## `store.db`

### Conversation history

```
conversations(id PK, data JSON, title, created_at, updated_at)
messages(conversation_id, idx, data JSON, PK(conversation_id, idx))
recall_archive(conversation_id PK, archive JSON, created_at, updated_at)
```

`conversations.data` holds a `Conversation` with `messages` stripped —
`meta_only()` builds it as an explicit struct literal so adding a field to
`Conversation` forces a decision here. That is a good pattern.

`title`, `created_at`, `updated_at` are promoted out of the JSON into real
columns so `list_summaries` never parses JSON. `summary`, `detected_profile`,
`channel_source`, `channel_id`, `channel_thread_id` stay inside the blob.

**Assessment: mostly right, one soft spot.** Normalising messages into rows was
the correct call — it is what makes `save_turn` an append instead of a full
rewrite. Keeping low-cardinality metadata in a JSON blob is fine while nothing
queries it. But `channel_source`/`channel_id`/`channel_thread_id` *are*
queried — just in the opposite direction, via two dedicated tables (below).
They are one fact stored in two shapes.

### Channel binding

```
channel_bindings(channel, external_key,
                 conv_id REFERENCES conversations(id) ON DELETE CASCADE,
                 created_at,
                 PRIMARY KEY (channel, external_key))
   INDEX (conv_id)
```

**Resolved since the first pass.** This was two tables —
`telegram_chat_map(chat_id, thread_id)` and
`slack_chat_map(team_id, channel_id, thread_ts)` — expressing one relation
twice, with Signal having neither and therefore no persistence at all. The
addressing tuple is now flattened to `external_key` by
`ChannelAddress::external_key`, so the spelling is derived in one place
rather than at each call site.

The cascade closed a live bug: deleting a conversation through the API left
the binding pointing at it, and the channel answered *"Internal error — please
try again"* to that message and every message after it. Legacy databases fold
both tables into this one on first boot and drop them; bindings naming a
conversation that no longer exists are not carried over, because they cannot
be and because they are exactly the rows that were breaking the channel.

The in-memory half of that bug — a channel loop caching the resolved id and
never reconsulting the binding — is handled by `load_or_rebind`, which treats
a conversation that no longer loads as an instruction to start a new one.
Only `NotFound` is healed; a storage failure propagates, because minting a
replacement conversation because the disk is unhappy would discard history
that is still there.

**Remaining:** `conversations.data` still carries `channel_source`,
`channel_id` and `channel_thread_id` inside its JSON blob, so the same fact
lives in two shapes with nothing reconciling them. Low-severity — nothing
queries the blob copy — but it is the residue of the old design.

### Secrets and the credential guard

```
secrets(name PK, data BLOB, created_at, updated_at, version DEFAULT 1)
secret_versions(name, version, data BLOB, replaced_at, replaced_by,
                PK(name, version))
secret_audit(id AUTOINC, name, op, authority, request_id, at)
credential_requests(id PK, name, action, proposed_data BLOB, reason,
                    conversation_id, status, created_at, decided_at, decided_by,
                    target_version, service, fields, link_token_hash,
                    link_expires_at)
```

**Assessment: the best-designed part of the schema.** Current value, superseded
values, and the audit log are three separate concerns in three tables.
`target_version` on a request is optimistic concurrency done properly — a stale
approval cannot clobber a newer user edit. Only the hash of the one-time link
token is stored. `secret_audit` is append-only with a `(name, at DESC)` index.
The partial index `idx_credential_requests_pending ON (name) WHERE status =
'pending'` matches the only hot query exactly.

Two notes: `secret_versions` has no FK to `secrets` — deliberately, since a
deleted secret must stay recoverable, and that should be a comment on the
table. And `credential_requests.conversation_id` is an unenforced reference.

### Devices and pairing

```
devices(id PK, name, token_hash BLOB, created_at, last_seen_at, revoked_at,
        push_token)
pairing_codes(code_hash BLOB PK, expires_at, used_at)
```

**Assessment: correct.** Tokens stored only as SHA-256 hashes; revoked rows
retained so audit entries naming them still resolve; single-use codes with
expiry. Nothing to change.

### Scheduling

```
scheduled_jobs(id PK, schedule, task, channel, chat_id, thread_id, one_shot,
               enabled, next_run_at, last_run_at, created_at, conversation_id,
               created_version)
   INDEX (next_run_at) WHERE enabled = 1
job_runs(id PK, job_id, status, output, started_at, finished_at,
         rustykrab_version)
   INDEX (job_id, finished_at DESC)
```

**Assessment: right shape.** Job definition and job history are correctly
separated; the partial index on `next_run_at WHERE enabled = 1` is exactly the
poller's query. `job_runs.job_id` now cascades from `scheduled_jobs`, including
for existing databases through FK adoption, so deleting a job cannot orphan
its run history. `created_version` / `rustykrab_version` stamping is a useful
way to attribute behaviour to a build.

Recurring jobs are deduplicated on `(task, channel, chat_id, thread_id)` at
insert, under the same lock as the write. The tuple deliberately excludes
`schedule`: a second job running the same task on a *different* schedule is
exactly what a failed replace leaves behind, and it is indistinguishable from
intent afterwards. `JobStore::create_job` takes an `allow_duplicate` escape
hatch for the case that is genuinely two jobs — the same task at 8:00 and
17:30 cannot be one expression when the minute fields differ. `delete_job`
answers `NotFound` rather than `Ok(false)`, so "deleted it" and "there was
nothing to delete" are not the same successful call.

The remaining duplication is `(channel, chat_id, thread_id)` on
`scheduled_jobs`: it repeats addressing information that can also live in
`channel_bindings`. That is intentional today because a scheduled job retains
its own delivery target after its originating conversation is deleted.

### Delegated tasks

```
delegated_tasks(id PK, message, conversation_id, status, result, error,
                principal, hop_budget, allowed_tools, trace_id,
                created_at, started_at, finished_at)
   INDEX (status, created_at)
```

**Assessment: correct.** The worker's only hot query is "oldest queued", and
the index is `(status, created_at)`. `allowed_tools` as a serialised list is
acceptable — it is a policy snapshot, not a queryable relation.

### Outcome instrumentation

```
outcome_records(id PK, conversation_id, session_id, recorded_at,
                verdict CHECK IN (success|failure|ambiguous),
                signal  CHECK IN (verifiable|explicit|implicit|judge),
                confidence, detail, tool_calls, tool_failures, iterations,
                compactions, rustykrab_version)
   INDEX (recorded_at DESC), INDEX (conversation_id, recorded_at DESC)
outcome_attributions(record_id REFERENCES outcome_records(id) ON DELETE CASCADE,
                     kind CHECK IN (skill|memory|tool), target_id,
                     PK(record_id, kind, target_id))
   INDEX (kind, target_id)
```

Analysis passes are kept alongside them:

```
dream_reports(id PK, generated_at, readiness, total_records, summary,
              report)                       -- full AnalysisReport as JSON
   INDEX (generated_at DESC)
```

Two denormalised columns (`readiness`, `total_records`) so "has this ever been
ready?" is a query rather than a JSON scan; the rest is stored as JSON because
the shape of an analysis will change and a schema per revision is not worth the
migrations. Capped at 2000 rows, pruned on insert.

Without this table the only record that the outer loop ran at all was a log
line, which made the Phase 1 gate — "reports show real, actionable patterns" —
unanswerable by anything but a human with `grep`.

**Assessment: the most sophisticated modelling in the codebase, and correctly
normalised.** Write-once event rows; per-artifact tallies derived by
aggregation, so the scoring function can change without losing the evidence.
This is the one place with a real declared foreign key, a real cascade, and
real `GROUP BY` joins (`outcomes.rs:143-207`). `CHECK` constraints enforce the
enums at the database rather than trusting the writer.

The one flaw is not in the table but in where the referent lives: for
`kind = 'memory'`, `target_id` is a UUID in **`memory.db`**. It cannot have a
foreign key, cannot be joined, and deleting a memory silently orphans every
attribution naming it. `rustykrab-dream` analyses these tallies by id and would
report a finding against a memory that no longer exists.

### Conversational project planning

```
projects(id PK, create_request_id UNIQUE, repository_id,
         canonical_conversation_id, title, status, judgment_policy,
         current_revision, create_request,
         data, created_at, updated_at)
   FK (id, current_revision) REFERENCES project_revisions(project_id, id)
project_revisions(id PK,
                  project_id REFERENCES projects(id) ON DELETE CASCADE,
                  parent_revision, sequence,
                  request_id, request_data, author, conversation_id,
                  source_message_id, summary, project_data, data, created_at,
                  UNIQUE(project_id, sequence),
                  UNIQUE(project_id, request_id), UNIQUE(project_id, id))
   FK (project_id, parent_revision) REFERENCES project_revisions(project_id, id)
   INDEX (project_id, sequence)
plan_nodes(revision_id, project_id REFERENCES projects(id) ON DELETE CASCADE,
           id, kind, data, PK(revision_id, id))
   FK (project_id, revision_id)
      REFERENCES project_revisions(project_id, id) ON DELETE CASCADE
   INDEX (project_id, id)
plan_edges(revision_id, project_id REFERENCES projects(id) ON DELETE CASCADE,
           id, from_node, relation, to_node, data, PK(revision_id, id))
   FK (project_id, revision_id)
      REFERENCES project_revisions(project_id, id) ON DELETE CASCADE
   FK (revision_id, from_node/to_node)
      REFERENCES plan_nodes(revision_id, id) ON DELETE CASCADE
   INDEX (project_id, from_node, to_node)
```

**Assessment: sound event history with deliberately redundant read indexes.**
`project_revisions.data` is the immutable reconstruction source. The project
row is the current pointer; `plan_nodes` and `plan_edges` materialize each
revision for indexed inspection and are inserted atomically with it. Request
ids make create and apply safe to replay, and the store compares the complete
serialized command before treating a duplicate as success. Conversation and
message identifiers are provenance references rather than ownership, so they
are intentionally not foreign keys and can survive conversation deletion.

Composite foreign keys make project identity a database invariant rather than
an assumption about `ProjectStore`: current and parent revisions must belong to
the named project, materialized rows must belong to their revision, and edge
endpoints must be nodes in that same revision. Direct-SQL negative tests attempt
each cross-project write and require SQLite to reject it; `foreign_key_check`
must remain empty afterward.

## `memory.db`

```
memories(id PK, agent_id, content, content_hash,
         scope CHECK IN (session|user|agent|global), session_id, user_id,
         lifecycle_stage CHECK IN (working|episodic|semantic|archival|tombstone),
         importance, importance_source, decay_rate, confidence,
         access_count, last_accessed_at, last_relevant_at, created_at,
         parent_memory_ids JSON, consolidation_generation, proof_count,
         occurred_start, occurred_end,
         is_valid, invalidated_by, invalidated_at, tags JSON, metadata JSON)
chunks(id PK, memory_id REFERENCES memories(id), chunk_index, content,
       embedding BLOB, embedding_model_version, created_at)
extracted_facts(id PK, source_memory_id REFERENCES memories(id), fact_type,
                subject, predicate, object, confidence, valid_from, valid_to,
                extraction_method, created_at)
memory_links(source_id, target_id, link_type, weight, created_at,
             PK(source_id, target_id, link_type))
memories_fts USING fts5(memory_id UNINDEXED, agent_id UNINDEXED, content)
```

**Assessment: well normalised, with two structural caveats.**

The decomposition is right: a memory, its embedded chunks, the triples
extracted from it, and the graph edges between memories are four genuinely
different things. `memory_links` as `(source, target, type)` with a weight is a
textbook edge table. The six partial indexes on `memories` all have
`WHERE is_valid = 1`, matching the soft-delete access pattern. `CHECK`
constraints on `scope` and `lifecycle_stage` are enforced in the database.

Caveat 1 — **`memory_links` has no foreign keys** while `chunks` and
`extracted_facts` do. Nothing stops an edge naming a deleted memory. Since
deletion is soft (`is_valid = 0`) the practical exposure is small, but the
asymmetry looks accidental rather than reasoned.

Caveat 2 — **semantic search is a full linear scan.**
`get_all_chunk_embeddings` (storage.rs:1290) pulls every non-null embedding for
the agent into process memory via a proper join, then cosine-scores in Rust.
It is cached per agent and invalidated on write, and the join and filter are
correct, but there is no ANN index. This is the right trade at a few thousand
memories and the wrong one at a few hundred thousand; the design should say
which regime it targets.

Caveat 3 — **`session_id` means two different things.**
The gateway's auto-persist path writes turns with `session_id = conversation.id`
(orchestrate.rs:229). The `memory_save` tool writes with
`HybridMemoryBackend::session_id`, which the CLI sets to a fresh UUID at process
boot (main.rs:725). The read path — `MemoryAdapter::search` — parses the
argument as a *conversation* id. So facts the agent explicitly saves land with a
session id that no session-scoped search will ever match, and the shutdown
`finalize_session(agent_id, session_id)` promotes the boot session rather than
any conversation. One column, two meanings.

## Join analysis: enforced, and deliberately not

`store.db` declares fourteen foreign keys, up from one. The ones that are
ownership cascade; the ones that record provenance are unenforced *on
purpose*, and the DDL now says which is which.

| Relationship | Enforced? | Note |
|---|---|---|
| `messages.conversation_id` | **CASCADE** | |
| `recall_archive.conversation_id` | **CASCADE** | |
| `channel_bindings.conv_id` | **CASCADE** | closed the live bug above |
| `job_runs.job_id` | **CASCADE** | `delete_job` used to orphan run history forever |
| `outcome_attributions.record_id` | **CASCADE** | was the only FK before |
| `project_revisions.project_id` | **CASCADE** | project owns immutable revision history |
| `(projects.id, current_revision)` | **Yes, composite** | current revision must belong to the project |
| `(project_revisions.project_id, parent_revision)` | **Yes, composite** | parent must belong to the same project |
| `(plan_nodes/plan_edges.project_id, revision_id)` | **CASCADE, composite** | revision owns materialized rows and must belong to the same project |
| `plan_edges.(revision_id, from_node/to_node)` | **CASCADE, composite** | endpoints must exist in the same revision |
| `chunks.memory_id`, `extracted_facts.source_memory_id` | **Yes** | in `memory.db` |
| `scheduled_jobs.conversation_id` | No, deliberate | a cron job keeps its own delivery channel and should keep firing |
| `credential_requests.conversation_id` | No, deliberate | audit-relevant after the conversation is gone |
| `delegated_tasks.conversation_id` | No, deliberate | the row records where work came from |
| `outcome_records.conversation_id` | No, deliberate | evidence outlives the conversation |
| `outcome_attributions.target_id` (memory) | **Impossible** | other database |
| `memory_links.source_id/target_id` | No | asymmetric with `chunks`; looks accidental |

`PRAGMA foreign_keys = ON` now earns its keep. Cleanup moved out of the HTTP
handler: `delete_conversation` no longer has to know what a conversation
owns, and its remaining work is the recall cache and todo list — process
state the database cannot reach, which is genuinely the handler's job.

Existing databases adopt the constraints through the rebuild SQLite requires
(`adopt_foreign_key`): create the new shape, copy the rows that satisfy it,
drop, rename, restore indexes — guarded on `PRAGMA foreign_key_list` so it
runs once, with foreign keys off for the duration so the drop does not
cascade into the copy, and `PRAGMA foreign_key_check` before the commit.

**Remaining:** `memory_links` still has no foreign keys while its sibling
tables do. Soft deletion limits the exposure, but the asymmetry is unexplained.

## Should the two databases be one?

Yes, on the current evidence. The split buys independent file lifecycle (you
can delete `memory.db` without touching credentials) and independent
connection contention. It costs:

- no referential integrity for memory attributions,
- no ability to answer "which memories were in play for the conversations that
  failed last week" in one query — the join has to be done in Rust,
- two migration mechanisms, two `run_migrations`, two conventions.

SQLite `ATTACH` would restore cross-database joins without merging the files,
which is the cheap middle path if the file separation is genuinely wanted.

## Concurrency

Every `store.db` access goes through
`with_conn(&Arc<Mutex<rusqlite::Connection>>, f)` — **one connection behind one
std mutex**, so all reads and writes across all 12 repository handles serialise.
WAL mode is enabled but its concurrent-reader benefit is unreachable. On a
single-user daemon this is defensible and simple, and should be stated as a
deliberate choice; a small connection pool (writer + N readers) would remove the
ceiling if it ever matters.

**Resolved since the first pass.** Every lock on the connection now recovers
from poisoning rather than propagating the panic. That took two passes:
`with_conn` first, on the reasoning that all access funnels through it, and
then the eight sites that bypass it — `SecretStore` locks directly inside its
own `run_blocking` closures, and `RecallArchiveStore` locks directly because
`RecallPersistence` is a synchronous trait. Test fixtures deliberately keep
`.unwrap()`: a test that tolerates a poisoned lock is worth less than one
that fails on it.

## Smaller notes

- The hand-written DDL remaining in test fixtures is now the deliberate kind:
  `jobs.rs`, `conversation.rs` and `channel_binding.rs` simulate *legacy*
  schemas to exercise the migration path, which is the right technique.
- No schema version table. The guarded-ALTER approach is idempotent and works,
  but "what shape is this database" is only answerable by reading 250 lines of
  Rust and mentally replaying every guard.
