//! Node-side worker that drains the delegated-task queue.
//!
//! A peer submits work with `POST /api/tasks` and gets an id back
//! immediately; this worker runs it. Splitting submission from execution
//! is what lets a delegation outlive the caller's tool-call budget — the
//! old synchronous path held an HTTP request open for the whole agent
//! turn, which on a local model routinely exceeds any sane timeout.
//!
//! Exactly one task runs at a time, by design. See
//! `rustykrab_store::TaskStore` for why serialising beats sharing on a
//! machine whose model has one KV-cache slot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_store::{DelegatedTask, TaskStatus};
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::orchestrate::{run_agent_with_options, RunOptions};
use crate::AppState;

/// Fallback poll interval. The queue also rings [`TaskQueueSignal`] on
/// submit, so this only covers a missed notification or a task enqueued
/// by a previous process.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a finished task's record is retained before the sweeper
/// deletes it. Long enough that a peer whose own process restarted can
/// still collect a result it never read.
const RESULT_RETENTION_HOURS: i64 = 48;

/// Wake-up channel and cancellation registry shared between the HTTP
/// handlers and the worker.
#[derive(Clone, Default)]
pub struct TaskQueueSignal {
    notify: Arc<Notify>,
    /// Abort handles for tasks currently executing, keyed by task id.
    /// Only ever holds one entry today — the worker is single-threaded —
    /// but keyed so a second worker would not need a different shape.
    running: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

impl TaskQueueSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the worker there is something to pick up.
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Abort a task's agent loop if it is mid-run. Returns whether one
    /// was actually running: a queued task needs no abort, its row is
    /// already terminal and the worker will skip it.
    pub fn abort(&self, task_id: &str) -> bool {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        match running.remove(task_id) {
            Some(handle) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    fn register(&self, task_id: &str, handle: AbortHandle) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task_id.to_string(), handle);
    }

    fn unregister(&self, task_id: &str) {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(task_id);
    }
}

/// Drain the delegated-task queue until the process shuts down.
///
/// Spawned once at startup and held in the CLI's `infra_handles`.
pub async fn run_task_worker(state: AppState) {
    let signal = state.task_signal.clone();
    let store = state.store.tasks();

    // Tasks left `running` belonged to an agent loop that died with the
    // previous process. Nothing will ever complete them, so fail them
    // now rather than leaving a peer polling forever.
    match store.fail_orphaned().await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(count = n, "failed delegated tasks orphaned by a restart"),
        Err(e) => tracing::error!(error = %e, "could not reconcile orphaned delegated tasks"),
    }

    // Thread affinity: bias the next claim toward the conversation just
    // run, so a multi-step delegation keeps its warm prompt prefix.
    let mut last_conversation: Option<String> = None;

    loop {
        let claimed = match store.claim_next(last_conversation.as_deref()).await {
            Ok(claimed) => claimed,
            Err(e) => {
                tracing::error!(error = %e, "delegated task claim failed");
                None
            }
        };

        let Some(task) = claimed else {
            // Nothing queued. Sweep expired results while idle, then wait
            // for either a submission or the fallback tick.
            if let Err(e) = store
                .sweep(chrono::Duration::hours(RESULT_RETENTION_HOURS))
                .await
            {
                tracing::warn!(error = %e, "delegated task sweep failed");
            }
            tokio::select! {
                _ = signal.notify.notified() => {}
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            continue;
        };

        let task_id = task.id.clone();
        tracing::info!(
            task_id = %task_id,
            principal = task.principal.as_deref().unwrap_or("unknown"),
            hop_budget = task.hop_budget,
            continuing = task.conversation_id.is_some(),
            "running delegated task"
        );

        // Spawn rather than await inline so a cancel can abort the agent
        // loop. The handle is registered before the first await point so
        // a cancel arriving immediately still finds it.
        let handle = tokio::spawn(execute(state.clone(), task.clone()));
        signal.register(&task_id, handle.abort_handle());
        let outcome = handle.await;
        signal.unregister(&task_id);

        match outcome {
            Ok(Ok(reply)) => {
                last_conversation = reply.conversation_id.clone();
                if let Err(e) = store.finish(&task_id, &reply.text).await {
                    tracing::error!(task_id = %task_id, error = %e, "could not record task result");
                }
                tracing::info!(task_id = %task_id, "delegated task done");
            }
            Ok(Err(e)) => {
                tracing::warn!(task_id = %task_id, error = %e, "delegated task failed");
                if let Err(e) = store.fail(&task_id, &e).await {
                    tracing::error!(task_id = %task_id, error = %e, "could not record task failure");
                }
            }
            // Aborted by a cancel, or the run panicked. `cancel` already
            // wrote the terminal row in the first case; `fail` is a no-op
            // against a row that is no longer queued or running, so this
            // is safe for both.
            Err(join_err) => {
                let reason = if join_err.is_cancelled() {
                    "cancelled by the delegating peer".to_string()
                } else {
                    format!("the agent loop panicked: {join_err}")
                };
                tracing::warn!(task_id = %task_id, reason = %reason, "delegated task ended early");
                if let Err(e) = store.fail(&task_id, &reason).await {
                    tracing::error!(task_id = %task_id, error = %e, "could not record task failure");
                }
            }
        }
    }
}

