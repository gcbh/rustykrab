use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, Semaphore};

use chrono::Utc;
use rustykrab_agent::AgentEvent;
use rustykrab_core::types::{Conversation, MessageContent};
use rustykrab_gateway::AppState;
use uuid::Uuid;

/// Where a task originated and how to deliver results.
#[derive(Debug, Clone)]
pub enum TaskSource {
    /// Scheduled cron job.
    Cron {
        job_id: String,
        channel: Option<String>,
        chat_id: Option<String>,
    },
}

/// A unit of work submitted to the task queue.
#[derive(Debug)]
pub struct TaskRequest {
    /// The prompt to execute.
    pub prompt: String,
    /// Where the task came from / where to deliver results.
    pub source: TaskSource,
    /// Deduplication key. If set, only one task with this key can be
    /// queued or running at a time. Subsequent submissions with the
    /// same key are silently dropped.
    pub dedupe_key: Option<String>,
}

/// Handle for submitting tasks. Cheaply cloneable.
#[derive(Clone)]
pub struct TaskQueue {
    tx: mpsc::Sender<TaskRequest>,
}

impl TaskQueue {
    /// Create a new task queue and spawn the worker loop.
    ///
    /// * `capacity` — max tasks buffered before backpressure.
    /// * `max_concurrent` — max agent runs in flight simultaneously.
    /// * `state` — shared app state for agent execution.
    /// * `store` — job store for marking cron jobs executed.
    ///
    /// Returns the queue handle and the worker's `JoinHandle`.
    pub fn spawn(
        capacity: usize,
        max_concurrent: usize,
        state: AppState,
        store: rustykrab_store::Store,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(capacity);
        let handle = tokio::spawn(worker_loop(rx, max_concurrent, state, store));
        (Self { tx }, handle)
    }

    /// Submit a task to the queue. Returns `Err` if the queue is full
    /// or the worker has stopped.
    pub async fn submit(
        &self,
        request: TaskRequest,
    ) -> Result<(), mpsc::error::SendError<TaskRequest>> {
        self.tx.send(request).await
    }
}

async fn worker_loop(
    mut rx: mpsc::Receiver<TaskRequest>,
    max_concurrent: usize,
    state: AppState,
    store: rustykrab_store::Store,
) {
    let semaphore = Arc::new(Semaphore::new(max_concurrent));
    let active_keys: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    while let Some(task) = rx.recv().await {
        // Deduplication: skip if this key is already in flight.
        if let Some(ref key) = task.dedupe_key {
            let mut keys = active_keys.lock().await;
            if keys.contains(key) {
                tracing::debug!(dedupe_key = %key, "task already active, skipping");
                continue;
            }
            keys.insert(key.clone());
        }

        let dedupe_key = task.dedupe_key.clone();

        // Wait for a concurrency permit before spawning.
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let state = state.clone();
        let store = store.clone();
        let active_keys = active_keys.clone();

        tokio::spawn(async move {
            let _permit = permit; // held until this task completes

            execute_task(&task, &state, &store).await;

            // Release dedup key so the next occurrence can be queued.
            if let Some(key) = dedupe_key {
                active_keys.lock().await.remove(&key);
            }
        });
    }

    tracing::warn!("task queue worker exited — channel closed");
}

async fn execute_task(task: &TaskRequest, state: &AppState, store: &rustykrab_store::Store) {
    match &task.source {
        TaskSource::Cron {
            job_id,
            channel,
            chat_id,
        } => {
            execute_cron_task(
                job_id,
                &task.prompt,
                channel.as_deref(),
                chat_id.as_deref(),
                state,
                store,
            )
            .await;
        }
    }
}

