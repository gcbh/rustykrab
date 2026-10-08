//! Scheduled jobs as work items (control-layer plan, sections 12.1 and 13,
//! scenario 30), behind `RUSTYKRAB_CRON_WORK_ITEMS=1`.
//!
//! Without the switch a due job runs as a task-queue conversation
//! (`task_queue.rs`, `job_executor_loop`). With it, every firing is a work
//! item the controller schedules like any other: filed with `trigger:
//! at(time)`, linked from `scheduled_jobs.work_item_id`, leased to the local
//! worker only when no interactive turn holds the model (12.1), one at a
//! time, overdue ones included. What a job run has always had comes with it:
//!
//! - **its persistent conversation**: the controller runs a firing under
//!   the job's conversation id, and [`JobTranscripts`] hands the local
//!   worker that conversation to continue, so the run sees earlier runs and
//!   is kept where they are;
//! - **its SKILL.md**: a task naming a registered skill has the skill's body
//!   appended to the run's system prompt, as `<skill_instructions>`;
//! - **its delivery target**: the run's user turn opens with the scheduled
//!   prompt (due again or for the first time, the memory_search guidance,
//!   and where the result goes), and when the firing is `done` its summary
//!   is delivered there, with any credential link the run minted, and
//!   recorded as the job's run before the job advances.
//!
//! A firing that fails, expires or is cancelled records an `error` run and
//! the job advances; the controller's notice for it, routed to the job's
//! target (`work_host.rs`), says what happened. A firing parked on the user
//! (a question, a credential) counts as that firing's end for the schedule,
//! so a recurring job keeps firing while the question waits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rustykrab_agent::{Resumed, RunTranscripts};
use rustykrab_control::controller::SUMMARY_EVIDENCE;
use rustykrab_control::handle::ControlHandle;
use rustykrab_control::worker::Brief;
use rustykrab_control::Provenance;
use rustykrab_core::types::Conversation;
use rustykrab_core::work::{
    ArtifactRef, Budget, Evidence, PlanOutcome, Status, Trigger, WorkItem, WorkItemDraft, WorkKind,
};
use rustykrab_gateway::AppState;
use rustykrab_skills::SkillRegistry;
use rustykrab_store::{ScheduledJob, Store};
use uuid::Uuid;

use crate::task_queue;

/// The switch: `1` (or `true`) files firings as work items.
pub(crate) const FLAG: &str = "RUSTYKRAB_CRON_WORK_ITEMS";

/// The artifact ref naming a firing's job.
const JOB_REF: &str = "scheduled_job";

/// The evidence a firing gets once its result is delivered and recorded as
/// the job's run: the job id. Present means done with, for the schedule.
const JOB_RUN: &str = "job_run";

/// Who files firings.
const ACTOR: &str = "scheduler";

/// How far past filing an overdue firing's trigger is set, so the
/// controller cannot lease it before its job points at it (the job's link
/// is what runs it in the job's conversation).
const LINK_MARGIN: TimeDelta = TimeDelta::seconds(2);

/// A firing's budget: a job run has always had the profile's iterations and
/// no token or time cap of its own. Keep a generous but finite envelope
/// within the default standing judgment, so every normal firing does not
/// create a fresh plan approval. A tighter user policy still applies.
fn firing_budget() -> Budget {
    Budget {
        iterations: 200,
        tokens: 1_000_000,
        wall_seconds: 3_600,
        ..Budget::default()
    }
}

/// Whether the switch is on.
pub(crate) fn enabled() -> bool {
    crate::overseer::manager_enabled()
        || std::env::var(FLAG)
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false)
}

/// Start the firing loop when the switch is on, polling every control
/// tick (`RUSTYKRAB_CONTROL_TICK_SECS`, 5 s by default). `None`: the switch
/// is off, and the caller starts the task-queue executor instead.
pub(crate) fn start(state: AppState) -> Option<tokio::task::JoinHandle<()>> {
    if !enabled() {
        return None;
    }
    let control = state.control.clone()?;
    let every = std::env::var("RUSTYKRAB_CONTROL_TICK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5)
        .max(1);
    tracing::info!(
        every_secs = every,
        "scheduled jobs fire as work items ({FLAG})"
    );
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(every));
        let lead = TimeDelta::seconds(i64::try_from(every).unwrap_or(5));
        loop {
            interval.tick().await;
            if let Err(e) = poll(&state, control.as_ref(), Utc::now(), lead).await {
                tracing::warn!(error = %e, "scheduled firings not polled");
            }
        }
    }))
}

