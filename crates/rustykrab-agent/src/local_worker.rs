//! `LocalWorker`: the `local` worker kind of the control layer (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, section 5).
//!
//! A local worker is a sub-agent conversation on this daemon, run for the
//! controller rather than for a model's `subagents` call. It takes an
//! [`AgentDefinition`] the way [`crate::SubagentRunner`] does (system
//! prompt, harness profile, `allowed_tools` as the ceiling) and runs one
//! [`Brief`] at a time on one KV slot.
//!
//! One run is one conversation:
//!
//! - one leading system message, the definition's prompt, and one user turn,
//!   the brief rendered as typed lines of pointers rather than prose
//!   (sections 6 step 4, 6.3 and 12);
//! - the brief's `required_tools`, the tools of its `required_mcp_servers`
//!   and the `work_*` tools activated in the run's [`ActiveToolsRegistry`]
//!   before the first model call, so the tool block is fixed from turn 0
//!   (section 12; Phase 2 adds `AgentDefinition.tools` for the rest);
//! - [`WORK_RUN_CONTEXT`] bound to the brief's item, which makes the runner
//!   treat the conversation as a worker run: only a successful
//!   `result_report` ends it, text is answered with a counted
//!   `[System notice]` reminder, and no request carries a system message
//!   after the first (section 12.1).
//!
//! `run` returns the [`ResultReport`] the run's `result_report` call handed
//! the injected [`WorkBackend`], exactly as the backend received it. A run
//! that ends without one returns a typed [`RunFailure`] (a spent iteration or
//! wall budget, nothing at all from the model, prose with no report), and a
//! provider error passes through unchanged; the controller reads either with
//! [`rustykrab_control::worker::run_failure_input`].

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use rustykrab_control::errors::{BudgetKind, GapKind, ProviderProblem};
use rustykrab_control::worker::{Brief, RunFailure, Worker, WorkerCapabilities};
use rustykrab_core::active_tools::ActiveToolsRegistry;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::recall::RecallStore;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_core::work::{
    ArtifactRef, PlanOutcome, ResultReport, WorkItemDraft, WorkItemId, WorkerKind,
};
use rustykrab_core::{AgentDefinition, CapabilitySet, Error, Result, Session, Tool};
use rustykrab_tools::work_backend::{
    Principal, Provenance, StatusQuery, ToolState, WorkBackend, WorkRunContext, WorkStatusView,
    WORK_RUN_CONTEXT,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::runner::{AgentConfig, AgentRunner};
use crate::sandbox::Sandbox;
use crate::subagent::profile_for;
use crate::trace::ExecutionTracer;

/// The control-layer tools every worker run holds, built per run over the
/// injected backend. `work_plan` is not among them: only the planner files
/// a graph, and a worker's follow-up work travels as drafts in its report.
const WORK_TOOL_NAMES: [&str; 3] = ["work_file", "work_status", "result_report"];

/// The ordinary conversation's completion signal. A worker never holds it.
const TASK_COMPLETE: &str = "task_complete";

/// Prefix of the tools one MCP server contributes (`mcp__<server>__<tool>`).
const MCP_PREFIX: &str = "mcp__";

/// Characters kept of one line of the brief.
const LINE_MAX: usize = 400;
/// Characters kept of an input's summary: its first line only (plan 6.3).
const SUMMARY_MAX: usize = 160;
/// Pointers listed per list; the rest are counted, not rendered.
const REFS_MAX: usize = 8;
/// Characters of the model's last text quoted in a failure's detail.
const QUOTE_MAX: usize = 200;

/// Where a [`LocalWorker`] keeps its runs' transcripts. A run is a
/// conversation like any other (plan section 16, Phase 1 exit: a task is
/// "completed by a scoped local worker in another" conversation), kept
/// under the id the controller gave the run ([`Brief::run`]), so the item's
/// `run` evidence points at it. The composition root implements this over
/// the conversation store; without one, runs are not kept.
#[async_trait]
pub trait RunTranscripts: Send + Sync {
    /// Store the run's conversation as it stands, replacing an earlier
    /// save of the same id.
    async fn save(&self, conversation: &Conversation) -> Result<()>;
}

/// What one run of a [`LocalWorker`] did, beyond its result: the numbers the
/// controller cannot read from a [`ResultReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRun {
    pub item: WorkItemId,
    /// Model turns the run took.
    pub iterations: u32,
    /// `result_report` reminders the runner sent after a text-only reply.
    pub reminders: u32,
    /// `result_report` calls the host refused; each went back to the model.
    pub rejected_reports: u32,
    /// Whether a report was recorded.
    pub reported: bool,
}

/// The `local` worker kind: a sub-agent conversation on this daemon.
pub struct LocalWorker {
    name: String,
    definition: Arc<AgentDefinition>,
    provider: Arc<dyn ModelProvider>,
    /// The host's tools, minus the ones a worker run builds for itself.
    tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    backend: Arc<dyn WorkBackend>,
    /// Every tool a run may call, sorted: [`Self::ceiling_of`].
    ceiling: Vec<String>,
    recall: Option<Arc<RecallStore>>,
    transcripts: Option<Arc<dyn RunTranscripts>>,
    /// The KV slot runs wait for. Local work is serialised per model
    /// (plan section 12.1), so workers on one model share one.
    slot: Arc<Semaphore>,
    last_run: Mutex<Option<LocalRun>>,
}

