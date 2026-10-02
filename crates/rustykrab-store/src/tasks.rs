//! Durable queue for tasks delegated by a peer RustyKrab instance.
//!
//! A delegating peer no longer blocks on the agent turn it asked for
//! (`POST /api/tasks` returns a handle immediately); the work is queued
//! here and drained by a single worker. Persistence is what makes the
//! handle meaningful — the caller can poll across its own restarts, and
//! a task interrupted by a daemon restart is reported as failed rather
//! than silently lost.
//!
//! One worker, not a pool. Delegation nodes run local models where the
//! KV cache is pinned to a single slot (`OLLAMA_NUM_PARALLEL=1`), so
//! interleaving two conversations evicts both prefixes and each turn
//! re-pays full prompt evaluation. Serialising is faster than sharing.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rustykrab_core::Error;

use crate::with_conn;

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
}

impl DelegatedTask {
    fn from_row(row: &Row) -> rusqlite::Result<DelegatedTask> {
        let parse_time = |raw: Option<String>| -> Option<DateTime<Utc>> {
            raw.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&Utc))
        };
        let created: String = row.get("created_at")?;
        Ok(DelegatedTask {
            id: row.get("id")?,
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
        })
    }
}

const COLUMNS: &str = "id, message, conversation_id, status, result, error, principal, \
                       hop_budget, allowed_tools, trace_id, created_at, started_at, \
                       finished_at";

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

    /// Queue a task and return the handle the caller polls.
    pub async fn enqueue(
        &self,
        message: &str,
        conversation_id: Option<&str>,
        principal: Option<&str>,
        hop_budget: i64,
        allowed_tools: Option<Vec<String>>,
        trace_id: Option<&str>,
    ) -> Result<DelegatedTask, Error> {
        let task = DelegatedTask {
            id: Uuid::new_v4().to_string(),
            message: message.to_string(),
            conversation_id: conversation_id.map(str::to_string),
            status: TaskStatus::Queued,
            result: None,
            error: None,
            principal: principal.map(str::to_string),
            hop_budget: hop_budget.max(0),
            allowed_tools,
            trace_id: trace_id.map(str::to_string),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        };

        let row = task.clone();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO delegated_tasks (id, message, conversation_id, status, principal, \
                 hop_budget, allowed_tools, trace_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;

        Ok(task)
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
                    "UPDATE delegated_tasks SET status = 'running', started_at = ?2 \
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
    async fn settle(
        &self,
        id: &str,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) -> Result<(), Error> {
        let id = id.to_string();
        let now = Utc::now().to_rfc3339();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE delegated_tasks SET status = ?2, result = ?3, error = ?4, \
                 finished_at = ?5 WHERE id = ?1 AND status IN ('queued', 'running')",
                params![id, status.as_str(), result, error, now],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    pub async fn finish(&self, id: &str, result: &str) -> Result<(), Error> {
        self.settle(id, TaskStatus::Done, Some(result.to_string()), None)
            .await
    }

    pub async fn fail(&self, id: &str, error: &str) -> Result<(), Error> {
        self.settle(id, TaskStatus::Failed, None, Some(error.to_string()))
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
            None,
            Some("cancelled by the delegating peer".to_string()),
        )
        .await?;
        Ok(Some(previous))
    }

    /// Fail every task left `running` by a previous process.
    ///
    /// Called once at startup. The agent loop that owned them died with
    /// the process, so without this they stay `running` forever and a
    /// polling peer waits on work that will never finish.
    pub async fn fail_orphaned(&self) -> Result<usize, Error> {
        let now = Utc::now().to_rfc3339();
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE delegated_tasks SET status = 'failed', \
                     error = 'interrupted: the node restarted mid-task', finished_at = ?1 \
                     WHERE status = 'running'",
                    params![now],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(n)
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
    async fn a_restart_fails_tasks_it_interrupted() {
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

        assert_eq!(s.fail_orphaned().await.unwrap(), 1);

        let running = s.get(&running.id).await.unwrap().unwrap();
        assert_eq!(running.status, TaskStatus::Failed);
        assert!(running.error.unwrap().contains("restarted"));

        // Queued work predates no agent loop, so it survives the restart.
        let queued = s.get(&queued.id).await.unwrap().unwrap();
        assert_eq!(queued.status, TaskStatus::Queued);
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