async fn execute_cron_task(
    job_id: &str,
    task_prompt: &str,
    channel: Option<&str>,
    chat_id: Option<&str>,
    state: &AppState,
    store: &rustykrab_store::Store,
) {
    let started_at = Utc::now();
    tracing::info!(job_id = %job_id, task = %task_prompt, "executing scheduled job");

    // Load the job so we can resume its persistent conversation and use
    // last_run_at in the prompt.
    let job = match store.jobs().get_job(job_id) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(job_id = %job_id, "failed to load scheduled job: {e}");
            let finished_at = Utc::now();
            let _ = store.jobs().record_run(
                job_id,
                "error",
                Some(&format!("failed to load job: {e}")),
                started_at,
                finished_at,
            );
            return;
        }
    };

    let mut conv = match resume_or_create_conversation(&job, state, store) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(job_id = %job_id, "failed to resume conversation for scheduled job: {e}");
            let finished_at = Utc::now();
            let _ = store.jobs().record_run(
                job_id,
                "error",
                Some(&format!("failed to resume conversation: {e}")),
                started_at,
                finished_at,
            );
            return;
        }
    };

    let prompt = build_scheduled_prompt(task_prompt, job.last_run_at);

    // Run the agent.
    let no_op_event = |_event: AgentEvent| {};
    let result =
        rustykrab_gateway::run_agent_streaming(state, &mut conv, &prompt, &no_op_event).await;

    let (status, response_text) = match result {
        Ok(msg) => match &msg.content {
            MessageContent::Text(t) => ("ok", t.clone()),
            _ => (
                "ok",
                "Scheduled task completed (no text response).".to_string(),
            ),
        },
        Err(_) => {
            tracing::error!(job_id = %job_id, "agent error executing scheduled job");
            (
                "error",
                "Sorry, the scheduled task encountered an error.".to_string(),
            )
        }
    };

    // Route the response to the originating channel.
    match channel {
        Some("telegram") => {
            if let (Some(tg), Some(cid)) = (&state.telegram, chat_id) {
                if let Ok(chat_id_num) = cid.parse::<i64>() {
                    if let Err(e) = tg.send_text(chat_id_num, &response_text, 0).await {
                        tracing::error!(job_id = %job_id, "failed to send scheduled job result to Telegram: {e}");
                    }
                } else {
                    tracing::error!(job_id = %job_id, chat_id = %cid, "invalid Telegram chat_id");
                }
            }
        }
        Some("signal") => {
            if let (Some(sig), Some(number)) = (&state.signal, chat_id) {
                if let Err(e) = sig.send_text(number, &response_text).await {
                    tracing::error!(job_id = %job_id, "failed to send scheduled job result to Signal: {e}");
                }
            }
        }
        _ => {
            tracing::info!(
                job_id = %job_id,
                "scheduled job completed (no channel routing): {response_text}"
            );
        }
    }

    // Record the run before marking executed so the result is persisted
    // even if mark_executed fails.
    let finished_at = Utc::now();
    if let Err(e) = store.jobs().record_run(
        job_id,
        status,
        Some(&response_text),
        started_at,
        finished_at,
    ) {
        tracing::warn!(job_id = %job_id, "failed to record job run: {e}");
    }

    // Mark the job as executed (advances next_run_at or disables one-shot).
    if let Err(e) = store.jobs().mark_executed(job_id) {
        tracing::error!(job_id = %job_id, "failed to mark scheduled job as executed: {e}");
    }

    // Conversation is intentionally NOT deleted: the next run of this job
    // resumes it so the agent sees prior context. Deletion happens when
    // the job itself is deleted (see CronAdapter::delete_job).
}

/// Resume this job's persistent conversation, or create one on the first
/// run. If the stored id points at a conversation that has been deleted
/// out from under us, silently create a fresh one and re-link.
fn resume_or_create_conversation(
    job: &rustykrab_store::ScheduledJob,
    state: &AppState,
    store: &rustykrab_store::Store,
) -> Result<Conversation, rustykrab_core::Error> {
    if let Some(cid) = &job.conversation_id {
        if let Ok(uuid) = Uuid::parse_str(cid) {
            match state.store.conversations().get(uuid) {
                Ok(c) => return Ok(c),
                Err(rustykrab_core::Error::NotFound(_)) => {
                    tracing::warn!(
                        job_id = %job.id,
                        stale_conv_id = %cid,
                        "stored conversation missing; creating replacement"
                    );
                }
                Err(e) => return Err(e),
            }
        } else {
            tracing::warn!(
                job_id = %job.id,
                bad_conv_id = %cid,
                "stored conversation id is malformed; creating replacement"
            );
        }
    }

    let conv = state.store.conversations().create()?;
    store
        .jobs()
        .set_conversation_id(&job.id, &conv.id.to_string())?;
    Ok(conv)
}

/// Build the user message prepended to the agent's turn. On the first run
/// there's no prior context, so we say so; on subsequent runs the prompt
/// points the agent at the conversation history it already has.
fn build_scheduled_prompt(
    task_prompt: &str,
    last_run_at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    match last_run_at {
        Some(last) => format!(
            "[Scheduled task] Your scheduled task is due again. Earlier runs are in \
             this conversation — refer to them for any state, filenames, or \
             decisions you made previously. Last run was at {last}.\n\n\
             Task: {task_prompt}"
        ),
        None => format!(
            "[Scheduled task] Your scheduled task is due for the first time. This \
             conversation will persist across runs, so any state you want to reuse \
             (filenames, conventions, summaries) can simply be written down here.\n\n\
             Task: {task_prompt}"
        ),
    }
}