impl LocalWorker {
    /// A local worker named `name` (the registry's name for it, "pinch")
    /// running `definition` on `provider`.
    ///
    /// `tools` is the host's catalog; the definition's `allowed_tools`
    /// narrows it, and any `work_file`, `work_status`, `result_report` or
    /// `task_complete` in it is dropped: each run builds its own `work_*`
    /// tools over `backend`, and a worker ends only with its report.
    pub fn new(
        name: impl Into<String>,
        definition: impl Into<Arc<AgentDefinition>>,
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        sandbox: Arc<dyn Sandbox>,
        backend: Arc<dyn WorkBackend>,
    ) -> Self {
        let definition = definition.into();
        let tools: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .filter(|t| !WORK_TOOL_NAMES.contains(&t.name()) && t.name() != TASK_COMPLETE)
            .collect();
        let ceiling = Self::ceiling_of(&definition, &tools);
        Self {
            name: name.into(),
            definition,
            provider,
            tools,
            sandbox,
            backend,
            ceiling,
            recall: None,
            transcripts: None,
            slot: Arc::new(Semaphore::new(1)),
            last_run: Mutex::new(None),
        }
    }

    /// Wait on `slot` instead of a slot of its own, so several local
    /// workers on one model never run at once.
    pub fn with_slot(mut self, slot: Arc<Semaphore>) -> Self {
        self.slot = slot;
        self
    }

    /// Archive what compaction displaces in `store`, so a finished run's
    /// history stays reachable through the recall tools.
    pub fn with_recall_store(mut self, store: Arc<RecallStore>) -> Self {
        self.recall = Some(store);
        self
    }

    /// Keep each run's conversation in `transcripts`: once with the brief
    /// before the first model call, so a run that is cancelled or lost
    /// mid-way still leaves its brief behind, and again when it ends.
    pub fn with_transcripts(mut self, transcripts: Arc<dyn RunTranscripts>) -> Self {
        self.transcripts = Some(transcripts);
        self
    }

    async fn keep(&self, conv: &Conversation) {
        if let Some(sink) = &self.transcripts {
            if let Err(e) = sink.save(conv).await {
                tracing::warn!(
                    worker = %self.name,
                    conversation = %conv.id,
                    error = %e,
                    "worker run transcript not kept"
                );
            }
        }
    }

    /// A definition for a general local worker: the default harness
    /// profile, the host's whole catalog as its ceiling, and a system prompt
    /// that states the worker's contract once, ahead of every brief.
    pub fn default_definition(id: &str) -> AgentDefinition {
        AgentDefinition {
            id: id.to_string(),
            description: "Runs one work item for the controller and reports a typed result.".into(),
            system_prompt: format!(
                "You are {id}, a RustyKrab worker. Each run gives you one work item. Do the \
                 work with your tools, then call result_report once, as your last call: the \
                 run ends only when that call succeeds. Report pointers (paths, URLs, ids), \
                 not content. If you cannot finish, report blocked or error instead of \
                 guessing. Put follow-up work in discovered; do not do it."
            ),
            profile: "default".into(),
            allowed_tools: None,
        }
    }

