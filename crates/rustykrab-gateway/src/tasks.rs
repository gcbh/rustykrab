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

use crate::run::run_agent_with_options;
use crate::AppState;
use rustykrab_runtime::RunOptions;

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
    let store = state.agent.store.tasks();

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
        Some(id) => match state.agent.store.conversations().get(id).await {
            Ok(conv) => conv,
            Err(_) => {
                tracing::warn!(
                    task_id = %task.id,
                    conversation_id = %id,
                    "delegated task named an unknown conversation; opening a fresh one"
                );
                state
                    .agent
                    .store
                    .conversations()
                    .create()
                    .await
                    .map_err(|e| format!("could not open a conversation: {e}"))?
            }
        },
        None => state
            .agent
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
            .agent
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
        denied_tools: denied_tools_for(&available_tool_names(&state), &task),
        ..RunOptions::default()
    };

    let assistant = run_agent_with_options(&state, &mut conv, &task.message, trace_id, &options)
        .await
        .map_err(|status| format!("the agent run failed ({status})"))?;

    conv.updated_at = Utc::now();
    if let Err(e) = state
        .agent
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

/// Tool names this node currently offers, which the ceiling is expressed
/// against — a denial only means anything for a tool that exists.
fn available_tool_names(state: &AppState) -> Vec<&str> {
    state
        .agent
        .tools
        .iter()
        .filter(|t| t.available())
        .map(|t| t.name())
        .collect()
}

/// Tools withheld from every delegated run regardless of configuration.
///
/// The credential family and the outbound `message` tool are the two
/// ways a delegated instruction could reach past the task it was given —
/// into this machine's stored secrets, or out through its own Telegram
/// and Signal accounts. A delegated instruction is composed by the
/// peer's *model*, so anything that reaches the peer as untrusted text
/// (a fetched web page, a search result) can end up phrased as a task
/// for this node. Withholding these here means the node's own operator
/// decides that, not the sentence that arrived over the network.
const ALWAYS_DENIED: &[&str] = &[
    "credential_read",
    "credential_write",
    "credential_request",
    "message",
    "gateway",
];

/// Tools a delegated run may use, as configured on this node.
///
/// `RUSTYKRAB_DELEGATION_TOOLS` is node-authoritative: unset applies the
/// default posture below, `all` lifts it, and a comma-separated list
/// names the only tools a delegated run may use. The submitting peer can
/// narrow this further per task but can never widen it — this machine
/// decides what a peer may do on it, not the peer.
fn configured_allowlist() -> Option<Vec<String>> {
    let raw = std::env::var("RUSTYKRAB_DELEGATION_TOOLS").ok()?;
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("all") {
        return None;
    }
    Some(
        raw.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
    )
}

/// Tools this particular delegated run may not use.
///
/// Three layers, most specific last:
///
/// 1. The sub-agent family, always. A node is a sub-agent; letting it
///    spawn its own would make the topology a tree of unknown depth
///    across machines. Work it needs to break up, it queues for itself.
/// 2. [`ALWAYS_DENIED`], plus anything outside this node's configured
///    allowlist.
/// 3. `nodes`, once the hop budget is spent, so the task cannot be
///    handed onward. Without it two peers listing each other bounce a
///    task between them indefinitely, at minutes of local inference per
///    hop; the `subagents` depth counter cannot help, being process-local.
fn denied_tools_for(available: &[&str], task: &DelegatedTask) -> Vec<String> {
    let mut denied: Vec<String> = rustykrab_core::capability::SUBAGENT_TOOL_NAMES
        .iter()
        .map(|t| t.to_string())
        .collect();
    denied.extend(ALWAYS_DENIED.iter().map(|t| t.to_string()));

    if let Some(allowed) = configured_allowlist() {
        for name in available {
            if !allowed.iter().any(|a| a == name) {
                denied.push((*name).to_string());
            }
        }
    }

    // Per-task narrowing from the submitting peer. Intersects with the
    // above rather than replacing it: a task may ask for less than the
    // node allows, never more.
    if let Some(requested) = task.allowed_tools.as_ref() {
        for name in available {
            if !requested.iter().any(|a| a == name) {
                denied.push((*name).to_string());
            }
        }
    }

    if task.hop_budget <= 0 {
        denied.push("nodes".to_string());
    }

    denied.sort();
    denied.dedup();
    denied
}

