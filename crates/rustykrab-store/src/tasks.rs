//! Durable queue for tasks delegated by a peer RustyKrab instance.
//!
//! A delegating peer no longer blocks on the agent turn it asked for
//! (`POST /api/tasks` returns a handle immediately); the work is queued
//! here and drained by a single worker. Persistence is what makes the
//! handle meaningful: the caller can poll across its own restarts, and a
//! task interrupted by a daemon restart goes back to the queue and runs
//! again (a bounded number of times) rather than being silently lost or
//! failed by the restart alone.
//!
//! A task is free text (the `nodes` tool's `message`) or structured (the
//! control layer's peer worker, plan `control-layer-and-worker-fleet.md`
//! sections 5 and 13): a typed brief, the work item it runs, the tools
//! the node must activate before its first model call, the submitting
//! controller's run id (so a resubmission after the controller restarts
//! finds the task it already has), and a typed `ResultReport` as its
//! result (`result_json`).
//!
//! One worker, not a pool. Delegation nodes run local models where the
//! KV cache is pinned to a single slot (`OLLAMA_NUM_PARALLEL=1`), so
//! interleaving two conversations evicts both prefixes and each turn
//! re-pays full prompt evaluation. Serialising is faster than sharing.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rustykrab_core::work::ResultReport;
use rustykrab_core::Error;

use crate::with_conn;

/// What `required_tools` reads as when its column cannot be parsed: a name
/// no tool has, so the node refuses the task with a typed reason instead of
/// running it with nothing activated (the conservative case).
pub const UNREADABLE_TOOLS: &str = "<unreadable required_tools>";

/// Lifecycle of a delegated task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Accepted and waiting for the worker.
    Queued,
    /// The worker is running the agent turn now.
    Running,
    /// Finished; `result` holds the agent's reply.
    Done,
    /// Finished; `error` explains why there is no result.
    Failed,
    /// Cancelled by the caller, before or during the run.
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    fn parse(raw: &str) -> TaskStatus {
        match raw {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "done" => TaskStatus::Done,
            "cancelled" => TaskStatus::Cancelled,
            // Anything unrecognised is treated as terminal-failed rather
            // than re-queued: a row we cannot interpret must never become
            // work the agent runs.
            _ => TaskStatus::Failed,
        }
    }

    /// Whether no further state transition is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

/// A task submitted by a peer for this node to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedTask {
    pub id: String,
    /// The instruction to run. Self-contained by contract: the peer's
    /// conversation is not shared with this node.
    pub message: String,
    /// Conversation the task runs in. Supplied by the caller to continue
    /// an earlier thread (and reuse its warm prompt prefix), or assigned
    /// by the worker when it opens a fresh one.
    pub conversation_id: Option<String>,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Who submitted it, from the gateway's authenticated principal.
    /// Recorded so a delegated turn is attributable to the peer that
    /// asked for it rather than looking like local user input.
    pub principal: Option<String>,
    /// Remaining delegation hops. A node may only hand work onward while
    /// this is above zero, which is what stops A -> B -> A recursion.
    pub hop_budget: i64,
    /// Tools the submitting peer asked to limit this task to. Advisory in
    /// one direction only: the node intersects it with its own policy, so
    /// a task can ask for less than the node allows and never more.
    /// `None` means the peer expressed no preference.
    pub allowed_tools: Option<Vec<String>>,
    /// The caller's trace id, so one delegation correlates across both
    /// machines' logs.
    pub trace_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// The work item this task runs, on the submitting controller. Recorded,
    /// never enforced: the item lives in another machine's store.
    pub work_item_id: Option<String>,
    /// Tools the node activates before the run's first model call, inside
    /// its own ceiling. An unreadable column reads as [`UNREADABLE_TOOLS`].
    pub required_tools: Vec<String>,
    /// The typed brief of a structured task, as submitted (JSON). `None`:
    /// a free-text task, run from `message`.
    pub brief: Option<String>,
    /// The submitting controller's run id: at most one task per run, so a
    /// resubmission finds this one.
    pub run_id: Option<String>,
    /// The typed result of a structured task (`result_json`): the worker's
    /// report, or on a failed run a report whose `error` says why.
    pub report: Option<ResultReport>,
    /// What the run spent, as the node's worker counted it (JSON).
    pub usage: Option<serde_json::Value>,
    /// Times the worker has claimed the task: more than one means a restart
    /// interrupted an earlier run and the task went back to the queue.
    pub attempts: u32,
}