    /// What the last run did, if one has finished.
    pub fn last_run(&self) -> Option<LocalRun> {
        self.last_run
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Every tool a run may call: the host's tools within the definition's
    /// `allowed_tools` (all of them when it names none) and the `work_*`
    /// tools, less any a session could not call even when granted (the
    /// sub-agent family and computer use need opt-ins a worker never gets).
    fn ceiling_of(definition: &AgentDefinition, tools: &[Arc<dyn Tool>]) -> Vec<String> {
        let allowed = definition.allowed_tools.as_deref();
        let mut names: Vec<String> = WORK_TOOL_NAMES.iter().map(|n| n.to_string()).collect();
        for tool in tools {
            let name = tool.name();
            let permitted = allowed.is_none_or(|a| a.iter().any(|n| n == name));
            if permitted && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        let probe = Self::capabilities_for(&names);
        names.retain(|n| probe.can_use_tool(n));
        names.sort();
        names
    }

    /// The session capabilities of a run: its tools, and the resource
    /// capabilities those tools need to act, as an interactive session has.
    fn capabilities_for(names: &[String]) -> CapabilitySet {
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        CapabilitySet::for_tools_permissive(&names)
    }

    /// The tools to activate before the first model call, or the gap that
    /// stops the run before it starts. The controller matches a brief to a
    /// worker whose capabilities cover it, so a gap here means the two
    /// disagreed; it comes back typed, for the ladder's order 2.
    fn activation(&self, brief: &Brief) -> std::result::Result<Vec<String>, RunFailure> {
        let mut names: Vec<String> = WORK_TOOL_NAMES.iter().map(|n| n.to_string()).collect();
        for tool in &brief.required_tools {
            if !self.ceiling.contains(tool) {
                return Err(RunFailure::Gap {
                    gap: GapKind::Tool,
                    name: tool.clone(),
                });
            }
            names.push(tool.clone());
        }
        for server in &brief.required_mcp_servers {
            let before = names.len();
            names.extend(
                self.ceiling
                    .iter()
                    .filter(|n| mcp_server_of(n).is_some_and(|s| s.eq_ignore_ascii_case(server)))
                    .cloned(),
            );
            if names.len() == before {
                return Err(RunFailure::Gap {
                    gap: GapKind::Tool,
                    name: format!("mcp server {server}"),
                });
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn conversation(&self, id: Uuid, brief: &Brief) -> Conversation {
        let now = Utc::now();
        Conversation {
            id,
            messages: vec![
                Message::stamped(
                    Role::System,
                    MessageContent::Text(self.definition.system_prompt.clone()),
                ),
                Message::stamped(Role::User, MessageContent::Text(render_brief(brief))),
            ],
            created_at: now,
            updated_at: now,
            title: Some(brief.title.clone()),
            summary: None,
            detected_profile: Some(self.definition.profile.clone()),
            channel_source: Some("worker".into()),
            channel_id: Some(self.name.clone()),
            channel_thread_id: Some(brief.item.clone()),
        }
    }
}

#[async_trait]
impl Worker for LocalWorker {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }

    fn capabilities(&self) -> WorkerCapabilities {
        let mut mcp_servers: Vec<String> = self
            .ceiling
            .iter()
            .filter_map(|n| mcp_server_of(n))
            .map(str::to_string)
            .collect();
        mcp_servers.sort();
        mcp_servers.dedup();
        WorkerCapabilities {
            models: vec![self.provider.name().to_string()],
            tools: self.ceiling.clone(),
            mcp_servers,
            // A local run writes through this daemon's own tools, so it can
            // reach any resource they can. Who may write a resource at once
            // is the controller's single-writer rule, not a capability.
            writable_resources: vec!["*".to_string()],
            ..WorkerCapabilities::default()
        }
    }

    /// One KV slot: a second item on the same model would evict the
    /// first's prefix cache (plan section 12.1).
    fn concurrency(&self) -> usize {
        1
    }

    /// A local worker is as available as the daemon it runs in. Its
    /// provider's failures come back from `run` for the ladder to classify
    /// rather than taking the worker out of the registry.
    fn healthy(&self) -> bool {
        true
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        let activate = self.activation(&brief).map_err(RunFailure::into_error)?;
        let _slot = self
            .slot
            .acquire()
            .await
            .map_err(|_| Error::Internal("local worker slot closed".into()))?;

        // Fresh `work_*` tools per run, over a recorder, so the run returns
        // exactly the report the backend accepted and nothing else.
        let recorder = Arc::new(RecordingBackend::new(self.backend.clone()));
        let mut tools = rustykrab_tools::work_tools(recorder.clone());
        tools.extend(self.tools.iter().cloned());

        // The controller's run id, when it is one, names the conversation,
        // so the item's `run` evidence points at this transcript.
        let conv_id = brief
            .run
            .as_deref()
            .and_then(|r| Uuid::parse_str(r).ok())
            .unwrap_or_else(Uuid::new_v4);
        let session = Session::with_capabilities(conv_id, Self::capabilities_for(&self.ceiling));
        let active = Arc::new(ActiveToolsRegistry::new());
        active.activate(conv_id, activate);

        let profile = profile_for(&self.definition);
        let max_iterations = profile
            .max_iterations
            .min(usize::try_from(brief.budget.iterations).unwrap_or(usize::MAX))
            .max(1);
        let config = AgentConfig {
            max_iterations,
            // No budget warnings: the controller owns the budget.
            soft_iteration_warning: 0,
            ..profile.to_agent_config()
        };
        let mut runner = AgentRunner::new(self.provider.clone(), tools, self.sandbox.clone())
            .with_config(config)
            .with_active_tools(active);
        if let Some(recall) = &self.recall {
            runner = runner.with_recall_store(recall.clone());
        }

        let mut conv = self.conversation(conv_id, &brief);
        self.keep(&conv).await;
        let tracer = ExecutionTracer::new();
        let binding = WorkRunContext {
            item: brief.item.clone(),
            actor: format!("worker:{}", self.name),
        };
        let wall = brief.budget.wall_seconds;
        let run = WORK_RUN_CONTEXT.scope(binding, runner.run_traced(&mut conv, &session, &tracer));
        // `None`: the wall budget ran out and the run was dropped mid-step.
        let ended = match wall {
            0 => Some(run.await),
            secs => tokio::time::timeout(Duration::from_secs(secs), run)
                .await
                .ok(),
        };

        self.keep(&conv).await;
        let reports = recorder.take();
        let stats = LocalRun {
            item: brief.item.clone(),
            iterations: tracer.iterations(),
            reminders: tracer.completion_reminders(),
            rejected_reports: tracer
                .tool_stats()
                .get("result_report")
                .map_or(0, |s| s.failures),
            reported: !reports.is_empty(),
        };
        tracing::info!(
            worker = %self.name,
            item = %stats.item,
            iterations = stats.iterations,
            reminders = stats.reminders,
            rejected_reports = stats.rejected_reports,
            reported = stats.reported,
            "local worker run ended"
        );
        *self.last_run.lock().unwrap_or_else(|e| e.into_inner()) = Some(stats.clone());

        let count = reports.len();
        if let Some(first) = reports.into_iter().next() {
            // Two reports in one batch can both be accepted before the run
            // ends; the first is the one the run's final message carries.
            if count > 1 {
                tracing::warn!(
                    worker = %self.name,
                    item = %brief.item,
                    count,
                    "more than one report recorded in one run; returning the first"
                );
            }
            return Ok(first);
        }

        let failure = match ended {
            None => RunFailure::Budget {
                budget: BudgetKind::Wall,
                detail: format!(
                    "{wall}s wall budget spent after {} iterations, before result_report",
                    stats.iterations
                ),
            },
            // A provider or runner error: classified as the core reports it.
            Some(Err(e)) => return Err(e),
            Some(Ok(())) if tracer.iteration_limit_reached() => RunFailure::Budget {
                budget: BudgetKind::Iterations,
                detail: format!(
                    "{max_iterations} iterations used without result_report \
                     ({} reminders)",
                    stats.reminders
                ),
            },
            Some(Ok(())) => match last_assistant_text(&conv) {
                None => RunFailure::Model {
                    problem: ProviderProblem::Empty,
                    detail: format!(
                        "the model stopped with no text and no result_report \
                         ({} reminders)",
                        stats.reminders
                    ),
                },
                Some(text) => RunFailure::Model {
                    problem: ProviderProblem::Format,
                    detail: format!(
                        "the run ended with text and no result_report ({} reminders): {}",
                        stats.reminders,
                        one_line(text, QUOTE_MAX)
                    ),
                },
            },
        };
        Err(failure.into_error())
    }
}

/// The server an MCP tool name belongs to (`mcp__<server>__<tool>`).
fn mcp_server_of(name: &str) -> Option<&str> {
    name.strip_prefix(MCP_PREFIX)?
        .split_once("__")
        .map(|(server, _)| server)
        .filter(|s| !s.is_empty())
}

/// The last non-empty text the model wrote, if any.
fn last_assistant_text(conv: &Conversation) -> Option<&str> {
    conv.messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.content.as_text())
        .find(|t| !t.trim().is_empty())
}

// ── the brief ─────────────────────────────────────────────────────────────

/// Render a brief as the run's one user turn: typed lines, pointers not
/// prose (plan sections 6.3 and 12). Empty fields are left out, inputs carry
/// their closed status, refs and one line of summary, never a transcript,
/// and the last lines say how the run ends.
pub fn render_brief(brief: &Brief) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "work item: #{}  kind: {}",
        brief.item,
        brief.kind.as_str()
    );
    let _ = writeln!(out, "title: {}", one_line(&brief.title, LINE_MAX));
    let _ = writeln!(out, "objective: {}", one_line(&brief.objective, LINE_MAX));
    let _ = writeln!(out, "done_when: {}", one_line(&brief.done_when, LINE_MAX));
    bullets(&mut out, "constraints", &brief.constraints);
    bullets(&mut out, "decisions_made", &brief.decisions_made);
    if !brief.artifact_refs.is_empty() {
        let _ = writeln!(out, "artifact_refs: {}", artifacts(&brief.artifact_refs));
    }
    if !brief.writable_resources.is_empty() {
        let _ = writeln!(
            out,
            "writable_resources: {}",
            list(
                brief
                    .writable_resources
                    .iter()
                    .map(|r| one_line(r, LINE_MAX))
            )
        );
    }
    if !brief.inputs.is_empty() || !brief.more_inputs.is_empty() {
        let _ = writeln!(out, "inputs:");
        for input in &brief.inputs {
            let mut head = format!(
                "  - item: #{} \"{}\"  status: {}",
                input.item,
                one_line(&input.title, LINE_MAX),
                input.status
            );
            if let Some(edge) = input.edge {
                let _ = write!(head, "  edge: {}", edge.as_str());
            }
            let _ = writeln!(out, "{head}");
            let mut detail = Vec::new();
            if !input.evidence.is_empty() {
                detail.push(format!("evidence: {}", artifacts(&input.evidence)));
            }
            if !input.artifacts.is_empty() {
                detail.push(format!("artifacts: {}", artifacts(&input.artifacts)));
            }
            let summary = first_line(&input.summary, SUMMARY_MAX);
            if !summary.is_empty() {
                detail.push(format!("summary: {summary}"));
            }
            if !detail.is_empty() {
                let _ = writeln!(out, "    {}", detail.join("  "));
            }
            if let Some(err) = &input.error {
                let _ = writeln!(
                    out,
                    "    error: {}/{}",
                    err.class.as_str(),
                    err.subclass.as_str()
                );
            }
        }
        if !brief.more_inputs.is_empty() {
            let ids: Vec<String> = brief
                .more_inputs
                .iter()
                .map(|id| format!("#{id}"))
                .collect();
            let _ = writeln!(
                out,
                "  more: {}  (read them with work_status)",
                ids.join(", ")
            );
        }
    }
    if brief.last_error.is_some() || !brief.prior_evidence.is_empty() {
        let _ = writeln!(out, "repair: an earlier attempt at this item failed");
        if let Some(err) = &brief.last_error {
            let _ = writeln!(out, "  last_error: {}", one_line(err, LINE_MAX));
        }
        if !brief.prior_evidence.is_empty() {
            let refs = brief
                .prior_evidence
                .iter()
                .map(|e| one_line(&format!("{}:{}", e.kind, e.reference), LINE_MAX));
            let _ = writeln!(out, "  prior_evidence: {}", list(refs));
        }
    }
    let _ = writeln!(out, "budget: {} steps", brief.budget.iterations);
    if let Some(origin) = &brief.origin_conversation_id {
        let _ = writeln!(out, "origin_conversation: {origin}");
    }
    out.push('\n');
    out.push_str(
        "End this run with one result_report call, as your last call. Put pointers in it \
         (paths, URLs, ids), not content. If you cannot finish, set blocked (what you need) \
         or error (what failed). Follow-up work goes in discovered. Text alone does not end \
         this run.",
    );
    out
}

/// A labelled list of one-line entries, or nothing when it is empty.
fn bullets(out: &mut String, label: &str, entries: &[String]) {
    if entries.is_empty() {
        return;
    }
    let _ = writeln!(out, "{label}:");
    for entry in entries {
        let _ = writeln!(out, "  - {}", one_line(entry, LINE_MAX));
    }
}

fn artifacts(refs: &[ArtifactRef]) -> String {
    list(
        refs.iter()
            .map(|r| one_line(&format!("{}:{}", r.kind, r.value), LINE_MAX)),
    )
}

/// `[a, b, c]`, with at most [`REFS_MAX`] entries and a count of the rest.
fn list(entries: impl Iterator<Item = String>) -> String {
    let all: Vec<String> = entries.collect();
    let mut shown: Vec<String> = all.iter().take(REFS_MAX).cloned().collect();
    if all.len() > REFS_MAX {
        shown.push(format!("+{} more", all.len() - REFS_MAX));
    }
    format!("[{}]", shown.join(", "))
}

/// The first non-blank line of `text`, on one line and cut at `max`.
fn first_line(text: &str, max: usize) -> String {
    one_line(
        text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""),
        max,
    )
}