/// Whether a status means the caller should keep polling.
pub fn is_pending(status: TaskStatus) -> bool {
    !status.is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative slice of what a node actually offers.
    const AVAILABLE: &[&str] = &[
        "read",
        "write",
        "exec",
        "web_fetch",
        "nodes",
        "subagents",
        "agents_list",
        "credential_read",
        "credential_write",
        "credential_request",
        "message",
        "gateway",
    ];

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
            allowed_tools: None,
            trace_id: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }

    fn denied(task: &DelegatedTask) -> Vec<String> {
        denied_tools_for(AVAILABLE, task)
    }

    #[test]
    fn a_spent_hop_budget_denies_onward_delegation() {
        assert!(denied(&task(0)).contains(&"nodes".to_string()));
        assert!(denied(&task(-1)).contains(&"nodes".to_string()));
        assert!(
            !denied(&task(1)).contains(&"nodes".to_string()),
            "a node with hops left may delegate onward"
        );
    }

    #[test]
    fn a_delegated_run_never_gets_the_subagent_family() {
        // This is what makes the node a sub-agent rather than another
        // orchestrator: it does its own work, and splits it by queueing
        // for itself rather than spawning further agents.
        let denied = denied(&task(5));
        for tool in rustykrab_core::capability::SUBAGENT_TOOL_NAMES {
            assert!(
                denied.contains(&tool.to_string()),
                "'{tool}' must be withheld from a delegated run"
            );
        }
    }

    #[test]
    fn credentials_and_outbound_messaging_are_always_withheld() {
        // A delegated instruction is composed by the peer's model, so it
        // can carry text this node never vetted. It must not be able to
        // reach this machine's secrets or send from its accounts.
        let denied = denied(&task(0));
        for tool in ALWAYS_DENIED {
            assert!(
                denied.contains(&tool.to_string()),
                "'{tool}' must be withheld from a delegated run"
            );
        }
        // Ordinary work is still possible.
        assert!(!denied.contains(&"read".to_string()));
        assert!(!denied.contains(&"exec".to_string()));
    }

    #[test]
    fn a_task_can_narrow_the_ceiling_but_not_widen_it() {
        let mut narrowed = task(9);
        // Asks for read plus two tools the node always withholds.
        narrowed.allowed_tools = Some(vec![
            "read".to_string(),
            "credential_read".to_string(),
            "message".to_string(),
        ]);
        let denied = denied(&narrowed);

        assert!(!denied.contains(&"read".to_string()), "read was requested");
        // Everything else the node offers is now off, because the task
        // asked to be limited.
        assert!(denied.contains(&"exec".to_string()));
        assert!(denied.contains(&"web_fetch".to_string()));
        // And the request cannot buy back what the node withholds.
        assert!(denied.contains(&"credential_read".to_string()));
        assert!(denied.contains(&"message".to_string()));
    }

    #[test]
    fn the_node_allowlist_overrides_what_a_task_may_touch() {
        // Process-global: this is the only test that sets the var.
        std::env::set_var("RUSTYKRAB_DELEGATION_TOOLS", "read, web_fetch");
        let denied = denied(&task(0));
        assert!(!denied.contains(&"read".to_string()));
        assert!(!denied.contains(&"web_fetch".to_string()));
        assert!(
            denied.contains(&"exec".to_string()),
            "a tool outside the node's allowlist must be withheld"
        );

        // `all` lifts the allowlist without lifting the fixed denials.
        std::env::set_var("RUSTYKRAB_DELEGATION_TOOLS", "all");
        let denied = denied_tools_for(AVAILABLE, &task(0));
        assert!(!denied.contains(&"exec".to_string()));
        assert!(denied.contains(&"credential_read".to_string()));

        std::env::remove_var("RUSTYKRAB_DELEGATION_TOOLS");
    }

    #[test]
    fn denials_are_deduplicated() {
        // `nodes` can be denied by both the hop budget and an allowlist;
        // the runner should see it once.
        let denied = denied(&task(0));
        let mut unique = denied.clone();
        unique.dedup();
        assert_eq!(denied, unique);
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