/// One pass: finish what closed, cancel firings of deleted jobs, file what
/// is due within `lead`.
pub(crate) async fn poll(
    state: &AppState,
    control: &dyn ControlHandle,
    now: DateTime<Utc>,
    lead: TimeDelta,
) -> rustykrab_core::Result<()> {
    let store = &state.agent.store;
    let mut open: HashMap<String, Vec<WorkItem>> = HashMap::new();
    for item in store.work_open_items().await? {
        if let Some(job) = job_of(&item) {
            open.entry(job).or_default().push(item);
        }
    }

    // A deleted job's open firing does not run.
    for (job, items) in &open {
        if matches!(
            store.jobs().get_job(job).await,
            Err(rustykrab_core::Error::NotFound(_))
        ) {
            for item in items {
                if let Err(e) = control
                    .cancel(
                        &item.id,
                        Some("its scheduled job was deleted".to_string()),
                        ACTOR,
                    )
                    .await
                {
                    tracing::warn!(item = %item.id, error = %e, "orphaned firing not cancelled");
                }
            }
        }
    }

    for job in store.jobs().get_due_jobs(now + lead).await? {
        if let Some(id) = store.jobs().work_item_id(&job.id).await? {
            if let Some(item) = store.work_get(&id).await? {
                let over = item.status.is_closed() || parked_on_user(&item);
                if !over {
                    // The firing is still going.
                    continue;
                }
                if !finished_with(store, &item.id).await? {
                    finish(state, &job, &item).await?;
                    continue;
                }
            }
        }
        // A firing filed but never linked (a restart between the two):
        // link it rather than file a second.
        let mut adopted = false;
        for item in open.get(&job.id).into_iter().flatten() {
            if !parked_on_user(item) && !finished_with(store, &item.id).await? {
                store.jobs().set_work_item_id(&job.id, &item.id).await?;
                adopted = true;
                break;
            }
        }
        if !adopted {
            file(state, control, &job, now).await?;
        }
    }
    Ok(())
}

fn job_of(item: &WorkItem) -> Option<String> {
    item.artifact_refs
        .iter()
        .find(|r| r.kind == JOB_REF)
        .map(|r| r.value.clone())
}

/// A firing waiting on the user: over, for the schedule.
fn parked_on_user(item: &WorkItem) -> bool {
    matches!(item.status, Status::Blocked(r) if r.needs_user())
}

async fn finished_with(store: &Store, item: &str) -> rustykrab_core::Result<bool> {
    Ok(store
        .work_evidence_list(item)
        .await?
        .iter()
        .any(|e| e.kind == JOB_RUN))
}