/// `text` on one line, whitespace collapsed, cut at `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(3)).collect();
    cut.push_str("...");
    cut
}

// ── the recorder ──────────────────────────────────────────────────────────

/// Forwards every call to the injected backend and keeps each report it
/// accepted, so the worker returns the report the controller received. A
/// report the backend refused is not kept: the tool's error goes back to
/// the model and the run goes on.
struct RecordingBackend {
    inner: Arc<dyn WorkBackend>,
    reports: Mutex<Vec<ResultReport>>,
}

impl RecordingBackend {
    fn new(inner: Arc<dyn WorkBackend>) -> Self {
        Self {
            inner,
            reports: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Vec<ResultReport> {
        std::mem::take(&mut *self.reports.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[async_trait]
impl WorkBackend for RecordingBackend {
    async fn file(&self, draft: WorkItemDraft, provenance: Provenance) -> Result<PlanOutcome> {
        self.inner.file(draft, provenance).await
    }

    async fn status(
        &self,
        query: StatusQuery,
        principal: &Principal,
    ) -> Result<Vec<WorkStatusView>> {
        self.inner.status(query, principal).await
    }

    async fn report(
        &self,
        item: WorkItemId,
        report: ResultReport,
        provenance: Provenance,
    ) -> Result<()> {
        self.inner.report(item, report.clone(), provenance).await?;
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(report);
        Ok(())
    }

    fn tool_state(&self, name: &str) -> ToolState {
        self.inner.tool_state(name)
    }

    fn mcp_server_configured(&self, name: &str) -> bool {
        self.inner.mcp_server_configured(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_control::errors::{classify, Context};
    use rustykrab_control::worker::run_failure_input;
    use rustykrab_core::model::ToolChoice;
    use rustykrab_core::model::{ModelResponse, StopReason, Usage};
    use rustykrab_core::types::{ToolCall, ToolSchema};
    use rustykrab_core::work::{
        Budget, EdgeKind, ErrorClass, ErrorSubclass, Evidence, InputRef, Status, WorkError,
        WorkKind,
    };
    use rustykrab_tools::work_backend::with_work_run;
    use rustykrab_tools::{StubWorkBackend, TaskCompleteTool, WorkCall};
    use serde_json::{json, Value};

    use crate::sandbox::NoSandbox;

    /// Plays a script, one response per call, and records each request's
    /// messages and tool names. Past the script it answers as Ollama does
    /// for a generation of zero tokens.
    struct Recording {
        script: Mutex<Vec<ModelResponse>>,
        requests: Mutex<Vec<(Vec<Message>, Vec<String>)>>,
        delay: Option<Duration>,
    }

    impl Recording {
        fn new(script: Vec<ModelResponse>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                requests: Mutex::new(Vec::new()),
                delay: None,
            })
        }

        fn slow(script: Vec<ModelResponse>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                requests: Mutex::new(Vec::new()),
                delay: Some(delay),
            })
        }

        fn requests(&self) -> Vec<(Vec<Message>, Vec<String>)> {
            self.requests.lock().unwrap().clone()
        }

        async fn next(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
            self.requests.lock().unwrap().push((
                messages.to_vec(),
                tools.iter().map(|t| t.name.clone()).collect(),
            ));
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Err(Error::ModelEmptyResponse("scripted silence".into()));
            }
            Ok(script.remove(0))
        }
    }

    #[async_trait]
    impl ModelProvider for Recording {
        fn name(&self) -> &str {
            "scripted-local"
        }
        async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
            self.next(messages, tools).await
        }
        async fn chat_with_choice(
            &self,
            messages: &[Message],
            tools: &[ToolSchema],
            _: ToolChoice,
        ) -> Result<ModelResponse> {
            self.next(messages, tools).await
        }
    }

    /// A tool that does nothing, under any name.
    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.0.into(),
                description: "test tool".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
    }