impl DelegatedTask {
    /// Whether the task carries a typed brief.
    pub fn is_structured(&self) -> bool {
        self.brief.is_some()
    }

    fn from_row(row: &Row) -> rusqlite::Result<DelegatedTask> {
        let parse_time = |raw: Option<String>| -> Option<DateTime<Utc>> {
            raw.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&Utc))
        };
        let created: String = row.get("created_at")?;
        let id: String = row.get("id")?;
        let report = row
            .get::<_, Option<String>>("result_json")?
            .and_then(|raw| match serde_json::from_str(&raw) {
                Ok(report) => Some(report),
                Err(e) => {
                    tracing::warn!(task_id = %id, error = %e, "unreadable result_json; read as none");
                    None
                }
            });
        Ok(DelegatedTask {
            message: row.get("message")?,
            conversation_id: row.get("conversation_id")?,
            status: TaskStatus::parse(&row.get::<_, String>("status")?),
            result: row.get("result")?,
            error: row.get("error")?,
            principal: row.get("principal")?,
            hop_budget: row.get("hop_budget")?,
            // A row we cannot parse must not silently widen the ceiling,
            // so an unreadable list is treated as "allow nothing".
            allowed_tools: row
                .get::<_, Option<String>>("allowed_tools")?
                .map(|raw| serde_json::from_str(&raw).unwrap_or_default()),
            trace_id: row.get("trace_id")?,
            created_at: parse_time(Some(created)).unwrap_or_else(Utc::now),
            started_at: parse_time(row.get("started_at")?),
            finished_at: parse_time(row.get("finished_at")?),
            work_item_id: row.get("work_item_id")?,
            // Nor may it silently drop a tool the run was promised: an
            // unreadable list names a tool that does not exist, which the
            // node refuses with a typed reason.
            required_tools: row
                .get::<_, Option<String>>("required_tools")?
                .map(|raw| {
                    serde_json::from_str(&raw).unwrap_or_else(|_| vec![UNREADABLE_TOOLS.into()])
                })
                .unwrap_or_default(),
            brief: row.get("brief")?,
            run_id: row.get("run_id")?,
            report,
            usage: row
                .get::<_, Option<String>>("usage")?
                .and_then(|raw| serde_json::from_str(&raw).ok()),
            attempts: u32::try_from(row.get::<_, i64>("attempts")?).unwrap_or(0),
            id,
        })
    }
}

const COLUMNS: &str = "id, message, conversation_id, status, result, error, principal, \
                       hop_budget, allowed_tools, trace_id, created_at, started_at, \
                       finished_at, work_item_id, required_tools, brief, run_id, \
                       result_json, usage, attempts";

/// Add the Phase 5 columns to `delegated_tasks` (control-layer plan,
/// sections 5 and 13), for databases made before them. `work_item_id` is
/// older and added by `Store::run_migrations` itself. Every column is
/// nullable or defaulted, so a row written before them reads as a free-text
/// task claimed once.
pub(crate) fn migrate(conn: &rusqlite::Connection) -> Result<(), Error> {
    let storage = |e: rusqlite::Error| Error::Storage(e.to_string());
    let mut stmt = conn
        .prepare("PRAGMA table_info(delegated_tasks)")
        .map_err(storage)?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    drop(stmt);
    for (column, ddl) in [
        ("required_tools", "required_tools TEXT"),
        ("brief", "brief TEXT"),
        ("run_id", "run_id TEXT"),
        ("result_json", "result_json TEXT"),
        ("usage", "usage TEXT"),
        ("attempts", "attempts INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !existing.iter().any(|c| c == column) {
            conn.execute(&format!("ALTER TABLE delegated_tasks ADD COLUMN {ddl}"), [])
                .map_err(storage)?;
        }
    }
    // A resubmission looks its task up by run id.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_delegated_tasks_run
             ON delegated_tasks (run_id) WHERE run_id IS NOT NULL",
        [],
    )
    .map_err(storage)?;
    Ok(())
}