/// File the job's next firing: its conversation made ready as a job run
/// always has made it, then one `personal` item with `trigger: at(time)`
/// and the job named in its refs, linked from the job.
async fn file(
    state: &AppState,
    control: &dyn ControlHandle,
    job: &ScheduledJob,
    now: DateTime<Utc>,
) -> rustykrab_core::Result<()> {
    let store = &state.agent.store;
    // An empty task body never ran and never will (an older build stored
    // them): recorded and disabled, as the task-queue path does.
    if job.task.trim().is_empty() {
        tracing::error!(job_id = %job.id, "scheduled job has an empty task body; disabling");
        let _ = store
            .jobs()
            .record_run(
                &job.id,
                "error",
                Some(
                    "scheduled job has an empty task body; disabled. Recreate with a non-empty task.",
                ),
                now,
                now,
            )
            .await;
        store.jobs().set_enabled(&job.id, false).await?;
        return Ok(());
    }
    let mut conv = match task_queue::resume_or_create_conversation(job, state, store).await {
        Ok(conv) => conv,
        Err(e) => {
            tracing::error!(job_id = %job.id, "no conversation for the scheduled firing: {e}");
            let _ = store
                .jobs()
                .record_run(
                    &job.id,
                    "error",
                    Some(&format!("failed to resume conversation: {e}")),
                    now,
                    now,
                )
                .await;
            return Ok(());
        }
    };
    let (channel, chat, thread) = task_queue::resolve_delivery_target(
        job.channel.as_deref(),
        job.chat_id.as_deref(),
        job.thread_id.as_deref(),
        &conv,
    );
    if task_queue::persist_channel_context_onto_conversation(
        &mut conv,
        channel.as_deref(),
        chat.as_deref(),
        thread.as_deref(),
    ) {
        if let Err(e) = store.conversations().save_meta(&conv).await {
            tracing::warn!(job_id = %job.id, "channel context not kept on the job's conversation: {e}");
        }
    }

    let execution = job.execution.clone().unwrap_or_default();
    let mut refs = execution.artifact_refs;
    refs.push(ArtifactRef {
        kind: JOB_REF.into(),
        value: job.id.clone(),
    });
    let draft = WorkItemDraft {
        kind: Some(execution.kind.unwrap_or(WorkKind::Personal)),
        title: format!("Scheduled: {}", one_line(&job.task, 80)),
        objective: job.task.trim().to_string(),
        done_when: execution.done_when.unwrap_or_else(|| {
            "The scheduled task is done, and the result summary holds the full deliverable.".into()
        }),
        artifact_refs: refs,
        worker_kind: execution.worker_kind,
        required_tools: execution.required_tools,
        required_mcp_servers: execution.required_mcp_servers,
        writable_resources: execution.writable_resources,
        constraints: execution.constraints,
        trigger: Trigger::At(job.next_run_at.max(now + LINK_MARGIN)),
        budget: Some(execution.budget.unwrap_or_else(firing_budget)),
        ..WorkItemDraft::default()
    };
    let provenance = Provenance {
        conversation_id: Some(conv.id.to_string()),
        filed_by_item: None,
        actor: ACTOR.to_string(),
    };
    match control.file_draft(draft, provenance).await? {
        PlanOutcome::Accepted(accepted) => {
            store
                .jobs()
                .set_work_item_id(&job.id, &accepted.root)
                .await?;
            tracing::info!(
                job_id = %job.id,
                item = %accepted.root,
                due = %job.next_run_at,
                "scheduled firing filed as a work item"
            );
        }
        PlanOutcome::Rejected(rejected) => {
            tracing::error!(job_id = %job.id, ?rejected, "scheduled firing refused");
        }
    }
    Ok(())
}

/// The firing is over: mark it finished with, record it as the job's run,
/// advance the job, and deliver a `done` result to the job's target.
async fn finish(
    state: &AppState,
    job: &ScheduledJob,
    item: &WorkItem,
) -> rustykrab_core::Result<()> {
    let store = &state.agent.store;
    let evidence = store.work_evidence_list(&item.id).await?;
    let (status, text) = if item.status == Status::Done {
        let summary = evidence
            .iter()
            .rev()
            .find(|e| e.kind == SUMMARY_EVIDENCE)
            .map(|e| e.reference.clone())
            .unwrap_or_else(|| "Scheduled task completed (no summary).".to_string());
        ("ok", summary)
    } else {
        (
            "error",
            format!("the scheduled task's work item ended {}", item.status),
        )
    };
    // The mark comes first: a restart between it and the delivery loses
    // the delivery rather than sending it twice.
    store
        .work_evidence_add(Evidence {
            item: item.id.clone(),
            kind: JOB_RUN.to_string(),
            reference: job.id.clone(),
            hash: None,
            verified_by: None,
            at: Utc::now(),
        })
        .await?;
    let started = store
        .work_lease_history(&item.id)
        .await?
        .first()
        .map(|l| l.lease.since)
        .unwrap_or(item.created_at);
    let finished = item.closed_at.unwrap_or_else(Utc::now);
    if let Err(e) = store
        .jobs()
        .record_run(&job.id, status, Some(&text), started, finished)
        .await
    {
        tracing::warn!(job_id = %job.id, "job run not recorded: {e}");
    }
    if let Err(e) = store.jobs().mark_executed(&job.id).await {
        tracing::error!(job_id = %job.id, "scheduled job not advanced: {e}");
    }
    if item.status != Status::Done {
        return Ok(());
    }
    let conv_id = job
        .conversation_id
        .as_deref()
        .and_then(|c| Uuid::parse_str(c).ok());
    let conv = match conv_id {
        Some(id) => store.conversations().get(id).await.ok(),
        None => None,
    }
    .unwrap_or_else(|| empty_conversation(conv_id.unwrap_or_else(Uuid::nil)));
    let (channel, chat, thread) = task_queue::resolve_delivery_target(
        job.channel.as_deref(),
        job.chat_id.as_deref(),
        job.thread_id.as_deref(),
        &conv,
    );
    task_queue::deliver_response(
        &job.id,
        channel.as_deref(),
        chat.as_deref(),
        thread.as_deref(),
        &text,
        conv.id,
        state,
    )
    .await;
    Ok(())
}