    /// Records the work item bound to the run it executes in.
    struct Probe(Arc<Mutex<Vec<Option<String>>>>);

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "reports the work item its run holds"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "probe".into(),
                description: "reports the work item its run holds".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            self.0
                .lock()
                .unwrap()
                .push(with_work_run(|r| r.item.clone()));
            Ok(json!({"ok": true}))
        }
    }

    fn respond(content: MessageContent, stop_reason: StopReason) -> ModelResponse {
        ModelResponse {
            message: Message::stamped(Role::Assistant, content),
            usage: Usage::default(),
            stop_reason,
            text: None,
        }
    }

    fn tool_call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            arguments: args,
        }
    }

    fn call(name: &str, args: Value) -> ModelResponse {
        respond(
            MessageContent::ToolCall(tool_call(name, args)),
            StopReason::ToolUse,
        )
    }

    fn batch(calls: Vec<ToolCall>) -> ModelResponse {
        respond(MessageContent::MultiToolCall(calls), StopReason::ToolUse)
    }

    fn text(t: &str) -> ModelResponse {
        respond(MessageContent::Text(t.into()), StopReason::EndTurn)
    }

    fn report(summary: &str) -> ModelResponse {
        call("result_report", json!({ "summary": summary }))
    }

    fn brief(item: &str) -> Brief {
        Brief {
            item: item.into(),
            kind: WorkKind::Personal,
            title: "Compare phone plans".into(),
            objective: "Find the cheapest of plans A, B and C".into(),
            done_when: "The cheapest plan is named with its monthly price".into(),
            constraints: vec!["UK plans only".into()],
            decisions_made: Vec::new(),
            artifact_refs: Vec::new(),
            required_tools: Vec::new(),
            required_mcp_servers: Vec::new(),
            writable_resources: Vec::new(),
            inputs: Vec::new(),
            more_inputs: Vec::new(),
            prior_evidence: Vec::new(),
            last_error: None,
            budget: Budget::default(),
            origin_conversation_id: None,
            run: None,
        }
    }

    fn worker(
        provider: Arc<Recording>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> (LocalWorker, Arc<StubWorkBackend>) {
        let stub = Arc::new(StubWorkBackend::new());
        let worker = LocalWorker::new(
            "pinch",
            LocalWorker::default_definition("pinch"),
            provider,
            tools,
            Arc::new(NoSandbox),
            stub.clone(),
        );
        (worker, stub)
    }

    fn reports(stub: &StubWorkBackend) -> Vec<(WorkItemId, ResultReport, Provenance)> {
        stub.calls()
            .into_iter()
            .filter_map(|c| match c {
                WorkCall::Report {
                    item,
                    report,
                    provenance,
                } => Some((item, report, provenance)),
                _ => None,
            })
            .collect()
    }

    fn classified(err: &Error) -> (ErrorClass, ErrorSubclass) {
        let ctx = Context {
            tool: None,
            worker_kind: Some(WorkerKind::Local),
        };
        let e = classify(&run_failure_input(err), &ctx);
        (e.class, e.subclass)
    }

    #[tokio::test]
    async fn an_accepted_report_ends_the_run_and_is_what_run_returns() {
        let provider = Recording::new(vec![report("B is cheapest at 12 GBP a month.")]);
        let (worker, stub) = worker(provider.clone(), vec![Arc::new(Named("noop"))]);

        let got = worker.run(brief("item-7")).await.unwrap();

        assert_eq!(got.summary, "B is cheapest at 12 GBP a month.");
        assert_eq!(
            provider.requests().len(),
            1,
            "no model call after the report"
        );
        let recorded = reports(&stub);
        assert_eq!(recorded.len(), 1);
        let (item, sent, provenance) = &recorded[0];
        assert_eq!(item, "item-7");
        assert_eq!(sent, &got, "run returns what the backend received");
        assert_eq!(provenance.actor, "worker:pinch");
        assert_eq!(provenance.filed_by_item.as_deref(), Some("item-7"));

        // One leading system message, then the brief as the one user turn.
        let (messages, _) = &provider.requests()[0];
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::System);
        assert!(messages[0]
            .content
            .as_text()
            .unwrap()
            .contains("You are pinch"));
        assert_eq!(messages[1].role, Role::User);
        let rendered = messages[1].content.as_text().unwrap();
        assert!(
            rendered.starts_with("work item: #item-7  kind: personal\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("done_when: The cheapest plan is named"),
            "{rendered}"
        );
        assert!(
            rendered.ends_with("Text alone does not end this run."),
            "{rendered}"
        );

        let run = worker.last_run().unwrap();
        assert_eq!(
            run,
            LocalRun {
                item: "item-7".into(),
                iterations: 1,
                reminders: 0,
                rejected_reports: 0,
                reported: true,
            }
        );
    }

    /// Every save a [`RunTranscripts`] received, in order.
    #[derive(Default)]
    struct Kept(Mutex<Vec<Conversation>>);

    #[async_trait]
    impl RunTranscripts for Kept {
        async fn save(&self, conversation: &Conversation) -> Result<()> {
            self.0.lock().unwrap().push(conversation.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_run_is_kept_as_a_conversation_under_the_controllers_run_id() {
        let provider = Recording::new(vec![report("B is cheapest.")]);
        let (worker, _) = worker(provider, vec![Arc::new(Named("noop"))]);
        let kept = Arc::new(Kept::default());
        let worker = worker.with_transcripts(kept.clone());
        let run = Uuid::new_v4();
        let mut b = brief("item-8");
        b.run = Some(run.to_string());

        worker.run(b).await.unwrap();

        let saves = kept.0.lock().unwrap().clone();
        assert_eq!(saves.len(), 2, "once with the brief, once at the end");
        assert!(saves.iter().all(|c| c.id == run));
        assert_eq!(
            saves[0].messages.len(),
            2,
            "the brief is kept before any call"
        );
        assert_eq!(saves[0].channel_source.as_deref(), Some("worker"));
        assert_eq!(saves[0].channel_thread_id.as_deref(), Some("item-8"));
        let last = &saves[1];
        assert!(last.messages.len() > 2, "the whole run is kept at the end");
        assert!(last.messages.iter().any(|m| matches!(
            &m.content,
            MessageContent::ToolCall(call) if call.name == "result_report"
        )));
    }

    #[tokio::test]
    async fn a_rejected_report_goes_back_to_the_model_and_the_run_goes_on() {
        let provider = Recording::new(vec![
            call(
                "result_report",
                json!({
                    "summary": "failed",
                    "error": { "class": "model", "subclass": "timeout", "detail": "slow" }
                }),
            ),
            report("Done after fixing the report."),
        ]);
        let (worker, stub) = worker(provider.clone(), Vec::new());

        let got = worker.run(brief("item-7")).await.unwrap();

        assert_eq!(got.summary, "Done after fixing the report.");
        assert_eq!(provider.requests().len(), 2);
        let fed_back = provider.requests()[1].0.iter().any(|m| {
            matches!(&m.content, MessageContent::ToolResult(r)
                if r.is_error && r.output.to_string().contains("belongs to class `tool`"))
        });
        assert!(fed_back, "the rejection reached the model as a tool result");
        assert_eq!(
            reports(&stub).len(),
            1,
            "a rejected report never reached the backend"
        );
        assert_eq!(worker.last_run().unwrap().rejected_reports, 1);
    }

    #[tokio::test]
    async fn a_run_that_never_reports_ends_with_a_classifiable_error() {
        // The iteration budget runs out.
        let provider = Recording::new(vec![
            call("noop", json!({})),
            call("noop", json!({})),
            call("noop", json!({})),
        ]);
        let (w, stub) = worker(provider.clone(), vec![Arc::new(Named("noop"))]);
        let mut b = brief("item-1");
        b.budget.iterations = 2;
        let err = w.run(b).await.unwrap_err();
        assert!(
            matches!(
                RunFailure::from_error(&err),
                Some(RunFailure::Budget {
                    budget: BudgetKind::Iterations,
                    ..
                })
            ),
            "{err}"
        );
        assert_eq!(
            classified(&err),
            (ErrorClass::Budget, ErrorSubclass::Iterations)
        );
        assert_eq!(provider.requests().len(), 2);
        assert!(stub.calls().is_empty());

        // Prose to every reminder, until the reminders run out.
        let provider = Recording::new(
            (0..4)
                .map(|i| text(&format!("Plan B, I think ({i}).")))
                .collect(),
        );
        let (w, _) = worker(provider.clone(), Vec::new());
        let err = w.run(brief("item-2")).await.unwrap_err();
        assert_eq!(classified(&err), (ErrorClass::Model, ErrorSubclass::Format));
        assert!(
            err.to_string()
                .contains("(3 reminders): Plan B, I think (3)."),
            "{err}"
        );
        assert_eq!(w.last_run().unwrap().reminders, 3);

        // Blank replies to every reminder.
        let provider = Recording::new((0..4).map(|_| text("  ")).collect());
        let (w, _) = worker(provider, Vec::new());
        let err = w.run(brief("item-3")).await.unwrap_err();
        assert_eq!(classified(&err), (ErrorClass::Model, ErrorSubclass::Empty));

        // The provider returns nothing at all: its own error, unchanged.
        let provider = Recording::new(Vec::new());
        let (w, _) = worker(provider, Vec::new());
        let err = w.run(brief("item-4")).await.unwrap_err();
        assert!(matches!(err, Error::ModelEmptyResponse(_)), "{err:?}");
        assert_eq!(classified(&err), (ErrorClass::Model, ErrorSubclass::Empty));

        // The wall budget runs out mid-call.
        let provider = Recording::slow(vec![report("too late")], Duration::from_secs(5));
        let (w, stub) = worker(provider, Vec::new());
        let mut b = brief("item-5");
        b.budget.wall_seconds = 1;
        let err = w.run(b).await.unwrap_err();
        assert_eq!(classified(&err), (ErrorClass::Budget, ErrorSubclass::Wall));
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn batched_calls_keep_the_work_run_binding() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Recording::new(vec![
            batch((0..3).map(|_| tool_call("probe", json!({}))).collect()),
            // A batched report binds its item the same way.
            batch(vec![
                tool_call("probe", json!({})),
                tool_call("result_report", json!({ "summary": "probed" })),
            ]),
        ]);
        let (w, stub) = worker(provider, vec![Arc::new(Probe(seen.clone()))]);
        let mut b = brief("item-8");
        b.required_tools = vec!["probe".into()];

        let got = w.run(b).await.unwrap();

        assert_eq!(got.summary, "probed");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "every call in both batches ran");
        assert!(
            seen.iter().all(|s| s.as_deref() == Some("item-8")),
            "{seen:?}"
        );
        assert_eq!(reports(&stub)[0].0, "item-8");
    }

    #[tokio::test]
    async fn required_tools_are_visible_from_the_first_prefill() {
        let provider = Recording::new(vec![report("done")]);
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Named("browser")),
            Arc::new(Named("calendar")),
            Arc::new(Named("mcp__linear__create_issue")),
            Arc::new(Named("mcp__linear__search")),
            Arc::new(Named("mcp__jira__search")),
            Arc::new(TaskCompleteTool::new()),
        ];
        let (w, _) = worker(provider.clone(), tools);
        let mut b = brief("item-9");
        b.required_tools = vec!["calendar".into()];
        b.required_mcp_servers = vec!["Linear".into()];

        w.run(b).await.unwrap();

        let (_, mut first) = provider.requests()[0].clone();
        first.sort();
        assert_eq!(
            first,
            [
                "calendar",
                "mcp__linear__create_issue",
                "mcp__linear__search",
                "result_report",
                "work_file",
                "work_status",
            ]
        );

        let caps = w.capabilities();
        assert_eq!(w.name(), "pinch");
        assert_eq!(w.kind(), WorkerKind::Local);
        assert_eq!(w.concurrency(), 1);
        assert!(w.healthy());
        assert_eq!(caps.models, ["scripted-local"]);
        assert_eq!(caps.mcp_servers, ["jira", "linear"]);
        assert!(caps.tools.iter().any(|t| t == "browser"));
        assert!(caps.tools.iter().any(|t| t == "result_report"));
        assert!(!caps.tools.iter().any(|t| t == "task_complete"));
        // Any resource: the controller's single-writer rule serialises
        // writers; a capability that listed none would lease no writer.
        assert_eq!(caps.writable_resources, ["*"]);
    }

    #[tokio::test]
    async fn a_brief_beyond_the_ceiling_is_a_gap_before_any_model_call() {
        let provider = Recording::new(vec![report("never")]);
        let mut definition = LocalWorker::default_definition("krabby");
        definition.allowed_tools = Some(vec!["calendar".into()]);
        let w = LocalWorker::new(
            "krabby",
            definition,
            provider.clone(),
            vec![Arc::new(Named("calendar")), Arc::new(Named("browser"))],
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        );
        assert_eq!(
            w.capabilities().tools,
            ["calendar", "result_report", "work_file", "work_status"]
        );

        let mut b = brief("item-10");
        b.required_tools = vec!["browser".into()];
        let err = w.run(b).await.unwrap_err();
        assert_eq!(
            RunFailure::from_error(&err),
            Some(RunFailure::Gap {
                gap: GapKind::Tool,
                name: "browser".into()
            })
        );
        assert_eq!(
            classified(&err),
            (ErrorClass::CapabilityGap, ErrorSubclass::ToolGap)
        );

        let mut b = brief("item-11");
        b.required_mcp_servers = vec!["linear".into()];
        let err = w.run(b).await.unwrap_err();
        assert!(err.to_string().ends_with("mcp server linear"), "{err}");
        assert!(provider.requests().is_empty());
    }

    /// Scenario 31's shape: the worker keeps its own scratch list, and the
    /// result carries exactly the one draft it reported.
    #[tokio::test]
    async fn only_the_reported_draft_leaves_the_run() {
        let draft = json!({
            "title": "Port the number",
            "objective": "Move the number to plan B",
            "done_when": "The number works on plan B"
        });
        let provider = Recording::new(vec![
            call(
                "todo_write",
                json!({ "todos": [
                    { "content": "scratch: compare roaming fees", "status": "in_progress" },
                    { "content": "scratch: ask about the contract end", "status": "pending" }
                ] }),
            ),
            call(
                "result_report",
                json!({ "summary": "B is cheapest.", "discovered": [draft] }),
            ),
        ]);
        let (w, stub) = worker(provider, crate::todo_tools());
        let mut b = brief("item-12");
        b.required_tools = vec!["todo_write".into()];

        let got = w.run(b).await.unwrap();

        assert_eq!(got.discovered.len(), 1);
        assert_eq!(got.discovered[0].title, "Port the number");
        assert_eq!(got.discovered[0].objective, "Move the number to plan B");
        let whole = serde_json::to_string(&got).unwrap();
        assert!(!whole.contains("scratch"), "scratch state leaked: {whole}");
        let calls = stub.calls();
        assert_eq!(calls.len(), 1, "nothing filed, one report: {calls:?}");
        assert!(matches!(&calls[0], WorkCall::Report { report, .. } if report == &got));
    }

    #[test]
    fn the_brief_is_typed_pointers() {
        let mut b = brief("item-43");
        b.decisions_made = vec!["No contract longer\nthan 12 months".into()];
        b.artifact_refs = vec![ArtifactRef {
            kind: "path".into(),
            value: "notes/plans.md".into(),
        }];
        b.writable_resources = vec!["carrier-account".into()];
        b.inputs = vec![
            InputRef {
                item: "41".into(),
                title: "research plan X".into(),
                status: Status::Done,
                edge: None,
                evidence: vec![ArtifactRef {
                    kind: "url".into(),
                    value: "https://example.com/x".into(),
                }],
                artifacts: Vec::new(),
                summary: "X is 15 GBP a month.\nFull table follows...".into(),
                error: None,
            },
            InputRef {
                item: "42".into(),
                title: "book table at X".into(),
                status: Status::Failed,
                edge: Some(EdgeKind::WaitsFor),
                evidence: Vec::new(),
                artifacts: Vec::new(),
                summary: String::new(),
                error: Some(WorkError {
                    class: ErrorClass::CapabilityGap,
                    subclass: ErrorSubclass::Credential,
                    fingerprint: "f".into(),
                    detail: "no login".into(),
                    artifact_refs: Vec::new(),
                    observed_by: "rule:credential".into(),
                }),
            },
        ];
        b.more_inputs = vec!["44".into(), "45".into()];
        b.last_error = Some("tool/timeout: carrier site timed out".into());
        b.prior_evidence = vec![Evidence {
            item: "item-43".into(),
            kind: "path".into(),
            reference: "notes/attempt-1.md".into(),
            hash: None,
            verified_by: None,
            at: Utc::now(),
        }];
        b.origin_conversation_id = Some("conv-1".into());

        let r = render_brief(&b);
        for line in [
            "decisions_made:\n  - No contract longer than 12 months\n",
            "artifact_refs: [path:notes/plans.md]\n",
            "writable_resources: [carrier-account]\n",
            "inputs:\n  - item: #41 \"research plan X\"  status: done\n",
            "    evidence: [url:https://example.com/x]  summary: X is 15 GBP a month.\n",
            "  - item: #42 \"book table at X\"  status: failed  edge: waits_for\n",
            "    error: capability_gap/credential\n",
            "  more: #44, #45  (read them with work_status)\n",
            "repair: an earlier attempt at this item failed\n",
            "  last_error: tool/timeout: carrier site timed out\n",
            "  prior_evidence: [path:notes/attempt-1.md]\n",
            "budget: 25 steps\n",
            "origin_conversation: conv-1\n",
        ] {
            assert!(r.contains(line), "missing {line:?} in:\n{r}");
        }
        assert!(
            !r.contains("Full table"),
            "only the first line of a summary"
        );

        let plain = render_brief(&brief("item-1"));
        for absent in ["inputs:", "repair:", "artifact_refs:", "decisions_made:"] {
            assert!(!plain.contains(absent), "{absent} in:\n{plain}");
        }
    }

    #[test]
    fn long_lists_and_lines_are_capped() {
        let mut b = brief("item-2");
        b.artifact_refs = (0..12)
            .map(|i| ArtifactRef {
                kind: "path".into(),
                value: format!("f{i}"),
            })
            .collect();
        b.objective = "x".repeat(LINE_MAX + 50);
        let r = render_brief(&b);
        assert!(r.contains("f7, +4 more]"), "{r}");
        let objective = r.lines().find(|l| l.starts_with("objective: ")).unwrap();
        assert_eq!(objective.chars().count(), "objective: ".len() + LINE_MAX);
        assert!(objective.ends_with("..."));
    }
}
