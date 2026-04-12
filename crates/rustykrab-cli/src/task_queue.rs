use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, Semaphore};

use rustykrab_agent::AgentEvent;
use rustykrab_core::types::MessageContent;
use rustykrab_gateway::AppState;

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
    tracing::info!(job_id = %job_id, task = %task_prompt, "executing scheduled job");

    // Create a fresh conversation for this job execution.
    let mut conv = match state.store.conversations().create() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(job_id = %job_id, "failed to create conversation for scheduled job: {e}");
            return;
        }
    };

    // Prefix the task so the agent knows this is a scheduled execution.
    let prompt = format!(
        "[Scheduled task] The following task was scheduled by the user and is now due. \
         Execute it and provide the result concisely.\n\nTask: {task_prompt}"
    );

    // Run the agent.
    let no_op_event = |_event: AgentEvent| {};
    let result =
        rustykrab_gateway::run_agent_streaming(state, &mut conv, &prompt, &no_op_event).await;

    let response_text = match result {
        Ok(msg) => match &msg.content {
            MessageContent::Text(t) => t.clone(),
            _ => "Scheduled task completed (no text response).".to_string(),
        },
        Err(_) => {
            tracing::error!(job_id = %job_id, "agent error executing scheduled job");
            "Sorry, the scheduled task encountered an error.".to_string()
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

    // Mark the job as executed (advances next_run_at or disables one-shot).
    if let Err(e) = store.jobs().mark_executed(job_id) {
        tracing::error!(job_id = %job_id, "failed to mark scheduled job as executed: {e}");
    }

    // Clean up the ephemeral conversation.
    if let Err(e) = state.store.conversations().delete(conv.id) {
        tracing::warn!(job_id = %job_id, "failed to clean up job conversation: {e}");
    }
}
