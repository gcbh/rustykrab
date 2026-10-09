# Consolidating legacy instances into v6

Use one v6 daemon as the work manager, scheduler and channel owner. Old
instances must be drained and stopped before the final snapshot. Pointing
several old/new daemons at one SQLite database would leave several schedulers
and Telegram pollers active; schema compatibility alone does not transfer
execution ownership.

## What transfers

The stage retains all destination v6 state and merges source conversations,
messages, recall archives, channel bindings/intake journals, schedules and run
history, encrypted credentials and their audit/version/request history,
registered devices, project revisions, work graphs, closed work and held
approvals, evidence, spend, questions, proposals and evaluation history.
Memory content, chunks/embeddings, facts and links retain their IDs and validity;
agent ownership changes to v6 with original agent identity in metadata. Skills
and wiki assets merge only when paths do not conflict.

The source worker registrations, routing defaults, live leases and pairing
codes remain in snapshots. The v6 pool, native CLI profiles and registered
repositories stay authoritative. Filesystem checkout/worktree artifacts are
not moved or deleted; old evidence pointers continue to name their original
paths. Source soul/config/logs/models are not execution policy imports. Keep
all original data and service configuration for rollback.

Imported schedules start disabled. Their former enabled intent appears in the
receipt. Historical pending notices are retired without claiming delivery,
and never replayed. Consent holds remain holds. Conflicting IDs, unknown tables,
active source work/payment authorization, leases and changed replays refuse
instead of replacing destination state.

## Stage

Prepare an operator-owned private JSON plan with directory paths and environment
variable names. Keys must already exist; no source key is generated. An omitted
source key name reads the existing OS master key. Do not put keys in arguments,
receipts or source control.

```json
{
  "offline": true,
  "destination": "/private/path/v6-data",
  "destination_key_env": "RK_DESTINATION_KEY",
  "sources": [
    {"name": "main", "data_dir": "/private/path/main-data", "master_key_env": null},
    {"name": "builder", "data_dir": "/private/path/builder-data", "master_key_env": "RK_BUILDER_KEY"}
  ]
}
```

Before staging, disable overseer auto-recovery for the old owners, drain all
runs, stop their launchd jobs and the destination, and verify the processes
exited. Old schedulers may predate controller locks, so the offline acknowledgment
is an operator precondition as well as the lock check. Preserve launchd/plist,
private environment, Serve routes and binary paths in a protected rollback copy.

```sh
rustykrab-cli consolidate stage --plan /private/plan.json --output /private/new-stage
```

The output directory must be new and outside every source/destination. It is
0700, its files 0600. `snapshots/` retains the original committed databases;
`normalized/` contains migrated source copies. The top-level database/memory
files are the merged candidate. A complete stage has `receipt.json` containing
counts, fingerprints and raw-snapshot hashes. An error leaves a partial stage
for inspection; it is never installed automatically. Compare all expected
counts, FK integrity, enabled intents, holds and representative API-readable
conversations/memory before cutover.

## Handoff and verification

Install the signed verified v6 build and swap the merged db/memory/assets into
the stopped destination, retaining its existing runtime configuration and
private native CLI profiles. Retain the original destination db/memory for
rollback. Connect the existing Telegram token/allowlist and integration
credentials through protected configuration and a working secure credential
backend. Preserve the destination authentication token. Start only v6.

For personal jobs, opt in to `RUSTYKRAB_INTEGRATION_WORKER=1` with
`RUSTYKRAB_PROVIDER=claude-cli` and the existing Max profile. The integration
worker reads Gmail/calendar and public HTTPS pages, and may write only
`Daily Briefings/Briefing_YYYY-MM-DD.md` in Obsidian. It has no shell, filesystem
write, credential-read/setup or message tool. Native Claude and Codex workers
continue coding in their registered isolated worktrees; no Ollama worker is
introduced. The dashboard shows this adapter in the ordinary worker pool.

Give each imported job explicit execution requirements through `CronExecution`,
including integration tool names and `obsidian:daily-briefings` when it saves a
briefing. Use a fresh execution conversation while retaining old conversation
and job-run history. Replace missing script dependencies with the user's recorded
requirements. The scheduled result is the complete report and the scheduler
delivers it once; recipes must not call message themselves.

Verify a real firing, successful host tool observations, the saved note when
required, exactly one new job-run record and successful delivery to the
existing target. Check persisted tool_observation evidence on the work item;
a run transcript or model summary alone does not establish host attestation.
Failed or unavailable integrations remain incomplete and
visible; do not label a model claim as proof. Verify Telegram intake continues
its binding and files work through the v6 manager. Then enable the original
recurrences, disable the retired launchd owners and remap their private Serve
route to v6, preserving any existing v6 route and both allowed HTTPS origins.

Rollback requires stopping v6 first and restoring the frozen original v6
state/config/binary and old route/owners. Never resume old and new schedulers
or Telegram pollers simultaneously. Keep rollback material protected, outside
source control and agent briefs.