fn empty_conversation(id: Uuid) -> Conversation {
    let now = Utc::now();
    Conversation {
        id,
        messages: Vec::new(),
        created_at: now,
        updated_at: now,
        title: None,
        summary: None,
        detected_profile: None,
        channel_source: None,
        channel_id: None,
        channel_thread_id: None,
    }
}

/// `text` on one line, cut at `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

/// The user turn a firing's run opens with: the scheduled prompt a job run
/// has always had, with delivery said the way a worker delivers, through
/// `result_report`'s summary.
pub(crate) fn firing_prompt(
    task: &str,
    last_run_at: Option<DateTime<Utc>>,
    channel: Option<&str>,
    chat: Option<&str>,
) -> String {
    let delivery = match (channel, chat) {
        (Some(c), Some(id)) => format!(
            "Your result_report summary will be delivered to {c} ({id}) as this task's \
             message: write the briefing or answer the recipient receives in it, in full. \
             Do not ask for clarification, do not promise future updates."
        ),
        _ => "Your result_report summary IS the deliverable for this scheduled task: write \
              the briefing or answer in it, in full. Do not ask for clarification, do not \
              promise future updates."
            .to_string(),
    };
    format!(
        "{}\n\n{}\n\n{delivery}",
        task_queue::scheduled_task_body(task, last_run_at),
        task_queue::SCHEDULED_CONTEXT_RECOVERY
    )
}

// ── the worker's side: runs kept as conversations, firings continued ──

/// Where the local worker keeps its runs: the conversation store, under the
/// controller's run id. A firing's run id is its job's conversation
/// (the controller names it), and [`RunTranscripts::resume`] hands that
/// conversation back to continue, with the job's SKILL.md and prompt.
pub(crate) struct JobTranscripts {
    store: Store,
    skills: Arc<SkillRegistry>,
}

pub(crate) fn transcripts(store: &Store, skills: Arc<SkillRegistry>) -> Arc<dyn RunTranscripts> {
    Arc::new(JobTranscripts {
        store: store.clone(),
        skills,
    })
}

#[async_trait]
impl RunTranscripts for JobTranscripts {
    async fn save(&self, conversation: &Conversation) -> rustykrab_core::Result<()> {
        self.store.conversations().save(conversation).await
    }

    async fn resume(&self, id: Uuid, brief: &Brief) -> rustykrab_core::Result<Option<Resumed>> {
        let Some(job) = self.store.jobs().job_for_work_item(&brief.item).await? else {
            return Ok(None);
        };
        if job.conversation_id.as_deref() != Some(id.to_string().as_str()) {
            return Ok(None);
        }
        let conversation = match self.store.conversations().get(id).await {
            Ok(conversation) => conversation,
            // Gone since filing: the run starts fresh under the job's id.
            Err(rustykrab_core::Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let guidance = task_queue::resolve_skill_for_task(&self.skills, &job.task).map(
            |(name, body)| {
                tracing::info!(job_id = %job.id, skill = %name, "SKILL.md in the scheduled firing's run");
                rustykrab_skills::prompt::SystemPromptBuilder::new()
                    .with_active_skill(&name, &body)
                    .build()
            },
        );
        let (channel, chat, _) = task_queue::resolve_delivery_target(
            job.channel.as_deref(),
            job.chat_id.as_deref(),
            job.thread_id.as_deref(),
            &conversation,
        );
        let preface = firing_prompt(
            &job.task,
            job.last_run_at,
            channel.as_deref(),
            chat.as_deref(),
        );
        Ok(Some(Resumed {
            conversation,
            guidance,
            preface: Some(preface),
        }))
    }
}

#[cfg(test)]
mod tests;