/// What a completed run hands back to the worker.
struct TaskReply {
    text: String,
    conversation_id: Option<String>,
}

/// Run one delegated task to completion.
///
/// Mirrors the `send_message` HTTP handler: load or open a conversation,
/// append the peer's instruction, run the agent, persist the turn.
async fn execute(state: AppState, task: DelegatedTask) -> Result<TaskReply, String> {
    // Continue the caller's thread when it named one. A conversation that
    // has since been deleted falls back to a fresh one rather than
    // failing the task — the message is self-contained by contract, so
    // losing the thread costs prefill, not correctness.
    let mut conv = match task
        .conversation_id
        .as_deref()
        .and_then(|id| id.parse().ok())
    {
        Some(id) => match state.store.conversations().get(id).await {
            Ok(conv) => conv,
            Err(_) => {
                tracing::warn!(
                    task_id = %task.id,
                    conversation_id = %id,
                    "delegated task named an unknown conversation; opening a fresh one"
                );
                state
                    .store
                    .conversations()
                    .create()
                    .await
                    .map_err(|e| format!("could not open a conversation: {e}"))?
            }
        },
        None => state
            .store
            .conversations()
            .create()
            .await
            .map_err(|e| format!("could not open a conversation: {e}"))?,
    };

    let conversation_id = conv.id.to_string();
    if task.conversation_id.as_deref() != Some(conversation_id.as_str()) {
        // Record it before the run, not after: a task that dies mid-turn
        // should still tell the peer which thread to resume.
        if let Err(e) = state
            .store
            .tasks()
            .set_conversation(&task.id, &conversation_id)
            .await
        {
            tracing::warn!(task_id = %task.id, error = %e, "could not record task conversation");
        }
    }

    let persisted_ids: Vec<Uuid> = conv.messages.iter().map(|m| m.id).collect();
    conv.messages.push(Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(task.message.clone()),
        created_at: Utc::now(),
        agent_version: Message::version_stamp(),
    });
    conv.updated_at = Utc::now();

    // Correlate both machines' logs under the delegating peer's trace id
    // when it sent one.
    let trace_id = task
        .trace_id
        .as_deref()
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(Uuid::new_v4);

    let options = RunOptions {
        denied_tools: denied_tools_for(&task),
        ..RunOptions::default()
    };

    let assistant = run_agent_with_options(&state, &mut conv, &task.message, trace_id, &options)
        .await
        .map_err(|status| format!("the agent run failed ({status})"))?;

    conv.updated_at = Utc::now();
    if let Err(e) = state
        .store
        .conversations()
        .save_turn(&conv, &persisted_ids)
        .await
    {
        // The work is done and the reply is in hand; failing the task
        // over a persistence error would throw away minutes of local
        // inference. Report it and return the result.
        tracing::error!(task_id = %task.id, error = %e, "could not persist delegated turn");
    }

    let text = match &assistant.content {
        MessageContent::Text(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| String::new()),
    };

    Ok(TaskReply {
        text,
        conversation_id: Some(conversation_id),
    })
}

/// Tools a delegated run may not use, given its remaining hop budget.
///
/// With no hops left the node may not delegate onward. That is the
/// cross-machine analogue of the local recursion guard in the
/// `subagents` tool, and without it a node whose own `RUSTYKRAB_NODES`
/// points back at the primary would recurse indefinitely — each hop
/// costing minutes of local inference.
fn denied_tools_for(task: &DelegatedTask) -> Vec<String> {
    if task.hop_budget > 0 {
        Vec::new()
    } else {
        vec!["nodes".to_string()]
    }
}

/// Whether a status means the caller should keep polling.
pub fn is_pending(status: TaskStatus) -> bool {
    !status.is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(hop_budget: i64) -> DelegatedTask {
        DelegatedTask {
            id: "t1".to_string(),
            message: "do the thing".to_string(),
            conversation_id: None,
            status: TaskStatus::Queued,
            result: None,
            error: None,
            principal: None,
            hop_budget,
            trace_id: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn a_spent_hop_budget_denies_onward_delegation() {
        assert_eq!(denied_tools_for(&task(0)), vec!["nodes".to_string()]);
        assert_eq!(denied_tools_for(&task(-1)), vec!["nodes".to_string()]);
        assert!(denied_tools_for(&task(1)).is_empty());
    }

    #[test]
    fn abort_reports_whether_a_task_was_running() {
        let signal = TaskQueueSignal::new();
        // Nothing registered: a queued task needs no abort.
        assert!(!signal.abort("t1"));
    }

    #[test]
    fn pending_covers_exactly_the_non_terminal_states() {
        assert!(is_pending(TaskStatus::Queued));
        assert!(is_pending(TaskStatus::Running));
        assert!(!is_pending(TaskStatus::Done));
        assert!(!is_pending(TaskStatus::Failed));
        assert!(!is_pending(TaskStatus::Cancelled));
    }
}
