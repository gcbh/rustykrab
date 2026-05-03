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
        /// Channel-specific thread identifier so the result can land in the
        /// thread that scheduled the job. Telegram: forum topic thread_id
        /// (numeric string). Slack: thread_ts. `None` posts at top level.
        thread_id: Option<String>,
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
            thread_id,
        } => {
            execute_cron_task(
                job_id,
                &task.prompt,
                channel.as_deref(),
                chat_id.as_deref(),
                thread_id.as_deref(),
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
    thread_id: Option<&str>,
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

    // Refuse to invoke the agent with an empty prompt body. Older builds
    // accepted empty `task` strings during creation; those rows still live
    // in the DB and would otherwise produce "no task or instruction has
    // been provided" responses on every fire.
    if task_prompt.trim().is_empty() {
        let finished_at = Utc::now();
        tracing::error!(
            job_id = %job_id,
            "scheduled job has an empty task body — disabling so it stops firing"
        );
        let _ = store.jobs().record_run(
            job_id,
            "error",
            Some("scheduled job has an empty task body; disabled. Recreate with a non-empty task."),
            started_at,
            finished_at,
        );
        if let Err(e) = store.jobs().set_enabled(job_id, false) {
            tracing::warn!(job_id = %job_id, "failed to disable empty-task job: {e}");
        }
        return;
    }

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

    // Resolve the delivery target. Precedence: job-specified > stored on the
    // job's persistent conversation > operator-wide env defaults. The
    // env defaults are the safety net for jobs created by older builds (or
    // by the model without channel/chat_id) so their output isn't silently
    // dropped on the floor.
    let (effective_channel, effective_chat_id, effective_thread_id) =
        resolve_delivery_target(channel, chat_id, thread_id, &conv);

    let prompt = build_scheduled_prompt(
        task_prompt,
        job.last_run_at,
        effective_channel.as_deref(),
        effective_chat_id.as_deref(),
    );

    // Run the agent. Mint a fresh trace id per scheduled run so prompt-log
    // rows and agent logs for this job line up.
    let trace_id = Uuid::new_v4();
    tracing::info!(%trace_id, job_id = %job.id, "scheduled task starting");
    let no_op_event = |_event: AgentEvent| {};
    let result =
        rustykrab_gateway::run_agent_streaming(state, &mut conv, &prompt, &no_op_event, trace_id)
            .await;

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
    match effective_channel.as_deref() {
        Some("telegram") => {
            if let (Some(tg), Some(cid)) = (&state.telegram, effective_chat_id.as_deref()) {
                if let Ok(chat_id_num) = cid.parse::<i64>() {
                    let tg_thread = effective_thread_id
                        .as_deref()
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(0);
                    if let Err(e) = tg.send_text(chat_id_num, &response_text, tg_thread).await {
                        tracing::error!(job_id = %job_id, "failed to send scheduled job result to Telegram: {e}");
                    }
                } else {
                    tracing::error!(job_id = %job_id, chat_id = %cid, "invalid Telegram chat_id");
                }
            } else {
                tracing::warn!(
                    job_id = %job_id,
                    has_telegram = state.telegram.is_some(),
                    has_chat_id = effective_chat_id.is_some(),
                    "telegram routing unavailable; result discarded: {response_text}"
                );
            }
        }
        Some("slack") => {
            if let (Some(sl), Some(channel_id)) = (&state.slack, effective_chat_id.as_deref()) {
                if let Err(e) = sl
                    .send_text(channel_id, &response_text, effective_thread_id.as_deref())
                    .await
                {
                    tracing::error!(job_id = %job_id, "failed to send scheduled job result to Slack: {e}");
                }
            } else {
                tracing::warn!(
                    job_id = %job_id,
                    has_slack = state.slack.is_some(),
                    has_chat_id = effective_chat_id.is_some(),
                    "slack routing unavailable; result discarded: {response_text}"
                );
            }
        }
        Some("signal") => {
            if let (Some(sig), Some(number)) = (&state.signal, effective_chat_id.as_deref()) {
                if let Err(e) = sig.send_text(number, &response_text).await {
                    tracing::error!(job_id = %job_id, "failed to send scheduled job result to Signal: {e}");
                }
            } else {
                tracing::warn!(
                    job_id = %job_id,
                    has_signal = state.signal.is_some(),
                    has_chat_id = effective_chat_id.is_some(),
                    "signal routing unavailable; result discarded: {response_text}"
                );
            }
        }
        Some(other) => {
            tracing::warn!(
                job_id = %job_id,
                channel = %other,
                "unknown channel for scheduled job; result discarded: {response_text}"
            );
        }
        None => {
            tracing::warn!(
                job_id = %job_id,
                "scheduled job has no delivery channel — set channel/chat_id on the \
                 job, or set RUSTYKRAB_DEFAULT_CHANNEL + RUSTYKRAB_DEFAULT_CHAT_ID. \
                 Result discarded: {response_text}"
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
/// points the agent at the conversation history it already has. Also tells
/// the model where its final response will be delivered, so it knows it's
/// being asked to actually produce output (not chat about what it might do).
fn build_scheduled_prompt(
    task_prompt: &str,
    last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    channel: Option<&str>,
    chat_id: Option<&str>,
) -> String {
    let delivery = match (channel, chat_id) {
        (Some(c), Some(id)) => format!(
            "Your final assistant message this turn will be delivered to {c} ({id}). \
             Treat that final message as the briefing/answer the recipient will \
             receive — do not ask for clarification, do not promise future updates."
        ),
        _ => "Your final assistant message this turn IS the deliverable for this \
             scheduled task. Produce it directly — do not ask for clarification, \
             do not promise future updates."
            .to_string(),
    };
    let body = match last_run_at {
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
    };
    format!("{body}\n\n{delivery}")
}

/// Env vars that name a fallback delivery target for scheduled jobs that
/// don't carry their own channel info. Set these on a single-user deploy so
/// briefings always land somewhere instead of getting dropped to the log.
const DEFAULT_CHANNEL_ENV: &str = "RUSTYKRAB_DEFAULT_CHANNEL";
const DEFAULT_CHAT_ID_ENV: &str = "RUSTYKRAB_DEFAULT_CHAT_ID";
const DEFAULT_THREAD_ID_ENV: &str = "RUSTYKRAB_DEFAULT_THREAD_ID";

/// Resolve the channel/chat_id/thread_id triple to use for delivering this
/// run's output. Precedence (first match wins):
///   1. The job's stored fields (set when the job was created).
///   2. The job's persistent conversation's `channel_*` fields (set when
///      the conversation originated from a channel).
///   3. Operator-wide env defaults.
fn resolve_delivery_target(
    job_channel: Option<&str>,
    job_chat_id: Option<&str>,
    job_thread_id: Option<&str>,
    conv: &Conversation,
) -> (Option<String>, Option<String>, Option<String>) {
    let channel = job_channel
        .map(|s| s.to_string())
        .or_else(|| conv.channel_source.clone())
        .or_else(|| std::env::var(DEFAULT_CHANNEL_ENV).ok())
        .filter(|s| !s.is_empty());
    let chat_id = job_chat_id
        .map(|s| s.to_string())
        .or_else(|| conv.channel_id.clone())
        .or_else(|| std::env::var(DEFAULT_CHAT_ID_ENV).ok())
        .filter(|s| !s.is_empty());
    let thread_id = job_thread_id
        .map(|s| s.to_string())
        .or_else(|| conv.channel_thread_id.clone())
        .or_else(|| std::env::var(DEFAULT_THREAD_ID_ENV).ok())
        .filter(|s| !s.is_empty());
    (channel, chat_id, thread_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Mutex;

    /// Env vars are process-global. Serialize tests that mutate them so they
    /// don't race when run with `--test-threads > 1`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn empty_conv() -> Conversation {
        Conversation {
            id: Uuid::new_v4(),
            messages: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            summary: None,
            detected_profile: None,
            channel_source: None,
            channel_id: None,
            channel_thread_id: None,
        }
    }

    fn channel_conv(source: &str, id: &str, thread: Option<&str>) -> Conversation {
        let mut c = empty_conv();
        c.channel_source = Some(source.to_string());
        c.channel_id = Some(id.to_string());
        c.channel_thread_id = thread.map(|s| s.to_string());
        c
    }

    #[test]
    fn scheduled_prompt_first_run_includes_delivery_target() {
        let prompt = build_scheduled_prompt(
            "Write the daily briefing.",
            None,
            Some("telegram"),
            Some("12345"),
        );
        assert!(prompt.contains("first time"));
        assert!(prompt.contains("Task: Write the daily briefing."));
        assert!(prompt.contains("delivered to telegram (12345)"));
        assert!(prompt.contains("do not promise future updates"));
    }

    #[test]
    fn scheduled_prompt_recurring_includes_last_run() {
        let last = Utc.with_ymd_and_hms(2026, 4, 30, 9, 0, 0).unwrap();
        let prompt = build_scheduled_prompt("Daily briefing.", Some(last), None, None);
        assert!(prompt.contains("due again"));
        assert!(prompt.contains("Last run was at 2026-04-30 09:00:00 UTC"));
        // No channel info → still tells the model the message IS the deliverable.
        assert!(prompt.contains("IS the deliverable"));
    }

    #[test]
    fn delivery_target_prefers_job_fields() {
        let _lock = ENV_LOCK.lock().unwrap();
        let conv = channel_conv("telegram", "999", Some("7"));
        let (ch, cid, tid) =
            resolve_delivery_target(Some("slack"), Some("CABC"), Some("ts.1"), &conv);
        assert_eq!(ch.as_deref(), Some("slack"));
        assert_eq!(cid.as_deref(), Some("CABC"));
        assert_eq!(tid.as_deref(), Some("ts.1"));
    }

    #[test]
    fn delivery_target_falls_back_to_conversation() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Make sure no stray env defaults leak in from the surrounding env.
        std::env::remove_var(DEFAULT_CHANNEL_ENV);
        std::env::remove_var(DEFAULT_CHAT_ID_ENV);
        std::env::remove_var(DEFAULT_THREAD_ID_ENV);
        let conv = channel_conv("telegram", "42", Some("9"));
        let (ch, cid, tid) = resolve_delivery_target(None, None, None, &conv);
        assert_eq!(ch.as_deref(), Some("telegram"));
        assert_eq!(cid.as_deref(), Some("42"));
        assert_eq!(tid.as_deref(), Some("9"));
    }

    #[test]
    fn delivery_target_falls_back_to_env_when_job_and_conv_empty() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var(DEFAULT_CHANNEL_ENV, "telegram");
        std::env::set_var(DEFAULT_CHAT_ID_ENV, "55");
        std::env::remove_var(DEFAULT_THREAD_ID_ENV);
        let conv = empty_conv();
        let (ch, cid, tid) = resolve_delivery_target(None, None, None, &conv);
        std::env::remove_var(DEFAULT_CHANNEL_ENV);
        std::env::remove_var(DEFAULT_CHAT_ID_ENV);
        assert_eq!(ch.as_deref(), Some("telegram"));
        assert_eq!(cid.as_deref(), Some("55"));
        assert_eq!(tid, None);
    }

    #[test]
    fn delivery_target_returns_none_when_nothing_configured() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(DEFAULT_CHANNEL_ENV);
        std::env::remove_var(DEFAULT_CHAT_ID_ENV);
        std::env::remove_var(DEFAULT_THREAD_ID_ENV);
        let conv = empty_conv();
        let (ch, cid, tid) = resolve_delivery_target(None, None, None, &conv);
        assert!(ch.is_none());
        assert!(cid.is_none());
        assert!(tid.is_none());
    }

    #[test]
    fn delivery_target_treats_empty_strings_as_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var(DEFAULT_CHANNEL_ENV);
        std::env::remove_var(DEFAULT_CHAT_ID_ENV);
        std::env::remove_var(DEFAULT_THREAD_ID_ENV);
        let conv = empty_conv();
        let (ch, cid, _) = resolve_delivery_target(Some(""), Some(""), None, &conv);
        assert!(ch.is_none(), "empty channel should not satisfy the filter");
        assert!(cid.is_none(), "empty chat_id should not satisfy the filter");
    }
}