/// Everything a submission records. `Default` is a free-text task with no
/// hops, no tool limit and nothing typed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewTask {
    pub message: String,
    pub conversation_id: Option<String>,
    pub principal: Option<String>,
    pub hop_budget: i64,
    pub allowed_tools: Option<Vec<String>>,
    pub trace_id: Option<String>,
    pub work_item_id: Option<String>,
    pub required_tools: Vec<String>,
    /// The typed brief, as JSON.
    pub brief: Option<String>,
    pub run_id: Option<String>,
}

/// How many times a task may be claimed before a restart that interrupts
/// it fails it instead of queueing it again: a task that takes the node
/// down every time it runs must not do so forever.
pub const MAX_ATTEMPTS: u32 = 3;

/// What [`TaskStore::requeue_orphaned`] did with the tasks a previous
/// process left running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Orphans {
    /// Back in the queue, to run again.
    pub requeued: Vec<String>,
    /// Interrupted [`MAX_ATTEMPTS`] times: failed, with the reason.
    pub failed: Vec<String>,
}

/// Handle for delegated-task CRUD, backed by SQLite.
///
/// Like the other stores, every method runs its rusqlite work on the
/// blocking pool so async workers never park on disk I/O.
#[derive(Clone)]
pub struct TaskStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl TaskStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Queue a free-text task and return the handle the caller polls.
    pub async fn enqueue(
        &self,
        message: &str,
        conversation_id: Option<&str>,
        principal: Option<&str>,
        hop_budget: i64,
        allowed_tools: Option<Vec<String>>,
        trace_id: Option<&str>,
    ) -> Result<DelegatedTask, Error> {
        let (task, _) = self
            .submit(NewTask {
                message: message.to_string(),
                conversation_id: conversation_id.map(str::to_string),
                principal: principal.map(str::to_string),
                hop_budget,
                allowed_tools,
                trace_id: trace_id.map(str::to_string),
                ..NewTask::default()
            })
            .await?;
        Ok(task)
    }

    /// Queue a task, free text or structured. A submission naming a
    /// `run_id` that a task already has returns that task instead of a
    /// second one (`false`): a controller that restarted mid-run resubmits
    /// its brief and finds the task it already has, whatever its state.
    pub async fn submit(&self, new: NewTask) -> Result<(DelegatedTask, bool), Error> {
        let task = DelegatedTask {
            id: Uuid::new_v4().to_string(),
            message: new.message,
            conversation_id: new.conversation_id,
            status: TaskStatus::Queued,
            result: None,
            error: None,
            principal: new.principal,
            hop_budget: new.hop_budget.max(0),
            allowed_tools: new.allowed_tools,
            trace_id: new.trace_id,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            work_item_id: new.work_item_id,
            required_tools: new.required_tools,
            brief: new.brief,
            run_id: new.run_id,
            report: None,
            usage: None,
            attempts: 0,
        };

        let row = task.clone();
        with_conn(&self.conn, move |conn| {
            // Select-then-insert under the connection mutex `with_conn`
            // holds for the whole closure, so two submissions of one run
            // cannot both insert.
            if let Some(run) = row.run_id.as_deref() {
                if let Some(existing) = by_run(conn, run)? {
                    return Ok((existing, false));
                }
            }
            conn.execute(
                "INSERT INTO delegated_tasks (id, message, conversation_id, status, principal, \
                 hop_budget, allowed_tools, trace_id, created_at, work_item_id, required_tools, \
                 brief, run_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    row.id,
                    row.message,
                    row.conversation_id,
                    row.status.as_str(),
                    row.principal,
                    row.hop_budget,
                    row.allowed_tools
                        .as_ref()
                        .map(|t| serde_json::to_string(t).unwrap_or_default()),
                    row.trace_id,
                    row.created_at.to_rfc3339(),
                    row.work_item_id,
                    (!row.required_tools.is_empty())
                        .then(|| serde_json::to_string(&row.required_tools).unwrap_or_default()),
                    row.brief,
                    row.run_id,
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok((row, true))
        })
        .await
    }

    /// The task a controller's run submitted, if this node has it.
    pub async fn find_by_run(&self, run_id: &str) -> Result<Option<DelegatedTask>, Error> {
        let run_id = run_id.to_string();
        with_conn(&self.conn, move |conn| by_run(conn, &run_id)).await
    }

    pub async fn get(&self, id: &str) -> Result<Option<DelegatedTask>, Error> {
        let id = id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM delegated_tasks WHERE id = ?1"
                ))
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut rows = stmt
                .query_map(params![id], DelegatedTask::from_row)
                .map_err(|e| Error::Storage(e.to_string()))?;
            match rows.next() {
                Some(row) => Ok(Some(row.map_err(|e| Error::Storage(e.to_string()))?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Most recent tasks first.
    pub async fn list(&self, limit: u32) -> Result<Vec<DelegatedTask>, Error> {
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM delegated_tasks ORDER BY created_at DESC LIMIT ?1"
                ))
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![limit], DelegatedTask::from_row)
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::Storage(e.to_string()))?);
            }
            Ok(out)
        })
        .await
    }

    /// Claim the next queued task, flipping it to `running`.
    ///
    /// `prefer_conversation` biases selection toward a task continuing the
    /// conversation the worker just ran. That is thread affinity, and on a
    /// local model it is worth real time: continuing a warm conversation
    /// re-uses its evaluated prompt prefix, where switching threads pays
    /// full prefill again. FIFO breaks the tie, so a preferred thread can
    /// never starve older work indefinitely — only jump ahead of it while
    /// it has queued steps.
    pub async fn claim_next(
        &self,
        prefer_conversation: Option<&str>,
    ) -> Result<Option<DelegatedTask>, Error> {
        let preferred = prefer_conversation.map(str::to_string);
        let now = Utc::now().to_rfc3339();
        with_conn(&self.conn, move |conn| {
            // No explicit transaction: `with_conn` holds the connection
            // mutex for the whole closure, and a single worker claims, so
            // select-then-update cannot interleave. The `status = 'queued'`
            // guard on the UPDATE keeps that assumption enforced rather
            // than merely assumed.
            let id: Option<String> = conn
                .query_row(
                    "SELECT id FROM delegated_tasks WHERE status = 'queued' \
                     ORDER BY (conversation_id IS NOT NULL AND conversation_id = ?1) DESC, \
                     created_at ASC LIMIT 1",
                    params![preferred],
                    |row| row.get(0),
                )
                .ok();
            let Some(id) = id else {
                return Ok(None);
            };

            let claimed = conn
                .execute(
                    "UPDATE delegated_tasks SET status = 'running', started_at = ?2, \
                     attempts = attempts + 1 \
                     WHERE id = ?1 AND status = 'queued'",
                    params![id, now],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if claimed == 0 {
                return Ok(None);
            }

            let task = conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM delegated_tasks WHERE id = ?1"),
                    params![id],
                    DelegatedTask::from_row,
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(Some(task))
        })
        .await
    }

    /// Record the conversation the worker opened for a task, so a follow-up
    /// `send` can continue the same thread.
    pub async fn set_conversation(&self, id: &str, conversation_id: &str) -> Result<(), Error> {
        let (id, conversation_id) = (id.to_string(), conversation_id.to_string());
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE delegated_tasks SET conversation_id = ?2 WHERE id = ?1",
                params![id, conversation_id],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Move a task to a terminal state.
    ///
    /// Refuses to overwrite an existing terminal state, so a cancel that
    /// lands while the agent is mid-turn is not undone by the run's own
    /// completion a moment later.
    async fn settle(&self, id: &str, status: TaskStatus, outcome: Outcome) -> Result<(), Error> {
        let id = id.to_string();
        let now = Utc::now().to_rfc3339();
        let report = match &outcome.report {
            Some(r) => Some(serde_json::to_string(r).map_err(|e| Error::Storage(e.to_string()))?),
            None => None,
        };
        let usage = outcome.usage.as_ref().map(|u| u.to_string());
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE delegated_tasks SET status = ?2, result = ?3, error = ?4, \
                 finished_at = ?5, result_json = COALESCE(?6, result_json), \
                 usage = COALESCE(?7, usage) \
                 WHERE id = ?1 AND status IN ('queued', 'running')",
                params![
                    id,
                    status.as_str(),
                    outcome.result,
                    outcome.error,
                    now,
                    report,
                    usage
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    pub async fn finish(&self, id: &str, result: &str) -> Result<(), Error> {
        self.settle(
            id,
            TaskStatus::Done,
            Outcome {
                result: Some(result.to_string()),
                ..Outcome::default()
            },
        )
        .await
    }

    pub async fn fail(&self, id: &str, error: &str) -> Result<(), Error> {
        self.settle(
            id,
            TaskStatus::Failed,
            Outcome {
                error: Some(error.to_string()),
                ..Outcome::default()
            },
        )
        .await
    }

    /// A structured task's run reported: done, with its typed result (the
    /// summary also as `result`, for a caller that reads only text) and
    /// what it spent.
    pub async fn finish_report(
        &self,
        id: &str,
        report: &ResultReport,
        usage: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        self.settle(
            id,
            TaskStatus::Done,
            Outcome {
                result: Some(report.summary.clone()),
                error: None,
                report: Some(report.clone()),
                usage,
            },
        )
        .await
    }

    /// A structured task's run ended without a report: failed, with `error`
    /// (for a caller that reads only text) and, when the node could type the
    /// failure, a report whose `error` is that type.
    pub async fn fail_report(
        &self,
        id: &str,
        error: &str,
        report: Option<&ResultReport>,
        usage: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        self.settle(
            id,
            TaskStatus::Failed,
            Outcome {
                result: None,
                error: Some(error.to_string()),
                report: report.cloned(),
                usage,
            },
        )
        .await
    }

    /// Cancel a task, returning the status it held beforehand.
    ///
    /// `Running` in the return means the worker still has to be told to
    /// abort — the row is already terminal, but the agent loop is not.
    pub async fn cancel(&self, id: &str) -> Result<Option<TaskStatus>, Error> {
        let previous = match self.get(id).await? {
            Some(task) => task.status,
            None => return Ok(None),
        };
        if previous.is_terminal() {
            return Ok(Some(previous));
        }
        self.settle(
            id,
            TaskStatus::Cancelled,
            Outcome {
                error: Some("cancelled by the delegating peer".to_string()),
                ..Outcome::default()
            },
        )
        .await?;
        Ok(Some(previous))
    }

    /// Put every task a previous process left `running` back in the queue.
    ///
    /// Called once at startup. The agent loop that owned them died with the
    /// process; nothing is failed merely because the node restarted (plan
    /// section 13). The task runs again from its brief, keeping its place
    /// in the queue, and the peer that polls it sees it `queued`, then
    /// `running` with `attempts` above one. A task already claimed
    /// [`MAX_ATTEMPTS`] times is failed instead, with the reason, so one
    /// that takes the node down each time it runs stops doing so.
    pub async fn requeue_orphaned(&self) -> Result<Orphans, Error> {
        let now = Utc::now().to_rfc3339();
        with_conn(&self.conn, move |conn| {
            let storage = |e: rusqlite::Error| Error::Storage(e.to_string());
            let tx = conn.unchecked_transaction().map_err(storage)?;
            let ids = |sql: &str| -> Result<Vec<String>, Error> {
                let mut stmt = tx.prepare(sql).map_err(storage)?;
                let rows = stmt
                    .query_map(params![MAX_ATTEMPTS], |row| row.get(0))
                    .map_err(storage)?
                    .collect::<rusqlite::Result<Vec<String>>>()
                    .map_err(storage)?;
                Ok(rows)
            };
            let orphans = Orphans {
                requeued: ids(
                    "SELECT id FROM delegated_tasks WHERE status = 'running' AND attempts < ?1",
                )?,
                failed: ids(
                    "SELECT id FROM delegated_tasks WHERE status = 'running' AND attempts >= ?1",
                )?,
            };
            tx.execute(
                "UPDATE delegated_tasks SET status = 'failed', finished_at = ?2, \
                 error = 'interrupted: the node restarted mid-task ' || attempts || \
                 ' times; not run again' \
                 WHERE status = 'running' AND attempts >= ?1",
                params![MAX_ATTEMPTS, now],
            )
            .map_err(storage)?;
            tx.execute(
                "UPDATE delegated_tasks SET status = 'queued', started_at = NULL \
                 WHERE status = 'running' AND attempts < ?1",
                params![MAX_ATTEMPTS],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
            Ok(orphans)
        })
        .await
    }

    /// Delete terminal tasks older than `max_age`. Queued and running rows
    /// are never swept, however old — an unfinished task is not garbage.
    pub async fn sweep(&self, max_age: chrono::Duration) -> Result<usize, Error> {
        let cutoff = (Utc::now() - max_age).to_rfc3339();
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM delegated_tasks WHERE status NOT IN ('queued', 'running') \
                     AND finished_at IS NOT NULL AND finished_at < ?1",
                    params![cutoff],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(n)
        })
        .await
    }
}

/// What a terminal transition writes besides the status.
#[derive(Debug, Default)]
struct Outcome {
    result: Option<String>,
    error: Option<String>,
    report: Option<ResultReport>,
    usage: Option<serde_json::Value>,
}

fn by_run(conn: &rusqlite::Connection, run: &str) -> Result<Option<DelegatedTask>, Error> {
    conn.query_row(
        &format!(
            "SELECT {COLUMNS} FROM delegated_tasks WHERE run_id = ?1 \
             ORDER BY created_at ASC LIMIT 1"
        ),
        params![run],
        DelegatedTask::from_row,
    )
    .optional()
    .map_err(|e| Error::Storage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TaskStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::Store::run_migrations(&conn).unwrap();
        TaskStore::new(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn a_submitted_task_is_claimable_exactly_once() {
        let s = store();
        let task = s
            .enqueue("do it", None, Some("m1max"), 0, None, None)
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Queued);

        let claimed = s.claim_next(None).await.unwrap().expect("a queued task");
        assert_eq!(claimed.id, task.id);
        assert_eq!(claimed.status, TaskStatus::Running);
        assert!(claimed.started_at.is_some());

        // A second worker (or the same one looping) must not re-run it.
        assert!(
            s.claim_next(None).await.unwrap().is_none(),
            "a claimed task must not be handed out again"
        );
    }

    #[tokio::test]
    async fn claims_are_fifo_but_prefer_the_warm_conversation() {
        let s = store();
        let older = s
            .enqueue("first", Some("convo-a"), None, 0, None, None)
            .await
            .unwrap();
        let newer = s
            .enqueue("second", Some("convo-b"), None, 0, None, None)
            .await
            .unwrap();

        // Affinity: continuing convo-b reuses its evaluated prompt prefix,
        // which is worth more than strict arrival order on a local model.
        let claimed = s.claim_next(Some("convo-b")).await.unwrap().unwrap();
        assert_eq!(claimed.id, newer.id);

        // With no preference, the remaining work comes out oldest-first.
        let claimed = s.claim_next(None).await.unwrap().unwrap();
        assert_eq!(claimed.id, older.id);
    }

    #[tokio::test]
    async fn a_cancel_mid_run_survives_the_run_finishing() {
        let s = store();
        let task = s
            .enqueue("long job", None, None, 0, None, None)
            .await
            .unwrap();
        s.claim_next(None).await.unwrap();

        let previous = s.cancel(&task.id).await.unwrap();
        assert_eq!(
            previous,
            Some(TaskStatus::Running),
            "the caller needs to know it must also abort the agent loop"
        );

        // The aborted run's own completion path must not resurrect it as a
        // result nobody asked for.
        s.finish(&task.id, "here is your answer").await.unwrap();
        let after = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Cancelled);
        assert!(after.result.is_none(), "a cancelled task has no result");
    }

    #[tokio::test]
    async fn cancelling_a_queued_task_reports_that_nothing_is_running() {
        let s = store();
        let task = s
            .enqueue("not started", None, None, 0, None, None)
            .await
            .unwrap();
        assert_eq!(s.cancel(&task.id).await.unwrap(), Some(TaskStatus::Queued));
        // And the worker must never pick it up afterwards.
        assert!(s.claim_next(None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_an_unknown_task_is_not_an_error() {
        let s = store();
        assert_eq!(s.cancel("no-such-task").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_restart_returns_interrupted_tasks_to_the_queue() {
        let s = store();
        let running = s
            .enqueue("interrupted", None, None, 0, None, None)
            .await
            .unwrap();
        s.claim_next(None).await.unwrap();
        let queued = s
            .enqueue("not yet started", None, None, 0, None, None)
            .await
            .unwrap();

        let orphans = s.requeue_orphaned().await.unwrap();
        assert_eq!(orphans.requeued, vec![running.id.clone()]);
        assert!(orphans.failed.is_empty());

        // Nothing is failed by the restart alone: the task runs again, and
        // its attempt count says an earlier run was cut short.
        let back = s.get(&running.id).await.unwrap().unwrap();
        assert_eq!(back.status, TaskStatus::Queued);
        assert!(back.started_at.is_none() && back.error.is_none());
        let again = s.claim_next(None).await.unwrap().unwrap();
        assert_eq!(again.id, running.id, "it keeps its place in the queue");
        assert_eq!(again.attempts, 2);

        let queued = s.get(&queued.id).await.unwrap().unwrap();
        assert_eq!(queued.status, TaskStatus::Queued);
    }

    #[tokio::test]
    async fn a_task_interrupted_too_often_fails_with_the_reason() {
        let s = store();
        let task = s
            .enqueue("takes the node down", None, None, 0, None, None)
            .await
            .unwrap();
        for _ in 1..MAX_ATTEMPTS {
            s.claim_next(None).await.unwrap().unwrap();
            assert_eq!(s.requeue_orphaned().await.unwrap().requeued.len(), 1);
        }
        s.claim_next(None).await.unwrap().unwrap();
        let orphans = s.requeue_orphaned().await.unwrap();
        assert_eq!(orphans.failed, vec![task.id.clone()]);

        let failed = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
        let why = failed.error.unwrap();
        assert!(why.contains("restarted") && why.contains('3'), "{why}");
        assert!(s.claim_next(None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_run_is_submitted_once_and_found_again() {
        let s = store();
        let brief = r#"{"item":"41"}"#.to_string();
        let new = NewTask {
            message: "work item #41".into(),
            work_item_id: Some("41".into()),
            required_tools: vec!["caldav".into()],
            brief: Some(brief.clone()),
            run_id: Some("run-1".into()),
            ..NewTask::default()
        };
        let (first, created) = s.submit(new.clone()).await.unwrap();
        assert!(created);
        assert!(first.is_structured());
        let (again, created) = s.submit(new).await.unwrap();
        assert!(!created, "a resubmitted run is the task it already has");
        assert_eq!(again.id, first.id);

        let found = s.find_by_run("run-1").await.unwrap().unwrap();
        assert_eq!(found.id, first.id);
        assert_eq!(found.work_item_id.as_deref(), Some("41"));
        assert_eq!(found.required_tools, ["caldav"]);
        assert_eq!(found.brief.as_deref(), Some(brief.as_str()));
        assert!(s.find_by_run("run-2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_typed_result_round_trips_and_a_bad_column_reads_conservatively() {
        use rustykrab_core::work::ArtifactRef;
        let s = store();
        let (task, _) = s
            .submit(NewTask {
                message: "report".into(),
                brief: Some("{}".into()),
                run_id: Some("run-9".into()),
                required_tools: vec!["caldav".into()],
                ..NewTask::default()
            })
            .await
            .unwrap();
        s.claim_next(None).await.unwrap();
        let report = ResultReport {
            summary: "Checked the calendar.".into(),
            artifacts: vec![ArtifactRef {
                kind: "message".into(),
                value: "e2e".into(),
            }],
            ..ResultReport::default()
        };
        s.finish_report(&task.id, &report, Some(serde_json::json!({ "tokens": 12 })))
            .await
            .unwrap();
        let done = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.report, Some(report));
        assert_eq!(done.result.as_deref(), Some("Checked the calendar."));
        assert_eq!(done.usage.unwrap()["tokens"], 12);

        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE delegated_tasks SET required_tools = 'not json' WHERE id = ?1",
                params![task.id],
            )
            .unwrap();
        }
        let reread = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reread.required_tools, [UNREADABLE_TOOLS]);
    }

    #[tokio::test]
    async fn a_failed_run_keeps_its_typed_error() {
        use rustykrab_core::work::{ErrorClass, ErrorSubclass, WorkError};
        let s = store();
        let (task, _) = s
            .submit(NewTask {
                message: "fails".into(),
                brief: Some("{}".into()),
                ..NewTask::default()
            })
            .await
            .unwrap();
        s.claim_next(None).await.unwrap();
        let report = ResultReport {
            summary: "the run ended without a result".into(),
            error: Some(WorkError {
                class: ErrorClass::Budget,
                subclass: ErrorSubclass::Wall,
                fingerprint: "f".into(),
                detail: "wall budget spent".into(),
                observed_by: "node".into(),
                artifact_refs: Vec::new(),
            }),
            ..ResultReport::default()
        };
        s.fail_report(&task.id, "wall budget spent", Some(&report), None)
            .await
            .unwrap();
        let failed = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("wall budget spent"));
        assert_eq!(
            failed.report.unwrap().error.unwrap().subclass,
            ErrorSubclass::Wall
        );
    }

    #[tokio::test]
    async fn the_sweeper_keeps_unfinished_work_however_old() {
        let s = store();
        let queued = s
            .enqueue("still waiting", None, None, 0, None, None)
            .await
            .unwrap();
        let done = s
            .enqueue("finished", None, None, 0, None, None)
            .await
            .unwrap();
        s.claim_next(None).await.unwrap();
        s.finish(&done.id, "result").await.unwrap();

        // Nothing is old enough to sweep yet.
        assert_eq!(s.sweep(chrono::Duration::hours(48)).await.unwrap(), 0);

        // With a zero-length retention the finished task goes and the
        // queued one stays: an unfinished task is not garbage.
        assert_eq!(s.sweep(chrono::Duration::zero()).await.unwrap(), 1);
        assert!(s.get(&done.id).await.unwrap().is_none());
        assert!(s.get(&queued.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_worker_records_the_conversation_it_opened() {
        let s = store();
        let task = s
            .enqueue("fresh thread", None, None, 0, None, None)
            .await
            .unwrap();
        assert!(task.conversation_id.is_none());

        s.set_conversation(&task.id, "convo-1").await.unwrap();
        let reloaded = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reloaded.conversation_id.as_deref(), Some("convo-1"));
    }

    #[tokio::test]
    async fn a_hop_budget_round_trips_and_never_goes_negative() {
        let s = store();
        let task = s
            .enqueue("delegate onward", None, None, 2, None, None)
            .await
            .unwrap();
        assert_eq!(task.hop_budget, 2);

        // A caller cannot ask for a negative budget and have it read back
        // as anything other than "no further hops".
        let task = s
            .enqueue("no hops", None, None, -5, None, None)
            .await
            .unwrap();
        assert_eq!(task.hop_budget, 0);
    }

    #[tokio::test]
    async fn a_requested_tool_limit_round_trips() {
        let s = store();
        let task = s
            .enqueue(
                "read-only please",
                None,
                None,
                0,
                Some(vec!["read".to_string(), "web_fetch".to_string()]),
                None,
            )
            .await
            .unwrap();
        let reloaded = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.allowed_tools,
            Some(vec!["read".to_string(), "web_fetch".to_string()])
        );

        // No preference stays absent rather than becoming an empty list —
        // the two mean opposite things to the node's ceiling.
        let task = s
            .enqueue("anything goes", None, None, 0, None, None)
            .await
            .unwrap();
        let reloaded = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reloaded.allowed_tools, None);
    }

    #[tokio::test]
    async fn an_unreadable_tool_limit_allows_nothing() {
        // A corrupt column must not read back as "no preference", which
        // would silently widen the ceiling to whatever the node permits.
        let s = store();
        let task = s
            .enqueue(
                "limited",
                None,
                None,
                0,
                Some(vec!["read".to_string()]),
                None,
            )
            .await
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE delegated_tasks SET allowed_tools = 'not json' WHERE id = ?1",
                params![task.id],
            )
            .unwrap();
        }
        let reloaded = s.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reloaded.allowed_tools, Some(Vec::new()));
    }

    #[tokio::test]
    async fn tasks_list_newest_first() {
        let s = store();
        s.enqueue("first", None, None, 0, None, None).await.unwrap();
        let second = s
            .enqueue("second", None, None, 0, None, None)
            .await
            .unwrap();
        let listed = s.list(10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
    }
}
