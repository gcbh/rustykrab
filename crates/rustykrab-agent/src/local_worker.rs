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
//!   before the first model call, with the definition's visible set
//!   (`AgentDefinition.tools` and `mcp_servers`), so the tool block is
//!   fixed from turn 0 and anything found later arrives by append
//!   (section 12);
//! - [`WORK_RUN_CONTEXT`] bound to the brief's item, which makes the runner
//!   treat the conversation as a worker run: only a successful
//!   `result_report` ends it, text is answered with a counted
//!   `[System notice]` reminder, and no request carries a system message
//!   after the first (section 12.1);
//! - tools that appeared after the worker was built ([`LateTools`]: a skill
//!   a capability build wrote at run time, section 8 rung 2b) joining the
//!   host's catalog, so a resumed item can activate the tool it waited for;
//! - for a `code` brief with a workspace, the worktree created before the
//!   run and removed after it, with `exec` bound to it, so the run works
//!   and commits in the isolated checkout the controller verifies
//!   (section 5);
//! - what the run spends counted at the provider (`metered.rs`) and kept
//!   for the controller's [`Worker::usage`] (tokens, wall time, iterations
//!   and completion reminders). Tokens are counted, never refused: the
//!   prompt's size is compaction's to manage.
//!
//! A run whose id names a conversation the host keeps (a scheduled job's
//! persistent conversation, [`RunTranscripts::resume`]) continues it
//! instead: the same one leading system message, with the host's guidance
//! (the job's SKILL.md body) appended, any later system message it kept
//! turned into a `[System notice]` user turn, and the brief behind the
//! host's preface as the run's user turn.
//!
//! `run` returns the [`ResultReport`] the run's `result_report` call handed
//! the injected [`WorkBackend`], exactly as the backend received it. A run
//! that ends without one returns a typed [`RunFailure`] (a spent iteration or
//! wall budget, nothing at all from the model, prose with no report), and a
//! provider error passes through unchanged; the controller reads either with
//! [`rustykrab_control::worker::run_failure_input`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use rustykrab_control::capability_proof::OPERATION_PROBE;
use rustykrab_control::errors::{BudgetKind, GapKind, ProviderProblem};
use rustykrab_control::progress::{StepLedger, DEFAULT_STALL_STEPS};
use rustykrab_control::worker::{
    Brief, RunFailure, RunUsage, Worker, WorkerCapabilities, COMMAND_RUN,
};
use rustykrab_core::active_tools::ActiveToolsRegistry;
use rustykrab_core::model::{ModelCheck, ModelProvider};
use rustykrab_core::recall::RecallStore;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_core::work::{
    ArtifactRef, PlanOutcome, ResultReport, WorkItemDraft, WorkItemId, WorkPlan, WorkerKind,
};
use rustykrab_core::{
    mcp_server_of, AgentDefinition, CapabilitySet, Error, Result, SandboxRequirements, Session,
    Tool,
};
use rustykrab_tools::work_backend::{
    AskOutcome, AskRequest, CapabilityAsk, Principal, Provenance, StatusQuery, ToolState,
    WorkBackend, WorkRunContext, WorkStatusView, WORK_RUN_CONTEXT,
};
use tokio::sync::{watch, Semaphore};
use uuid::Uuid;

use crate::metered::{Meter, MeteredProvider};
use crate::runner::{AgentConfig, AgentRunner, NOTICE_PREFIX};
use crate::sandbox::Sandbox;
use crate::subagent::profile_for;
use crate::trace::ExecutionTracer;

/// The control-layer tools every worker run holds, built per run over the
/// injected backend. Every agent can file a graph as well as execute a slice.
const WORK_TOOL_NAMES: [&str; 4] = ["work_file", "work_status", "work_discuss", "result_report"];

/// The work tools declared from a run's first turn: only the report. The
/// rest are in the ceiling and loadable (`tools_list`), as the question
/// tools are: filing follow-ups goes in the report's `discovered`, and the
/// brief carries the item's inputs and discussion. In the front block their
/// schemas were most of what a coding worker was shown on every request,
/// and on 2026-10-14 qwen3.8 spent 55 of its calls on `work_status` and
/// `work_discuss` (50 of them refused) instead of the work.
const FRONT_WORK_TOOLS: [&str; 1] = ["result_report"];

/// A worker's typed questions (plan section 7): in every worker's ceiling,
/// built per run, loadable rather than in the front block, since a report
/// can carry `blocked` and `questions` too.
const QUESTION_TOOL_NAMES: [&str; 2] = ["ask_user", "capability_request"];

/// Graph filing is part of every agent's work protocol.
const PLAN_TOOL: &str = "work_plan";

/// The ordinary conversation's completion signal. A worker never holds it.
const TASK_COMPLETE: &str = "task_complete";

/// Characters kept of one line of the brief.
const LINE_MAX: usize = 400;
/// Characters kept of an input's summary: its first line only (plan 6.3).
const SUMMARY_MAX: usize = 160;
/// Pointers listed per list; the rest are counted, not rendered.
const REFS_MAX: usize = 8;

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

    /// The kept conversation a run under `id` continues instead of starting
    /// fresh, if there is one. The controller names such a conversation as
    /// the run's id only for a scheduled job's firing (the job's persistent
    /// conversation), so the default, that every run starts fresh, is right
    /// for a host without scheduled work. An error fails the run rather than
    /// starting a fresh conversation over the one it could not read.
    async fn resume(&self, _id: Uuid, _brief: &Brief) -> Result<Option<Resumed>> {
        Ok(None)
    }
}

/// Tools that exist only once the daemon is running: the composition root
/// rescans what a capability build may have written (a `SKILL.md` becomes
/// a tool) and hands back every such tool. Asked at the start of each run
/// and by [`LocalWorker::capabilities`].
pub trait LateTools: Send + Sync {
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// A kept conversation a run continues (see [`RunTranscripts::resume`]),
/// with what the host adds for this run.
#[derive(Debug, Clone)]
pub struct Resumed {
    pub conversation: Conversation,
    /// Appended to the definition's system prompt, the one leading system
    /// message: a scheduled job's SKILL.md body.
    pub guidance: Option<String>,
    /// Opens the run's user turn, ahead of the brief: a scheduled job's
    /// prompt, with where its result is delivered.
    pub preface: Option<String>,
}

/// What an ongoing or ended run has spent, until the controller asks
/// ([`Worker::usage`]).
struct Spending {
    meter: Arc<Meter>,
    tracer: Arc<ExecutionTracer>,
    started: Instant,
    /// Set when the run ends; a run stopped mid-way counts until asked.
    wall: Mutex<Option<Duration>>,
}

/// Runs whose spend is kept for the controller, at most. A host that never
/// asks (a test, a caller without a controller) does not grow the map
/// without bound: the oldest go first.
const SPENDING_MAX: usize = 64;

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

/// The local runs in flight in this process, so the daemon can end them on
/// its way down: the in-process counterpart of
/// [`crate::external_worker::RunGroups`]. Each [`LocalWorker`] run joins
/// the set [`LocalWorker::with_runs`] gave it ([`LocalRuns::global`] by
/// default) for as long as it runs, waiting for its slot included.
#[derive(Default)]
pub struct LocalRuns {
    live: Mutex<HashMap<u64, watch::Sender<Option<String>>>>,
    next: std::sync::atomic::AtomicU64,
}

impl LocalRuns {
    /// The set every [`LocalWorker`] joins unless told otherwise.
    pub fn global() -> Arc<LocalRuns> {
        static GLOBAL: std::sync::OnceLock<Arc<LocalRuns>> = std::sync::OnceLock::new();
        GLOBAL
            .get_or_init(|| Arc::new(LocalRuns::default()))
            .clone()
    }

    /// How many runs are in the set now.
    pub fn live(&self) -> usize {
        self.lock().len()
    }

    /// End every run in the set: each stops where it is, removes its
    /// worktree and returns [`RunFailure::Interrupted`] carrying `detail`.
    /// Returns how many were told.
    pub fn interrupt_all(&self, detail: &str) -> usize {
        let live = self.lock();
        for tx in live.values() {
            tx.send_replace(Some(detail.to_string()));
        }
        live.len()
    }

    fn enter(self: &Arc<Self>) -> RunEntry {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = watch::channel(None);
        self.lock().insert(id, tx);
        RunEntry {
            runs: self.clone(),
            id,
            rx,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, watch::Sender<Option<String>>>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One run's membership in a [`LocalRuns`]; dropping it leaves the set.
struct RunEntry {
    runs: Arc<LocalRuns>,
    id: u64,
    rx: watch::Receiver<Option<String>>,
}

impl RunEntry {
    /// Resolves, with the typed error, once the run is interrupted.
    async fn interrupted(&self) -> Error {
        let mut rx = self.rx.clone();
        let told = rx
            .wait_for(Option::is_some)
            .await
            .map(|detail| detail.clone().unwrap_or_default());
        match told {
            Ok(detail) => RunFailure::Interrupted { detail }.into_error(),
            // The sender lives until this entry drops.
            Err(_) => std::future::pending().await,
        }
    }
}

impl Drop for RunEntry {
    fn drop(&mut self) {
        self.runs.lock().remove(&self.id);
    }
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
    /// Where each run's session log is written, `<run id>.jsonl`: the
    /// daemon's `<data dir>/run-logs`, shared with the external workers.
    /// `None`: no file is kept ([`Worker::run_log`] answers `None`).
    run_logs: Option<std::path::PathBuf>,
    late: Option<Arc<dyn LateTools>>,
    /// The KV slot runs wait for. Local work is serialised per model
    /// (plan section 12.1), so workers on one model share one.
    slot: Arc<Semaphore>,
    /// The set its runs join, so shutdown can interrupt them.
    runs: Arc<LocalRuns>,
    /// Steps in a row without new evidence before a run has stalled
    /// (section 6, step 7).
    stall_steps: u32,
    full_reports: bool,
    repository_shell: bool,
    /// Overrides the harness profile's `result_report_reminder`.
    result_report_reminder: Option<bool>,
    /// Paths a run's shell may not read, beside the credential stores
    /// `rustykrab_tools::Confinement` always keeps out of reach.
    private_paths: Vec<std::path::PathBuf>,
    readable_files: Vec<std::path::PathBuf>,
    last_run: Mutex<Option<LocalRun>>,
    /// Spend per run id, for [`Worker::usage`].
    spending: Mutex<HashMap<String, Arc<Spending>>>,
    /// The provider's last answer to [`ModelProvider::check_model`] and
    /// when it was asked; `None` until the first [`Worker::refresh`].
    model: Mutex<Option<(Instant, ModelCheck)>>,
}

/// How often [`Worker::refresh`] asks the provider about its model again.
/// The registry refreshes every few seconds; a model pulled or removed
/// shows within this long.
const MODEL_CHECK_EVERY: Duration = Duration::from_secs(30);

impl LocalWorker {
    fn probe_catalog(
        &self,
        context: &rustykrab_control::capability_proof::ProbeContext,
    ) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
        if context.requirements.read_only_review {
            return (Vec::new(), vec!["result_report".into()]);
        }
        let (mut tools, mut ceiling) = self.catalog();
        // Planning uses stable workspace identity, without allocating a directory
        // on every tick. The producer below requires the actual directory.
        if self.repository_shell && context.requirements.workspace.is_none() {
            let has_files =
                context.requirements.writable_resources.iter().any(|r| {
                    r.starts_with("file:") && self.definition.writable_resources.contains(r)
                });
            let denied = |name: &str| {
                name == "exec"
                    || name == "read" && !has_files && self.readable_files.is_empty()
                    || !has_files && ["edit", "write"].contains(&name)
            };
            tools.retain(|t| !denied(t.name()));
            ceiling.retain(|name| !denied(name));
        }
        if let Some(dir) = &context.bound_dir {
            tools = tools
                .into_iter()
                .map(|t| {
                    if t.name() == "exec" {
                        Arc::new(rustykrab_tools::ExecTool::in_dir(dir.clone())) as Arc<dyn Tool>
                    } else {
                        t
                    }
                })
                .collect();
        }
        (tools, ceiling)
    }

    /// Stable effective scope; actual execution rechecks it and the directory.
    pub fn probe_scope(
        &self,
        context: &rustykrab_control::capability_proof::ProbeContext,
    ) -> rustykrab_control::capability_proof::ProbeScope {
        use rustykrab_control::capability_proof::{
            binding_digest, tool_ceiling_digest, ExecutionPath, ProbeScope,
        };
        let (_, ceiling) = self.probe_catalog(context);
        ProbeScope {
            worker: self.name.clone(),
            path: ExecutionPath::Local,
            policy_digest: binding_digest(&Worker::access_profile(self)),
            ceiling_digest: tool_ceiling_digest(&ceiling),
        }
    }

    /// Host-only producer; the Worker trait's coordinated probe hook delegates
    /// here when integrated. It neither calls a model nor writes a ResultReport.
    pub async fn observe_probe(
        &self,
        request: &rustykrab_control::capability_proof::ProbeRequest,
    ) -> rustykrab_control::capability_proof::OperationProbe {
        use rustykrab_control::capability_proof::ProbeOutcome;
        let unobserved = |why| request.expected.observation(ProbeOutcome::Unobserved, why);
        if !request.valid() || request.scope != self.probe_scope(&request.context) {
            return unobserved("probe request bindings do not match this worker");
        }
        if request.context.requirements.workspace.is_some()
            && request
                .context
                .bound_dir
                .as_ref()
                .is_none_or(|d| !d.is_dir())
        {
            return unobserved("controller-owned execution directory is unavailable");
        }
        // Freeze catalog and effective ceiling together; late tools may change.
        let (tools, ceiling) = self.probe_catalog(&request.context);
        if request.scope.ceiling_digest
            != rustykrab_control::capability_proof::tool_ceiling_digest(&ceiling)
        {
            return unobserved("effective tool ceiling changed while freezing the probe catalog");
        }
        if request
            .context
            .requirements
            .required_tools
            .iter()
            .any(|t| !ceiling.contains(t))
            || request
                .context
                .requirements
                .required_mcp_servers
                .iter()
                .any(|server| {
                    !ceiling
                        .iter()
                        .any(|t| mcp_server_of(t).is_some_and(|s| s.eq_ignore_ascii_case(server)))
                })
            || request
                .context
                .requirements
                .writable_resources
                .iter()
                .filter(|r| r.starts_with("file:"))
                .any(|r| !self.definition.writable_resources.contains(r))
        {
            return unobserved("effective requirements exceed this worker's execution boundary");
        }
        // Includes waiting for the same local model/resource slot as real work.
        let entry = self.runs.enter();
        let observe = async {
            let slot = tokio::select! {
                slot = self.slot.acquire() => slot,
                _ = entry.interrupted() => return unobserved("local worker probe was interrupted"),
            };
            let Ok(_slot) = slot else {
                return unobserved("local worker slot is closed");
            };
            tokio::select! {
                result = crate::capability_probe::observe(request, &tools, &ceiling, &self.sandbox) => result,
                _ = entry.interrupted() => unobserved("local worker probe was interrupted"),
            }
        };
        match tokio::time::timeout(request.deadline, observe).await {
            Ok(probe) => probe,
            Err(_) => unobserved("probe deadline elapsed before an observation completed"),
        }
    }

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
            .filter(|t| {
                !WORK_TOOL_NAMES.contains(&t.name())
                    && !QUESTION_TOOL_NAMES.contains(&t.name())
                    && t.name() != PLAN_TOOL
                    && t.name() != TASK_COMPLETE
            })
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
            run_logs: None,
            late: None,
            slot: Arc::new(Semaphore::new(1)),
            runs: LocalRuns::global(),
            stall_steps: DEFAULT_STALL_STEPS,
            full_reports: false,
            repository_shell: false,
            result_report_reminder: None,
            private_paths: Vec::new(),
            readable_files: Vec::new(),
            last_run: Mutex::new(None),
            spending: Mutex::new(HashMap::new()),
            model: Mutex::new(None),
        }
    }

    /// Add exact operator-delegated input files, without write authority.
    pub fn with_readable_files(mut self, files: &[String]) -> Self {
        self.readable_files = files.iter().map(std::path::PathBuf::from).collect();
        self
    }

    /// Add operator-delegated exact files to this worker's existing write scope.
    pub fn with_writable_files(mut self, files: &[String]) -> Self {
        let mut definition = (*self.definition).clone();
        for file in files {
            let resource = format!("file:{file}");
            if !definition.writable_resources.contains(&resource) {
                definition.writable_resources.push(resource);
            }
        }
        self.definition = Arc::new(definition);
        self
    }

    /// Accept complete bounded report content for a host-owned deliverable.
    /// Default workers continue to use compact result summaries.
    pub fn with_full_reports(mut self) -> Self {
        self.full_reports = true;
        self
    }

    /// Only expose shell execution inside a controller-owned repository workspace.
    pub fn with_repository_shell(mut self) -> Self {
        self.repository_shell = true;
        self
    }

    /// Keep `paths` out of a run's shell, which is confined to its worktree
    /// (`rustykrab_tools::Confinement`): the daemon's data directory, where
    /// its secrets file is.
    pub fn with_private_paths(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.private_paths.extend(paths);
        self
    }

    /// Whether a run that ends with text is first reminded to call
    /// `result_report`. Either way its text is then taken as the answer.
    pub fn with_result_report_reminder(mut self, remind: bool) -> Self {
        self.result_report_reminder = Some(remind);
        self
    }

    /// Call a run stalled after `steps` tool calls in a row that bring no
    /// new evidence (plan section 6, step 7).
    pub fn with_stall_steps(mut self, steps: u32) -> Self {
        self.stall_steps = steps.max(1);
        self
    }

    /// The controller planner's embedded file definition. The host may
    /// substitute a data-dir work-planner.md using the same definition loader.
    pub fn planner_definition() -> AgentDefinition {
        let definition = rustykrab_skills::agents::builtin("work-planner")
            .expect("the embedded work-planner definition exists");
        Self::named_definition(&definition, "planner")
    }

    /// Wait on `slot` instead of a slot of its own, so several local
    /// workers on one model never run at once.
    pub fn with_slot(mut self, slot: Arc<Semaphore>) -> Self {
        self.slot = slot;
        self
    }

    /// Join `runs` instead of [`LocalRuns::global`].
    pub fn with_runs(mut self, runs: Arc<LocalRuns>) -> Self {
        self.runs = runs;
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

    /// Leave each run's whole session as a file in `dir`, `<run id>.jsonl`
    /// (the daemon's `<data dir>/run-logs`): a marker line, then one JSON
    /// message per line, readable by its owner only. Written with the brief
    /// before the first model call and again when the run ends, so a run
    /// that fails, stalls or is cancelled still leaves what it had. The
    /// controller records the file as the item's `run_log` evidence
    /// ([`Worker::run_log`]), which is what an investigation of a failed
    /// run opens: the conversation store keeps the same messages, but an
    /// external worker cannot read the store.
    pub fn with_run_logs(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.run_logs = Some(dir.into());
        self
    }

    /// The session log file of `run`, when this worker keeps logs.
    fn log_path(&self, run: &str) -> Option<std::path::PathBuf> {
        self.run_logs
            .as_ref()
            .map(|dir| dir.join(format!("{}.jsonl", crate::external_worker::file_id(run))))
    }

    /// Start the session log of `run` with `conv` as it stands, when this
    /// worker keeps logs.
    fn start_log(&self, run: &str, conv: &Conversation) -> Option<Arc<RunLog>> {
        let path = self.log_path(run)?;
        let header = serde_json::json!({
            "type": "rustykrab.invocation",
            "invocation": "local",
            "run": run,
            "worker": self.name,
            "kind": WorkerKind::Local.as_str(),
            "model": self.provider.name(),
            "conversation": conv.id,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        let log = Arc::new(RunLog {
            path,
            header,
            worker: self.name.clone(),
            messages: Mutex::new(Vec::new()),
        });
        for message in &conv.messages {
            log.note(message);
        }
        log.rewrite();
        Some(log)
    }

    /// Add the tools `late` hands back to the catalog of every run.
    pub fn with_late_tools(mut self, late: Arc<dyn LateTools>) -> Self {
        self.late = Some(late);
        self
    }

    /// The host's tools with the late ones not already among them, and
    /// the ceiling over both.
    fn catalog(&self) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
        let Some(late) = &self.late else {
            return (self.tools.clone(), self.ceiling.clone());
        };
        let mut tools = self.tools.clone();
        for tool in late.tools() {
            let name = tool.name();
            let reserved = WORK_TOOL_NAMES.contains(&name) || name == TASK_COMPLETE;
            if !reserved && !tools.iter().any(|t| t.name() == name) {
                tools.push(tool);
            }
        }
        let ceiling = Self::ceiling_of(&self.definition, &tools);
        (tools, ceiling)
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

    /// The definition of a general local worker named `id`: the built-in
    /// `worker` definition file (`rustykrab-skills/agents/worker.md`), with
    /// the default harness profile, the host's whole catalog as its ceiling,
    /// and a system prompt that states the worker's contract once, ahead of
    /// every brief. A daemon that loads `<data dir>/agents/worker.md` passes
    /// that one through [`Self::named_definition`] instead.
    pub fn default_definition(id: &str) -> AgentDefinition {
        let worker = rustykrab_skills::builtin_agent("worker")
            .expect("the built-in worker definition is embedded in the binary");
        Self::named_definition(&worker, id)
    }

    /// `definition` for the worker named `name`: its id becomes the name,
    /// and `{name}` in its system prompt is replaced with it.
    pub fn named_definition(definition: &AgentDefinition, name: &str) -> AgentDefinition {
        AgentDefinition {
            id: name.to_string(),
            system_prompt: definition.system_prompt.replace("{name}", name),
            ..definition.clone()
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
    /// `allowed_tools` (all of them when it names none) plus its protocol:
    /// ordinary work/report/questions, or plan/status for a planner.
    /// Excludes any a session could not call even when granted (the
    /// sub-agent family and computer use need opt-ins a worker never gets).
    fn ceiling_of(definition: &AgentDefinition, tools: &[Arc<dyn Tool>]) -> Vec<String> {
        let allowed = definition.allowed_tools.as_deref();
        let mut names: Vec<String> = WORK_TOOL_NAMES
            .iter()
            .chain(QUESTION_TOOL_NAMES.iter())
            .map(|n| n.to_string())
            .collect();
        names.push(PLAN_TOOL.into());
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
    /// stops the run before it starts: the `work_*` tools, the definition's
    /// visible set (its `tools` and its `mcp_servers`' tools, within the
    /// ceiling), and the brief's `required_tools` and required servers. The
    /// ceiling is the run's: the host's tools and the late ones. The
    /// controller matches a brief to a worker whose capabilities cover it,
    /// so a gap here means the two disagreed; it comes back typed, for the
    /// ladder's order 2.
    fn activation(
        &self,
        brief: &Brief,
        ceiling: &[String],
    ) -> std::result::Result<Vec<String>, RunFailure> {
        let mut names: Vec<String> = FRONT_WORK_TOOLS.iter().map(|n| n.to_string()).collect();
        // The brief names inputs it does not inline: their reader comes with it.
        if !brief.more_inputs.is_empty() {
            names.push("work_status".into());
        }
        names.extend(
            self.definition
                .visible_set(ceiling.iter().map(String::as_str)),
        );
        for tool in &brief.required_tools {
            if !ceiling.contains(tool) {
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
                ceiling
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
                Message::stamped(
                    Role::User,
                    MessageContent::Text(render_brief_for_report(brief, self.full_reports)),
                ),
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

    /// A kept conversation, continued by this run: one leading system
    /// message (the definition's prompt with the host's guidance), every
    /// later system message the conversation kept as a `[System notice]`
    /// user turn (section 12.1), then the brief as the run's user turn,
    /// behind the host's preface. Its channel fields stay the conversation's
    /// own.
    fn continued(&self, resumed: Resumed, brief: &Brief) -> Conversation {
        let mut conv = resumed.conversation;
        let mut system = self.definition.system_prompt.clone();
        if let Some(guidance) = resumed.guidance.filter(|g| !g.trim().is_empty()) {
            system.push_str("\n\n");
            system.push_str(&guidance);
        }
        let lead = conv
            .messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        conv.messages.drain(..lead);
        for message in conv.messages.iter_mut() {
            if message.role == Role::System {
                if let Some(text) = message.content.as_text() {
                    message.content = MessageContent::Text(format!("{NOTICE_PREFIX}{text}"));
                }
                message.role = Role::User;
            }
        }
        conv.messages.insert(
            0,
            Message::stamped(Role::System, MessageContent::Text(system)),
        );
        let turn = match resumed.preface.filter(|p| !p.trim().is_empty()) {
            Some(preface) => format!(
                "{preface}\n\n{}",
                render_brief_for_report(brief, self.full_reports)
            ),
            None => render_brief_for_report(brief, self.full_reports),
        };
        conv.messages
            .push(Message::stamped(Role::User, MessageContent::Text(turn)));
        conv.updated_at = Utc::now();
        conv
    }

    /// Keep `spending` for `run` until the controller asks, dropping the
    /// oldest past [`SPENDING_MAX`].
    fn track(&self, run: &str, spending: Arc<Spending>) {
        let mut map = self.spending.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= SPENDING_MAX {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, s)| s.started)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(run.to_string(), spending);
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

    fn access_profile(&self) -> serde_json::Value {
        let caps = self.capabilities();
        serde_json::json!({
            "capabilities": {
                "models": rustykrab_control::capability_proof::sorted(&caps.models),
                "tools": rustykrab_control::capability_proof::sorted(&caps.tools),
                "mcp_servers": rustykrab_control::capability_proof::sorted(&caps.mcp_servers),
                "repos": rustykrab_control::capability_proof::sorted(&caps.repos),
                "writable_resources": rustykrab_control::capability_proof::sorted(&caps.writable_resources),
                "machine": caps.machine,
            },
            "repository_shell": self.repository_shell,
            "readable_files": self.readable_files.iter().collect::<std::collections::BTreeSet<_>>(),
        })
    }

    fn probe_scope(
        &self,
        context: &rustykrab_control::capability_proof::ProbeContext,
    ) -> Option<rustykrab_control::capability_proof::ProbeScope> {
        Some(LocalWorker::probe_scope(self, context))
    }

    async fn probe(
        &self,
        request: &rustykrab_control::capability_proof::ProbeRequest,
    ) -> rustykrab_control::capability_proof::OperationProbe {
        self.observe_probe(request).await
    }

    fn planning_only(&self) -> bool {
        self.definition.planning_only
    }

    fn capabilities(&self) -> WorkerCapabilities {
        let (tools, ceiling) = self.catalog();
        let can_act = tools.iter().any(|tool| {
            ceiling.iter().any(|name| name == tool.name())
                && tool.sandbox_requirements().has_side_effects()
        });
        let mut mcp_servers: Vec<String> = ceiling
            .iter()
            .filter_map(|n| mcp_server_of(n))
            .map(str::to_string)
            .collect();
        mcp_servers.sort();
        mcp_servers.dedup();
        WorkerCapabilities {
            models: vec![self.provider.name().to_string()],
            tools: ceiling.clone(),
            mcp_servers,
            // A local run writes through this daemon's own tools, so it can
            // reach any resource they can, unless its definition names the
            // ones it may. Who may write a resource at once is the
            // controller's single-writer rule, not a capability.
            writable_resources: if self.definition.writable_resources.is_empty() {
                if can_act {
                    vec!["*".to_string()]
                } else {
                    Vec::new()
                }
            } else {
                self.definition.writable_resources.clone()
            },
            ..WorkerCapabilities::default()
        }
    }

    /// One KV slot: a second item on the same model would evict the
    /// first's prefix cache (plan section 12.1).
    fn concurrency(&self) -> usize {
        1
    }

    /// A local worker is as available as the daemon it runs in, unless its
    /// provider's server said at the last [`Worker::refresh`] that the
    /// model does not exist: then no item could run, and none is leased.
    /// Other provider failures come back from `run` for the ladder to
    /// classify rather than taking the worker out of the registry.
    fn healthy(&self) -> bool {
        !matches!(
            *self.model.lock().unwrap_or_else(|e| e.into_inner()),
            Some((_, ModelCheck::Missing(_)))
        )
    }

    /// Why the worker is unhealthy, for its health line: the provider's own
    /// account of the missing model.
    fn unhealthy_reason(&self) -> Option<String> {
        match &*self.model.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((_, ModelCheck::Missing(why))) => Some(why.clone()),
            _ => None,
        }
    }

    /// Ask the provider whether its model exists (the registry does at
    /// registration and on its timer), at most once per
    /// [`MODEL_CHECK_EVERY`]. Returns `false` when it asked too recently.
    /// A server that cannot say (`Unknown`: unreachable, or an unexpected
    /// status) leaves the last answer standing, so a missing model stays
    /// missing through an outage and is asked about again when due.
    async fn refresh(&self) -> bool {
        let due = self
            .model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= MODEL_CHECK_EVERY);
        if !due {
            return false;
        }
        let check = self.provider.check_model().await;
        let mut model = self.model.lock().unwrap_or_else(|e| e.into_inner());
        let was_missing = matches!(*model, Some((_, ModelCheck::Missing(_))));
        let check = match check {
            ModelCheck::Missing(why) => {
                if !was_missing {
                    tracing::warn!(worker = %self.name, %why, "local worker unhealthy: its model is missing");
                }
                ModelCheck::Missing(why)
            }
            ModelCheck::Available => {
                if was_missing {
                    tracing::info!(worker = %self.name, "local worker healthy again: its model is back");
                }
                ModelCheck::Available
            }
            ModelCheck::Unknown => match model.take() {
                Some((_, kept @ ModelCheck::Missing(_))) => kept,
                _ => ModelCheck::Unknown,
            },
        };
        *model = Some((Instant::now(), check));
        true
    }

    /// The run's tokens (every model call it made, counted at the
    /// provider), wall time, iterations and completion reminders, taken
    /// once. A run the controller stopped counts up to where it stopped.
    fn usage(&self, run: &str) -> Option<RunUsage> {
        let spending = self
            .spending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run)?;
        let wall = spending
            .wall
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or_else(|| spending.started.elapsed());
        Some(RunUsage {
            tokens: spending.meter.used(),
            wall_ms: u64::try_from(wall.as_millis()).unwrap_or(u64::MAX),
            iterations: spending.tracer.iterations(),
            reminders: spending.tracer.completion_reminders(),
        })
    }

    /// The session log [`LocalWorker::with_run_logs`] keeps for the run.
    fn run_log(&self, run: &str) -> Option<std::path::PathBuf> {
        self.log_path(run).filter(|path| path.is_file())
    }

    fn isolates_worktree(&self) -> bool {
        self.repository_shell
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        let review = is_read_only_review(&brief);
        if review
            && (brief.workspace.is_some()
                || !brief.writable_resources.is_empty()
                || !brief.required_mcp_servers.is_empty()
                || brief
                    .required_tools
                    .iter()
                    .any(|name| name != "result_report"))
        {
            return Err(Error::Auth(
                "read-only review requests execution or writable scope".into(),
            ));
        }
        let (mut host_tools, mut ceiling) = if review {
            (Vec::new(), vec!["result_report".to_string()])
        } else {
            self.catalog()
        };
        let files: Vec<std::path::PathBuf> = brief
            .writable_resources
            .iter()
            .filter(|r| self.definition.writable_resources.contains(r))
            .filter_map(|r| r.strip_prefix("file:").map(std::path::PathBuf::from))
            .collect();
        if brief
            .writable_resources
            .iter()
            .filter(|r| r.starts_with("file:"))
            .count()
            != files.len()
        {
            return Err(Error::Auth("run requests an undelegated file write".into()));
        }
        if self.repository_shell && brief.workspace.is_none() {
            let denied = |name: &str| {
                name == "exec"
                    || name == "read" && files.is_empty() && self.readable_files.is_empty()
                    || files.is_empty() && ["edit", "write"].contains(&name)
            };
            host_tools.retain(|t| !denied(t.name()));
            ceiling.retain(|t| !denied(t));
        }
        let mut activate = if review {
            ceiling.clone()
        } else {
            self.activation(&brief, &ceiling)
                .map_err(RunFailure::into_error)?
        };
        if !files.is_empty() || !self.readable_files.is_empty() {
            for name in ["read", "edit", "write"] {
                if ceiling.iter().any(|n| n == name) && !activate.iter().any(|n| n == name) {
                    activate.push(name.into());
                }
            }
        }
        let entry = self.runs.enter();
        let _slot = tokio::select! {
            slot = self.slot.acquire() => {
                slot.map_err(|_| Error::Internal("local worker slot closed".into()))?
            }
            e = entry.interrupted() => return Err(e),
        };

        if !files.is_empty() || !self.readable_files.is_empty() {
            host_tools = host_tools
                .into_iter()
                .map(|inner| {
                    if ["read", "edit", "write"].contains(&inner.name()) {
                        Arc::new(crate::file_scope::ScopedFileTool {
                            inner,
                            workspace: brief.workspace.as_ref().map(|w| w.path.clone()),
                            files: files.clone(),
                            readable_files: self.readable_files.clone(),
                        }) as Arc<dyn Tool>
                    } else {
                        inner
                    }
                })
                .collect();
        }

        // A `code` run works in its own worktree: created now, `exec`
        // bound to it, removed when the run ends (the branch stays).
        if let Some(ws) = brief.workspace.clone() {
            tokio::task::spawn_blocking(move || ws.create())
                .await
                .map_err(|e| Error::Internal(e.to_string()))?
                .map_err(|why| {
                    RunFailure::Process {
                        code: Some(128),
                        stderr_tail: why,
                    }
                    .into_error()
                })?;
        }
        // Shutdown interrupts the run where it is; the worktree still goes.
        let result = tokio::select! {
            result = self.run_in(brief.clone(), host_tools, ceiling, activate) => result,
            e = entry.interrupted() => {
                tracing::info!(worker = %self.name, "local run interrupted");
                Err(e)
            }
        };
        if let Some(ws) = brief.workspace {
            if let Err(why) = tokio::task::spawn_blocking(move || ws.remove())
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            {
                tracing::warn!(worker = %self.name, %why, "worktree not removed");
            }
        }
        result
    }
}

impl LocalWorker {
    async fn run_in(
        &self,
        brief: Brief,
        host_tools: Vec<Arc<dyn Tool>>,
        ceiling: Vec<String>,
        activate: Vec<String>,
    ) -> Result<ResultReport> {
        // Fresh `work_*` tools per run, over a recorder, so the run returns
        // exactly the report the backend accepted and nothing else.
        let review = is_read_only_review(&brief);
        let mut backend = RecordingBackend::new(self.backend.clone());
        backend.read_only = review;
        let recorder = Arc::new(backend);
        let mut tools = rustykrab_tools::work_tools(recorder.clone());
        if self.full_reports {
            tools.retain(|tool| tool.name() != "result_report");
            tools.push(Arc::new(
                rustykrab_tools::ResultReportTool::for_full_report(recorder.clone()),
            ));
        }
        tools.extend(rustykrab_tools::question_tools(recorder.clone()));
        tools.extend(rustykrab_tools::plan_tools(recorder.clone()));
        let workdir = brief.workspace.as_ref().map(|ws| ws.path.clone());
        let exact_files = brief
            .writable_resources
            .iter()
            .any(|r| r.starts_with("file:"))
            || !self.readable_files.is_empty();
        // A run in a worktree works there and nowhere else: its shell and
        // its file tools are bound to that directory, where the host's own
        // are bound to the daemon's workspace.
        tools.extend(host_tools.into_iter().map(|t| {
            match &workdir {
                Some(_) if exact_files && ["read", "edit", "write"].contains(&t.name()) => t,
                Some(dir) => {
                    match t.name() {
                        "exec" => Arc::new(
                            rustykrab_tools::ExecTool::in_dir(dir.clone())
                                .confined(self.private_paths.clone()),
                        ) as Arc<dyn Tool>,
                        "read" => Arc::new(rustykrab_tools::ReadTool::in_dir(dir.clone()))
                            as Arc<dyn Tool>,
                        "edit" => Arc::new(rustykrab_tools::EditTool::in_dir(dir.clone()))
                            as Arc<dyn Tool>,
                        "write" => Arc::new(rustykrab_tools::WriteTool::in_dir(dir.clone()))
                            as Arc<dyn Tool>,
                        _ => t,
                    }
                }
                None => t,
            }
        }));
        if review {
            tools.retain(|tool| tool.name() == "result_report");
        }
        // Every call is a step in the progress ledger (section 6, step 7).
        let ledger = Arc::new(Mutex::new(StepLedger::new(self.stall_steps)));
        let tools: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|inner| {
                Arc::new(LedgerTool {
                    inner,
                    ledger: ledger.clone(),
                    commands: recorder.commands.clone(),
                }) as Arc<dyn Tool>
            })
            .collect();

        // The controller's run id, when it is one, names the conversation,
        // so the item's `run` evidence points at this transcript.
        let conv_id = brief
            .run
            .as_deref()
            .and_then(|r| Uuid::parse_str(r).ok())
            .unwrap_or_else(Uuid::new_v4);
        let session = Session::with_capabilities(conv_id, Self::capabilities_for(&ceiling));
        let active = Arc::new(ActiveToolsRegistry::new());
        active.activate(conv_id, activate);
        // Kept to read the run's tool gaps once it ends.
        let registry = active.clone();

        // What the run spends, kept for the controller whatever happens to it.
        let spending = Arc::new(Spending {
            meter: Arc::new(Meter::new()),
            tracer: Arc::new(ExecutionTracer::new()),
            started: Instant::now(),
            wall: Mutex::new(None),
        });
        self.track(
            brief.run.as_deref().unwrap_or(&conv_id.to_string()),
            spending.clone(),
        );
        let provider: Arc<dyn ModelProvider> = Arc::new(MeteredProvider::new(
            self.provider.clone(),
            spending.meter.clone(),
        ));

        let profile = profile_for(&self.definition, self.provider.as_ref());
        let max_iterations = profile
            .max_iterations
            .min(usize::try_from(brief.budget.iterations).unwrap_or(usize::MAX))
            .max(1);
        let config = AgentConfig {
            max_iterations,
            // No budget warnings: the controller owns the budget.
            soft_iteration_warning: 0,
            result_report_reminder: self
                .result_report_reminder
                .unwrap_or(profile.result_report_reminder),
            ..profile.to_agent_config()
        };
        let mut runner = AgentRunner::new(provider, tools, self.sandbox.clone())
            .with_config(config)
            .with_active_tools(active);
        if let Some(recall) = self.recall.as_ref().filter(|_| !review) {
            runner = runner.with_recall_store(recall.clone());
        }

        // A kept conversation the run continues (a scheduled job's), else a
        // fresh one.
        let resumed = match &self.transcripts {
            Some(t) => t.resume(conv_id, &brief).await?,
            None => None,
        };
        let mut conv = match resumed {
            Some(resumed) => self.continued(resumed, &brief),
            None => self.conversation(conv_id, &brief),
        };
        self.keep(&conv).await;
        // The session log goes by the id the controller asks for.
        let log_id = brief.run.clone().unwrap_or_else(|| conv_id.to_string());
        if let Some(dir) = &self.run_logs {
            crate::external_worker::prune_files(dir, crate::external_worker::RUN_LOG_RETENTION);
        }
        let log = self.start_log(&log_id, &conv);
        if let Some(log) = &log {
            let log = log.clone();
            runner = runner.with_on_message(Arc::new(move |m: &Message| log.record(m)));
        }
        let tracer = spending.tracer.clone();
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
        let stalled = ledger.lock().unwrap_or_else(|e| e.into_inner()).stalled();

        *spending.wall.lock().unwrap_or_else(|e| e.into_inner()) = Some(spending.started.elapsed());
        self.keep(&conv).await;
        if let Some(log) = &log {
            log.finish(&conv);
        }
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
            coerced_args = tracer.arg_coercions(),
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

        // The host told the model no tool provides a need it kept searching
        // for (`tools_list`), and the run ended without a report: that gap,
        // typed, is why, whatever the run did after it, and the ladder's
        // order 2 is what can resolve it (plan section 8).
        if let Some(need) = registry.tool_gaps(conv_id).into_iter().next() {
            tracing::warn!(
                worker = %self.name,
                item = %brief.item,
                need = %need,
                "run ended on a tool gap without a report"
            );
            return Err(RunFailure::Gap {
                gap: GapKind::Tool,
                name: need,
            }
            .into_error());
        }

        // A run that stopped on text gave its answer (feather's rule): the
        // text is the report, filed through the backend `result_report`
        // uses, so attestation and the controller see it the same way.
        if matches!(ended, Some(Ok(())))
            && !tracer.iteration_limit_reached()
            && !tracer.ended_by_tool()
        {
            if let Some(text) = last_assistant_text(&conv) {
                let provenance = Provenance {
                    conversation_id: Some(conv_id.to_string()),
                    filed_by_item: Some(brief.item.clone()),
                    actor: format!("worker:{}", self.name),
                };
                let report = report_from_text(text, self.full_reports);
                recorder
                    .report(brief.item.clone(), report, provenance)
                    .await?;
                if let Some(report) = recorder.take().into_iter().next() {
                    return Ok(report);
                }
            }
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
            // Told it was repeating itself, the run went on to its cap: the
            // loop is why, and the repair run gets the ledger's evidence.
            Some(Ok(())) if tracer.iteration_limit_reached() => match stalled {
                Some(stall) => RunFailure::Model {
                    problem: ProviderProblem::Loop,
                    detail: format!(
                        "{max_iterations} iterations used without result_report; {}",
                        stall.detail()
                    ),
                },
                None => RunFailure::Budget {
                    budget: BudgetKind::Iterations,
                    detail: format!(
                        "{max_iterations} iterations used without result_report \
                         ({} reminders)",
                        stats.reminders
                    ),
                },
            },
            Some(Ok(())) => RunFailure::Model {
                problem: ProviderProblem::Empty,
                detail: format!(
                    "the model stopped with no text and no result_report \
                     ({} reminders)",
                    stats.reminders
                ),
            },
        };
        Err(failure.into_error())
    }
}

/// The report a run's final text stands for: the result contract the text
/// carries, as Claude Code and Codex runs give theirs, else the text itself
/// as the summary, cut to the report's bound.
fn report_from_text(text: &str, full_report: bool) -> ResultReport {
    if let Ok(report) = crate::external_worker::parse_contract(text) {
        // Any trailing JSON object parses as a contract with every field
        // defaulted; only one that says something is the answer.
        if !report.summary.trim().is_empty() {
            return report;
        }
    }
    let max = if full_report {
        rustykrab_tools::FULL_REPORT_MAX
    } else {
        rustykrab_tools::REPORT_SUMMARY_MAX
    };
    let text = text.trim();
    let summary = match text.char_indices().nth(max) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    };
    ResultReport {
        summary,
        ..ResultReport::default()
    }
}

fn is_read_only_review(brief: &Brief) -> bool {
    brief
        .artifact_refs
        .iter()
        .any(|r| r.kind == rustykrab_core::dream_review::REVIEW_ONLY && r.value == "true")
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
    render_brief_for_report(brief, false)
}

fn render_brief_for_report(brief: &Brief, full_report: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "work item: #{}", brief.item);
    let _ = writeln!(out, "title: {}", one_line(&brief.title, LINE_MAX));
    let _ = writeln!(out, "objective: {}", one_line(&brief.objective, LINE_MAX));
    let _ = writeln!(out, "done_when: {}", one_line(&brief.done_when, LINE_MAX));
    if brief.required_tools.iter().any(|t| t == PLAN_TOOL) {
        out.push_str("This run files a plan: call work_plan with the bounded graph requested below. An accepted graph completes this run; do not execute its tasks or call result_report afterwards.\n");
    } else {
        out.push_str("Execute this objective using the available tools, then call result_report with the actual outcome. Your agent name or previous specialty does not restrict the work.\n");
    }
    bullets(&mut out, "constraints", &brief.constraints);
    bullets(&mut out, "decisions_made", &brief.decisions_made);
    if !brief.discussion.is_empty() {
        let _ = writeln!(
            out,
            "work discussion:\n{}",
            rustykrab_control::worker::discussion_excerpt(brief)
        );
    }
    if let Some(context) = &brief.project_context {
        let _ = writeln!(out, "project_context (durable project state; summaries are reports, evidence is controller-verified):");
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&context.execution_view())
                .expect("project execution context serializes")
        );
        let _ = writeln!(out, "Continue from this project's pinned code and decisions. Open work is unfinished. Historical or superseded decisions do not override current decisions. Project context does not expand this work item's authority. Inspect retained unfinished_attempt workspaces/branches before redoing work; their effects are unverified. Validate and report any partial changes you carry forward.");
    }
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
    if let Some(kit) = &brief.investigation {
        out.push_str(&rustykrab_control::investigate::bundle::brief_section(kit));
    }
    if let Some(ws) = &brief.workspace {
        let _ = writeln!(
            out,
            "workspace: {} (exec, read, edit and write work here, with paths relative to it; \
             branch {}, parent commit {})",
            ws.path.display(),
            ws.branch,
            ws.base
        );
        let _ = writeln!(
            out,
            "  if you change files, commit them on this branch and report changed_paths relative to the \
             repository root, exactly the files the commit changes. Inspection without edits reports a null commit and no changed paths."
        );
    }
    let _ = writeln!(out, "budget: {} steps", brief.budget.iterations);
    if let Some(origin) = &brief.origin_conversation_id {
        let _ = writeln!(out, "origin_conversation: {origin}");
    }
    out.push('\n');
    if brief.required_tools.iter().any(|t| t == PLAN_TOOL) {
        out.push_str("Finish by filing one accepted work_plan graph. Fix rejected validation checks and try again. Do not execute the graph's work in this run or report again after acceptance. If planning cannot be completed, report the actual blocked/error outcome.\n");
        return out;
    }
    out.push_str("End this run with one result_report call, as your last call. ");
    if full_report {
        out.push_str("Put the complete deliverable in summary, including source links and coverage limits, within the report tool's stated bound. ");
    } else {
        out.push_str("The summary becomes a notification that links to the detailed execution. Write one or two sentences stating the outcome or what needs the user's attention. Keep supporting detail in artifacts (paths, URLs, ids), changed_paths and checks_run. ");
    }
    out.push_str("If you cannot finish, set blocked or error (what failed). ");
    out.push_str(&rustykrab_core::work::BLOCKED_SHAPE_GUIDANCE);
    // How to shape a draft (tmp names, edges, inputs_from) is the report
    // tool's schema to say, on the one run in many that files one; every
    // brief used to carry it as a paragraph.
    out.push_str(" Finish this item's done_when and stop there: further work you find goes in the report's discovered drafts, for another run. Do not claim an unfinished done_when as complete.");
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

// ── the session log ──────────────────────────────────────────────────────

/// A run's session log: every message the run ever held, in the order it
/// came, behind one marker line. Compaction replaces a conversation's
/// history with a summary, and a log written from the conversation as the
/// run ended lost everything before it: a run in the 2026-10-14 reminder
/// eval kept 11 of its 25 tool calls. Each message the runner adds is
/// appended as it comes (`AgentRunner::with_on_message`), and the file is
/// rewritten once at the end with the marker's count. Best effort: a log
/// that cannot be written is logged and the run goes on.
struct RunLog {
    path: std::path::PathBuf,
    header: serde_json::Value,
    worker: String,
    messages: Mutex<Vec<Message>>,
}

impl RunLog {
    /// Keep `message` unless the log already has it; whether it was new.
    fn note(&self, message: &Message) -> bool {
        let mut messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        if messages.iter().any(|m| m.id == message.id) {
            return false;
        }
        messages.push(message.clone());
        true
    }

    /// A message the runner added: kept, and appended to the file.
    fn record(&self, message: &Message) {
        if !self.note(message) {
            return;
        }
        self.write(
            |file| {
                use std::io::Write as _;
                let line = serde_json::to_string(message).map_err(std::io::Error::other)?;
                writeln!(file, "{line}")
            },
            false,
        );
    }

    /// The run ended: anything in `conv` the log has not seen (reshaped
    /// before the first turn), then the whole file with its count.
    fn finish(&self, conv: &Conversation) {
        for message in &conv.messages {
            self.note(message);
        }
        self.rewrite();
    }

    fn rewrite(&self) {
        let messages = self
            .messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut header = self.header.clone();
        header["messages"] = serde_json::json!(messages.len());
        self.write(
            |file| {
                use std::io::Write as _;
                writeln!(file, "{header}")?;
                for message in &messages {
                    let line = serde_json::to_string(message).map_err(std::io::Error::other)?;
                    writeln!(file, "{line}")?;
                }
                Ok(())
            },
            true,
        );
    }

    fn write(
        &self,
        body: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()>,
        replace: bool,
    ) {
        let written = (|| -> std::io::Result<()> {
            use std::io::Write as _;
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.create(true);
            if replace {
                options.write(true).truncate(true);
            } else {
                options.append(true);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = std::io::BufWriter::new(options.open(&self.path)?);
            body(&mut file)?;
            file.flush()
        })();
        if let Err(e) = written {
            tracing::warn!(
                worker = %self.worker,
                path = %self.path.display(),
                error = %e,
                "session log not written"
            );
        }
    }
}

// ── the progress ledger ───────────────────────────────────────────────────

/// A tool as a run sees it, with every call recorded as a step in the run's
/// progress ledger (plan section 6, step 7). Everything else delegates.
struct LedgerTool {
    inner: Arc<dyn Tool>,
    ledger: Arc<Mutex<StepLedger>>,
    commands: Arc<Mutex<Vec<String>>>,
}

/// What a call that repeats an earlier call and its result carries back,
/// once the ledger calls the run stalled: the run goes on, told so.
fn repeat_notice(stall: &rustykrab_control::progress::Stall) -> String {
    format!(
        "Note: this is call {} in a row that repeats an earlier call with the same result, so \
         it tells you nothing new. Change the approach, or finish with what you have.",
        stall.without_progress
    )
}

#[async_trait]
impl Tool for LedgerTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn schema(&self) -> rustykrab_core::types::ToolSchema {
        self.inner.schema()
    }

    fn available(&self) -> bool {
        self.inner.available()
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        self.inner.sandbox_requirements()
    }

    fn blocks_turn(&self) -> bool {
        self.inner.blocks_turn()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value> {
        let result = self.inner.execute(args.clone()).await;
        // Only a host-dispatched exec with a process result attests a command.
        // Rejected/tool-error calls and model-authored artifacts are not proof.
        if self.inner.name() == "exec"
            && result.as_ref().is_ok_and(|value| {
                value
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .is_some()
            })
        {
            if let Some(command) = args.get("command").and_then(serde_json::Value::as_str) {
                self.commands
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(command.to_string());
            }
        }
        let outcome = match &result {
            Ok(v) => v.to_string(),
            Err(e) => format!("error: {e}"),
        };
        let stalled = {
            let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
            ledger.record(self.inner.name(), &args, &outcome);
            ledger.stalled()
        };
        // A repeat is answered as text the model can act on, never by ending
        // the run: the iteration cap is the only stop (feather's rule).
        let Some(stall) = stalled else {
            return result;
        };
        let notice = repeat_notice(&stall);
        match result {
            Ok(serde_json::Value::Object(mut object)) => {
                object.insert("notice".into(), serde_json::Value::String(notice));
                Ok(serde_json::Value::Object(object))
            }
            Ok(other) => Ok(serde_json::json!({ "result": other, "notice": notice })),
            Err(Error::ToolExecution(error)) => {
                Err(Error::ToolExecution(rustykrab_core::ToolError {
                    kind: error.kind,
                    message: format!("{}\n\n{notice}", error.message),
                }))
            }
            Err(other) => Err(other),
        }
    }
}

// ── the recorder ──────────────────────────────────────────────────────────

/// Forwards every call to the injected backend and keeps each report it
/// accepted, so the worker returns the report the controller received. A
/// report the backend refused is not kept: the tool's error goes back to
/// the model and the run goes on.
struct RecordingBackend {
    inner: Arc<dyn WorkBackend>,
    reports: Mutex<Vec<ResultReport>>,
    commands: Arc<Mutex<Vec<String>>>,
    read_only: bool,
}

impl RecordingBackend {
    fn new(inner: Arc<dyn WorkBackend>) -> Self {
        Self {
            inner,
            reports: Mutex::new(Vec::new()),
            commands: Arc::new(Mutex::new(Vec::new())),
            read_only: false,
        }
    }

    fn take(&self) -> Vec<ResultReport> {
        std::mem::take(&mut *self.reports.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[async_trait]
impl WorkBackend for RecordingBackend {
    async fn comment(
        &self,
        item: &str,
        text: &str,
        provenance: rustykrab_tools::work_backend::Provenance,
    ) -> rustykrab_core::Result<rustykrab_core::work::WorkEvent> {
        self.inner.comment(item, text, provenance).await
    }
    async fn discussion(
        &self,
        item: &str,
        offset: usize,
        limit: usize,
        principal: &rustykrab_tools::work_backend::Principal,
    ) -> rustykrab_core::Result<rustykrab_tools::work_discuss::WorkDiscussion> {
        self.inner.discussion(item, offset, limit, principal).await
    }

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
        // A report is the sole local review tool, not an alternate route to
        // file work, ask questions, install classifiers or claim execution.
        if self.read_only
            && (!report.artifacts.is_empty()
                || !report.changed_paths.is_empty()
                || report.commit.is_some()
                || !report.checks_run.is_empty()
                || report.blocked.is_some()
                || !report.questions.is_empty()
                || !report.discovered.is_empty())
        {
            return Err(Error::Auth(
                "read-only review may report only its summary, known limits and failure".into(),
            ));
        }
        let mut report = report;
        report
            .artifacts
            .retain(|artifact| artifact.kind != COMMAND_RUN && artifact.kind != OPERATION_PROBE);
        report.artifacts.extend(
            self.commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|command| ArtifactRef {
                    kind: COMMAND_RUN.into(),
                    value: command.clone(),
                }),
        );
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

    async fn plan(&self, plan: WorkPlan, provenance: Provenance) -> Result<PlanOutcome> {
        self.inner.plan(plan, provenance).await
    }

    async fn ask(
        &self,
        item: WorkItemId,
        request: AskRequest,
        provenance: Provenance,
    ) -> Result<AskOutcome> {
        self.inner.ask(item, request, provenance).await
    }

    async fn request_capability(
        &self,
        item: WorkItemId,
        request: CapabilityAsk,
        provenance: Provenance,
    ) -> Result<AskOutcome> {
        self.inner
            .request_capability(item, request, provenance)
            .await
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
            discussion: Vec::new(),
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
            workspace: None,
            capability: None,
            project_context: None,
            investigation: None,
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

    struct ProbeResources {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        output: Value,
        delay: Duration,
    }
    #[async_trait]
    impl Tool for ProbeResources {
        fn name(&self) -> &str {
            "work_resources"
        }
        fn description(&self) -> &str {
            "fixture resources"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name().into(),
                description: self.description().into(),
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(self.output.clone())
        }
    }
    fn probe_request(
        worker: &LocalWorker,
        action: rustykrab_control::capability_proof::RegisteredAction,
    ) -> rustykrab_control::capability_proof::ProbeRequest {
        use rustykrab_control::capability_proof::*;
        let context = ProbeContext {
            requirements: EffectiveRequirements {
                kind: rustykrab_core::work::WorkKind::Task,
                required_tools: vec![],
                required_mcp_servers: vec![],
                writable_resources: vec![],
                read_only_review: false,
                workspace: None,
            },
            bound_dir: None,
        };
        let scope = worker.probe_scope(&context);
        ProbeRequest {
            expected: ExpectedOperation {
                operation: action.operation(),
                executor: scope.worker.clone(),
                inputs_digest: probe_inputs_digest(&action, &scope, &context),
                generation: "fixture:2".into(),
                run: "fixture-probe".into(),
            },
            action,
            scope,
            context,
            deadline: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn worker_trait_dispatch_observes_local_operation_and_exposes_effective_access() {
        use rustykrab_control::capability_proof::*;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resources = Arc::new(ProbeResources {
            calls: calls.clone(),
            output: json!({"workers":[]}),
            delay: Duration::ZERO,
        });
        let (worker, _) = worker(Recording::new(vec![]), vec![resources]);
        let before = Worker::access_profile(&worker);
        let worker = worker
            .with_repository_shell()
            .with_readable_files(&["/fixture/authorized-input".into()]);
        let profile = Worker::access_profile(&worker);
        assert_ne!(before, profile);
        assert_eq!(
            profile["readable_files"],
            json!(["/fixture/authorized-input"])
        );
        assert_eq!(profile["repository_shell"], true);
        let request = probe_request(
            &worker,
            RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: json!({}),
            },
        );
        let worker: Arc<dyn Worker> = Arc::new(worker);
        assert_eq!(
            worker.probe_scope(&request.context),
            Some(request.scope.clone())
        );
        assert_eq!(worker.probe(&request).await.outcome, ProbeOutcome::Ok);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn host_probe_calls_registered_tool_without_model_or_private_output() {
        use rustykrab_control::capability_proof::*;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resources = Arc::new(ProbeResources {
            calls: calls.clone(),
            output: json!({"workers":[],"private":"secret-output"}),
            delay: Duration::ZERO,
        });
        let (worker, backend) = worker(Recording::new(vec![]), vec![resources]);
        let request = probe_request(
            &worker,
            RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: json!({}),
            },
        );
        let observed = worker.observe_probe(&request).await;
        assert_eq!(observed.outcome, ProbeOutcome::Ok);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!serde_json::to_string(&observed)
            .unwrap()
            .contains("secret-output"));
        assert!(
            backend.calls().is_empty(),
            "a probe never forwards a model ResultReport"
        );
    }

    #[tokio::test]
    async fn host_probe_rejects_unregistered_args_changed_ceiling_and_other_executor_without_calls()
    {
        use rustykrab_control::capability_proof::*;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resources = Arc::new(ProbeResources {
            calls: calls.clone(),
            output: json!({"workers":[]}),
            delay: Duration::ZERO,
        });
        let (worker, _) = worker(Recording::new(vec![]), vec![resources]);
        let request = probe_request(
            &worker,
            RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: json!({}),
            },
        );
        for case in 0..4 {
            let mut changed = request.clone();
            match case {
                0 => {
                    changed.action = RegisteredAction::ToolCallable {
                        tool: "work_resources".into(),
                        args: json!({"project_id":"other"}),
                    }
                }
                1 => changed.scope.ceiling_digest = tool_ceiling_digest(&[]),
                2 => {
                    changed.scope.worker = "other".into();
                    changed.expected.executor = "other".into();
                }
                _ => {
                    changed.action = RegisteredAction::BinaryAvailable {
                        binary: "cargo; touch /tmp/forged".into(),
                    }
                }
            }
            changed.expected.operation = changed.action.operation();
            changed.expected.inputs_digest =
                probe_inputs_digest(&changed.action, &changed.scope, &changed.context);
            assert_eq!(
                worker.observe_probe(&changed).await.outcome,
                ProbeOutcome::Unobserved
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn host_probe_deadline_includes_tool_and_existing_shared_slot() {
        use rustykrab_control::capability_proof::*;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resources = Arc::new(ProbeResources {
            calls: calls.clone(),
            output: json!({"workers":[]}),
            delay: Duration::from_secs(60),
        });
        let (worker, _) = worker(Recording::new(vec![]), vec![resources]);
        let mut request = probe_request(
            &worker,
            RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: json!({}),
            },
        );
        request.deadline = Duration::from_millis(20);
        let held = worker.slot.acquire().await.unwrap();
        assert_eq!(
            worker.observe_probe(&request).await.outcome,
            ProbeOutcome::Unobserved
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(held);
        assert_eq!(
            worker.observe_probe(&request).await.outcome,
            ProbeOutcome::Unobserved
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            worker.slot.available_permits(),
            1,
            "timed-out observation releases the resource slot"
        );
    }

    #[tokio::test]
    async fn host_probe_exec_exercises_real_binary_in_bound_directory_and_respects_repository_shell(
    ) {
        use rustykrab_control::capability_proof::*;
        let dir = tempfile::tempdir().unwrap();
        let (worker, _) = worker(
            Recording::new(vec![]),
            vec![Arc::new(rustykrab_tools::ExecTool::new())],
        );
        let worker = worker.with_repository_shell();
        let mut request = probe_request(
            &worker,
            RegisteredAction::BinaryAvailable {
                binary: "node".into(),
            },
        );
        assert_eq!(
            worker.observe_probe(&request).await.outcome,
            ProbeOutcome::Unobserved
        );
        request.context.bound_dir = Some(dir.path().to_path_buf());
        request.context.requirements.workspace = Some(WorkspaceIdentity {
            repo: "/fixture".into(),
            base: "base".into(),
        });
        request.scope = worker.probe_scope(&request.context);
        request.expected.inputs_digest =
            probe_inputs_digest(&request.action, &request.scope, &request.context);
        assert_eq!(
            worker.observe_probe(&request).await.outcome,
            ProbeOutcome::Ok
        );
        let old = dir.path().to_path_buf();
        dir.close().unwrap();
        assert_eq!(request.context.bound_dir, Some(old));
        assert_eq!(
            worker.observe_probe(&request).await.outcome,
            ProbeOutcome::Unobserved
        );
    }

    struct DenyProbe;
    #[async_trait]
    impl Sandbox for DenyProbe {
        async fn execute(
            &self,
            _: &str,
            _: &Value,
            _: &SandboxRequirements,
            _: &crate::sandbox::SandboxPolicy,
        ) -> Result<()> {
            Err(Error::Auth("fixture denied".into()))
        }
    }
    #[tokio::test]
    async fn host_probe_uses_sandbox_and_requires_resource_schema() {
        use rustykrab_control::capability_proof::*;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for deny in [true, false] {
            let resources = Arc::new(ProbeResources {
                calls: calls.clone(),
                output: json!({"error":"secret failure"}),
                delay: Duration::ZERO,
            });
            let (mut worker, _) = worker(Recording::new(vec![]), vec![resources]);
            if deny {
                worker.sandbox = Arc::new(DenyProbe);
            }
            let request = probe_request(
                &worker,
                RegisteredAction::ToolCallable {
                    tool: "work_resources".into(),
                    args: json!({}),
                },
            );
            let observed = worker.observe_probe(&request).await;
            assert_eq!(observed.outcome, ProbeOutcome::Failed);
            assert!(!observed.observation.contains("secret failure"));
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "denied sandbox must prevent invocation"
        );
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
        assert!(rendered.starts_with("work item: #item-7\n"), "{rendered}");
        assert!(
            rendered.contains("done_when: The cheapest plan is named"),
            "{rendered}"
        );
        assert!(
            rendered.ends_with("Do not claim an unfinished done_when as complete."),
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

    #[test]
    fn a_fix_items_brief_says_where_its_investigation_kit_is() {
        let mut b = brief("item-11");
        assert!(!render_brief(&b).contains("investigation:"));
        let dir = std::path::PathBuf::from("/data/investigations/item-run");
        b.investigation = Some(rustykrab_control::investigate::bundle::Kit {
            fingerprint: "0123456789abcdef".into(),
            index: dir.join("index.md"),
            bundle: dir.join("bundle.json"),
            read_dirs: vec![dir.clone()],
            dir,
            source: None,
            own_source: false,
            items: 2,
            runs: 3,
            failed_runs: 3,
            earlier_fixes: Vec::new(),
        });
        let rendered = render_brief(&b);
        assert!(
            rendered.contains("investigation: this item investigates failure 0123456789abcdef"),
            "{rendered}"
        );
        assert!(rendered.contains("/data/investigations/item-run/index.md"));
        assert!(rendered.contains("/data/investigations/item-run/bundle.json"));
        assert!(rendered.contains("a portion at a time"));
    }

    /// The lines of a session log, each parsed as JSON.
    fn log_lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is one JSON value"))
            .collect()
    }

    /// Compaction swaps the history for a summary; the log keeps both.
    #[test]
    fn the_session_log_keeps_what_compaction_took_out_of_context() {
        let dir = tempfile::tempdir().unwrap();
        let text = |role, t: &str| Message::stamped(role, MessageContent::Text(t.into()));
        let brief = text(Role::User, "work item: #item-40");
        let early = text(Role::Assistant, "early exploration");
        let mut conv = Conversation {
            id: Uuid::new_v4(),
            messages: vec![text(Role::System, "you are pinch"), brief.clone()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            title: None,
            summary: None,
            detected_profile: None,
            channel_source: None,
            channel_id: None,
            channel_thread_id: None,
        };
        let log = RunLog {
            path: dir.path().join("run.jsonl"),
            header: json!({ "type": "rustykrab.invocation" }),
            worker: "pinch".into(),
            messages: Mutex::new(Vec::new()),
        };
        for m in &conv.messages {
            log.note(m);
        }
        log.rewrite();
        log.record(&early);
        let summary = text(Role::Assistant, "summary of the early work");
        log.record(&summary);
        // Compacted: the early turn is gone from the conversation.
        conv.messages = vec![conv.messages[0].clone(), brief, summary];
        log.finish(&conv);

        let lines = log_lines(&log.path);
        assert_eq!(lines[0]["messages"], 4);
        assert_eq!(lines.len(), 5);
        let all = lines.iter().map(Value::to_string).collect::<String>();
        assert!(all.contains("early exploration"), "{all}");
        assert!(all.contains("summary of the early work"), "{all}");
    }

    #[tokio::test]
    async fn a_run_leaves_its_session_log_as_a_file_named_through_run_log() {
        let provider = Recording::new(vec![report("B is cheapest.")]);
        let (worker, _) = worker(provider, vec![Arc::new(Named("noop"))]);
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("run-logs");
        let worker = worker.with_run_logs(&logs);
        let run = Uuid::new_v4().to_string();
        assert_eq!(worker.run_log(&run), None, "no file before the run");
        let mut b = brief("item-8");
        b.run = Some(run.clone());

        worker.run(b).await.unwrap();

        let path = worker.run_log(&run).expect("the run left a session log");
        assert_eq!(path, logs.join(format!("{run}.jsonl")));
        let lines = log_lines(&path);
        assert_eq!(lines[0]["type"], "rustykrab.invocation");
        assert_eq!(lines[0]["run"], run.as_str());
        assert_eq!(lines[0]["worker"], "pinch");
        assert_eq!(lines[0]["kind"], "local");
        let messages = &lines[1..];
        assert_eq!(lines[0]["messages"], messages.len());
        assert!(messages.len() > 2, "the whole run, not only the brief");
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[1].to_string().contains("work item: #item-8"),
            "the brief is the first user turn: {}",
            messages[1]
        );
        assert!(
            messages
                .iter()
                .any(|m| m.to_string().contains("result_report")),
            "the report call is in the log"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "readable by its owner only");
        }
    }

    /// A run that ends without a report is the one an investigation opens.
    #[tokio::test]
    async fn a_failed_run_leaves_its_session_log_too() {
        let provider = Recording::new(vec![text("  ")]);
        let (worker, _) = worker(provider, Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let worker = worker.with_run_logs(dir.path());
        let run = Uuid::new_v4().to_string();
        let mut b = brief("item-9");
        b.run = Some(run.clone());

        worker.run(b).await.unwrap_err();

        let lines = log_lines(&worker.run_log(&run).expect("a failed run has a log"));
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("work item: #item-9")),
            "the brief the run failed on is kept"
        );
    }

    #[tokio::test]
    async fn a_worker_without_a_log_directory_names_no_log() {
        let provider = Recording::new(vec![report("B is cheapest.")]);
        let (worker, _) = worker(provider, Vec::new());
        let run = Uuid::new_v4().to_string();
        let mut b = brief("item-10");
        b.run = Some(run.clone());
        worker.run(b).await.unwrap();
        assert_eq!(worker.run_log(&run), None);
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

        // Prose to every reminder: once they run out, the prose is the answer.
        let provider = Recording::new(
            (0..4)
                .map(|i| text(&format!("Plan B, I think ({i}).")))
                .collect(),
        );
        let (w, stub) = worker(provider.clone(), Vec::new());
        let w = w.with_result_report_reminder(true);
        let answered = w.run(brief("item-2")).await.unwrap();
        assert_eq!(answered.summary, "Plan B, I think (3).");
        assert_eq!(w.last_run().unwrap().reminders, 3);
        assert!(
            stub.calls()
                .iter()
                .any(|c| matches!(c, WorkCall::Report { .. })),
            "the text answer is filed like a result_report: {:?}",
            stub.calls()
        );

        // Blank replies to every reminder.
        let provider = Recording::new((0..4).map(|_| text("  ")).collect());
        let (w, _) = worker(provider, Vec::new());
        let w = w.with_result_report_reminder(true);
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

    /// Plan section 8, order 2: a worker that keeps searching for a tool
    /// the catalog lacks is told it is final, and a run that then ends
    /// without a report fails as the typed gap, whatever came after it
    /// (more prose, or the model going silent), so the ladder can acquire
    /// or build the tool.
    #[tokio::test]
    async fn a_need_no_tool_provides_ends_the_run_as_a_tool_gap() {
        let search = || call("tools_list", json!({ "query": "current weather" }));
        let tools = || -> Vec<Arc<dyn Tool>> {
            vec![
                Arc::new(rustykrab_tools::ToolsListTool::new()),
                Arc::new(Named("get_forecast")),
            ]
        };
        let mut script = vec![search(), search(), search()];
        script.extend((0..4).map(|_| text("No tool can tell the current weather.")));
        let provider = Recording::new(script);
        let (w, stub) = worker(provider.clone(), tools());

        let err = w.run(brief("item-9")).await.unwrap_err();

        assert_eq!(
            RunFailure::from_error(&err),
            Some(RunFailure::Gap {
                gap: GapKind::Tool,
                name: "current weather".into()
            }),
            "{err}"
        );
        assert_eq!(
            classified(&err),
            (ErrorClass::CapabilityGap, ErrorSubclass::ToolGap)
        );
        assert!(stub.calls().is_empty());
        // The third search told the worker to report the gap, typed.
        let told = provider.requests()[3]
            .0
            .iter()
            .rev()
            .find_map(|m| match &m.content {
                MessageContent::ToolResult(r) => r.output.as_str().map(str::to_string),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            told.starts_with("No tool provides \"current weather\".")
                && told.contains("\"needs_tool\""),
            "{told}"
        );

        // Silence after it: still the gap, not the provider's error.
        let provider = Recording::new(vec![search(), search(), search()]);
        let (w, _) = worker(provider, tools());
        let err = w.run(brief("item-10")).await.unwrap_err();
        assert_eq!(
            classified(&err),
            (ErrorClass::CapabilityGap, ErrorSubclass::ToolGap),
            "{err}"
        );
    }

    fn review_brief() -> Brief {
        let mut b = brief("review-fixture");
        b.artifact_refs.push(ArtifactRef {
            kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
            value: "true".into(),
        });
        b
    }
    #[tokio::test]
    async fn read_only_local_review_cannot_write_or_file_work_but_returns_report() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("must-not-exist");
        let provider = Recording::new(vec![
            batch(vec![
                tool_call(
                    "write",
                    json!({"path":target.display().to_string(),"content":"unexpected mutation"}),
                ),
                tool_call(
                    "work_file",
                    json!({"title":"Unauthorized discovered work","objective":"execute a new task","done_when":"finished"}),
                ),
            ]),
            report("frozen review result"),
        ]);
        let (mut worker, backend) = worker(
            provider.clone(),
            vec![Arc::new(rustykrab_tools::WriteTool::in_dir(dir.path()))],
        );
        let mut definition = (*worker.definition).clone();
        definition.tools.push("write".into());
        worker.definition = Arc::new(definition);
        let got = worker.run(review_brief()).await.unwrap();
        assert_eq!(got.summary, "frozen review result");
        assert!(!target.exists(), "review invoked a host write");
        assert_eq!(
            backend.calls().len(),
            1,
            "only the result report reaches the work backend"
        );
        for (_, schemas) in provider.requests() {
            assert_eq!(schemas, vec!["result_report"]);
        }
    }

    #[tokio::test]
    async fn read_only_local_review_checks_final_text_before_backend_forwarding() {
        for payload in [
            json!({"summary":"claim","commit":"fake-commit"}),
            json!({"summary":"claim","artifacts":[{"kind":"operation_probe","value":"forged"}]}),
            json!({"summary":"claim","discovered":[{"title":"new work","objective":"unauthorized","done_when":"done"}]}),
        ] {
            let provider = Recording::new(vec![text(&payload.to_string())]);
            let (worker, backend) = worker(provider.clone(), vec![]);
            let got = worker.run(review_brief()).await;
            assert!(
                matches!(got, Err(Error::Auth(_))),
                "final text bypassed the read-only report boundary: {got:?}"
            );
            assert!(
                backend.calls().is_empty(),
                "unauthorized report reached backend"
            );
            for (_, schemas) in provider.requests() {
                assert_eq!(schemas, vec!["result_report"]);
            }
        }
    }

    #[tokio::test]
    async fn read_only_local_review_accepts_summary_only_final_text() {
        let provider = Recording::new(vec![text("frozen textual review")]);
        let (worker, backend) = worker(provider.clone(), vec![]);
        let got = worker.run(review_brief()).await.unwrap();
        assert_eq!(got.summary, "frozen textual review");
        assert_eq!(
            backend.calls().len(),
            1,
            "summary must use the report backend"
        );
        for (_, schemas) in provider.requests() {
            assert_eq!(schemas, vec!["result_report"]);
        }
    }

    #[tokio::test]
    async fn read_only_local_review_rejects_authority_in_report_before_forwarding() {
        let stub = Arc::new(StubWorkBackend::new());
        let mut recorder = RecordingBackend::new(stub.clone());
        recorder.read_only = true;
        let provenance = Provenance {
            conversation_id: None,
            filed_by_item: None,
            actor: "worker:pinch".into(),
        };
        for report in [
            ResultReport {
                summary: "claim".into(),
                discovered: vec![WorkItemDraft {
                    title: "new work".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ResultReport {
                summary: "claim".into(),
                artifacts: vec![ArtifactRef {
                    kind: "classifier_rule".into(),
                    value: "invented authority".into(),
                }],
                ..Default::default()
            },
            ResultReport {
                summary: "claim".into(),
                commit: Some("fake-commit".into()),
                ..Default::default()
            },
        ] {
            assert!(recorder
                .report("review-fixture".into(), report, provenance.clone())
                .await
                .is_err());
        }
        assert!(stub.calls().is_empty());
        assert!(recorder.take().is_empty());
        recorder
            .report(
                "review-fixture".into(),
                ResultReport {
                    summary: "legitimate JSON review".into(),
                    ..Default::default()
                },
                provenance,
            )
            .await
            .unwrap();
        assert_eq!(stub.calls().len(), 1);
    }

    #[tokio::test]
    async fn read_only_local_review_rejects_conflicting_scope_before_model_execution() {
        for conflict in 0..3 {
            let provider = Recording::new(vec![report("must not run")]);
            let (worker, _) = worker(provider.clone(), vec![Arc::new(Named("exec"))]);
            let mut b = review_brief();
            match conflict {
                0 => b.writable_resources = vec!["repo:/tmp/fixture".into()],
                1 => b.required_tools = vec!["exec".into()],
                _ => b.required_mcp_servers = vec!["external".into()],
            }
            assert!(matches!(worker.run(b).await, Err(Error::Auth(_))));
            assert!(provider.requests().is_empty());
        }
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
            ]
        );

        let caps = w.capabilities();
        assert!(
            caps.writable_resources.is_empty(),
            "context-only tools grant no external write scope"
        );
        assert_eq!(w.name(), "pinch");
        assert_eq!(w.kind(), WorkerKind::Local);
        assert_eq!(w.concurrency(), 1);
        assert!(w.healthy());
        assert_eq!(caps.models, ["scripted-local"]);
        assert_eq!(caps.mcp_servers, ["jira", "linear"]);
        assert!(caps.tools.iter().any(|t| t == "browser"));
        assert!(caps.tools.iter().any(|t| t == "result_report"));
        assert!(!caps.tools.iter().any(|t| t == "task_complete"));
        // These stand-ins declare no side effects, so they grant no writes.
        assert!(caps.writable_resources.is_empty());
    }

    #[tokio::test]
    async fn a_definitions_visible_set_is_declared_from_the_first_prefill() {
        let provider = Recording::new(vec![report("done")]);
        let coder = rustykrab_skills::builtin_agent("coder").unwrap();
        let mut definition = LocalWorker::named_definition(&coder, "clawd");
        definition.mcp_servers = vec!["linear".into()];
        let w = LocalWorker::new(
            "clawd",
            definition,
            provider.clone(),
            vec![
                Arc::new(Named("read")),
                Arc::new(Named("exec")),
                Arc::new(Named("browser")),
                Arc::new(Named("mcp__linear__search")),
                Arc::new(Named("mcp__jira__search")),
            ],
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        );

        w.run(brief("item-13")).await.unwrap();

        let (_, mut first) = provider.requests()[0].clone();
        first.sort();
        assert_eq!(
            first,
            ["exec", "mcp__linear__search", "read", "result_report"],
            "the coder's filesystem and runtime tools the host has, its server's \
             tools, and the report; not the rest of the catalog"
        );
    }

    #[test]
    fn the_default_definition_is_the_worker_file_under_the_workers_name() {
        let d = LocalWorker::default_definition("pinch");
        assert_eq!(d.id, "pinch");
        assert!(
            d.system_prompt.starts_with(
                "You are pinch, a RustyKrab worker. Each run gives you one work item."
            ),
            "{}",
            d.system_prompt
        );
        assert!(!d.system_prompt.contains("{name}"));
        assert_eq!(d.profile, "default");
        assert!(d.allowed_tools.is_none() && d.tools.is_empty());

        // A definition that names what it may write narrows the capability.
        let mut narrowed = d.clone();
        narrowed.writable_resources = vec!["calendar".into()];
        let w = LocalWorker::new(
            "pinch",
            narrowed,
            Recording::new(Vec::new()),
            Vec::new(),
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        );
        assert_eq!(w.capabilities().writable_resources, ["calendar"]);
    }

    #[tokio::test]
    async fn an_exact_file_grant_executes_without_a_workspace_or_ambient_shell() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("audit.json");
        let input = root.path().join("previous.json");
        std::fs::write(&input, "previous audit").unwrap();
        let provider = Recording::new(vec![
            call("read", json!({"path":input})),
            call(
                "write",
                json!({"path":file,"content":"{\"verified\":true}"}),
            ),
            report("audit saved"),
        ]);
        let (worker, _) = worker(
            provider.clone(),
            vec![
                Arc::new(rustykrab_tools::WriteTool::new()),
                Arc::new(rustykrab_tools::ReadTool::new()),
                Arc::new(Named("exec")),
            ],
        );
        let worker = worker
            .with_repository_shell()
            .with_writable_files(&[file.to_string_lossy().into_owned()])
            .with_readable_files(&[input.to_string_lossy().into_owned()]);
        let mut task = brief("audit");
        task.writable_resources = vec![format!("file:{}", file.display())];
        worker.run(task.clone()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"verified\":true}"
        );
        let names = &provider.requests()[0].1;
        assert!(names.iter().any(|name| name == "write"));
        assert!(!names.iter().any(|name| name == "exec"));
        task.writable_resources = vec![format!(
            "file:{}",
            root.path().join("sibling.json").display()
        )];
        assert!(worker.run(task).await.is_err());
        assert_eq!(std::fs::read_to_string(input).unwrap(), "previous audit");
        assert_eq!(provider.requests().len(), 3);
    }

    #[tokio::test]
    async fn scoped_repository_shell_is_unavailable_without_a_controller_workspace() {
        let provider = Recording::new(vec![report("analysis")]);
        let (worker, _) = worker(provider.clone(), vec![Arc::new(Named("exec"))]);
        let worker = worker.with_repository_shell();
        let mut needs_shell = brief("shell");
        needs_shell.required_tools = vec!["exec".into()];
        assert!(worker.run(needs_shell).await.is_err());
        assert!(
            provider.requests().is_empty(),
            "reject unscoped shell before inference"
        );
        worker.run(brief("analysis")).await.unwrap();
        assert!(provider.requests()[0].1.iter().all(|t| t != "exec"));
    }

    #[test]
    fn a_real_execution_tool_grants_default_write_scope_without_a_specialty() {
        let w = LocalWorker::new(
            "general",
            LocalWorker::default_definition("general"),
            Recording::new(Vec::new()),
            vec![Arc::new(rustykrab_tools::ExecTool::new())],
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        );
        assert_eq!(w.capabilities().writable_resources, ["*"]);
        assert!(w.capabilities().tools.iter().any(|t| t == "work_plan"));
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
            [
                "ask_user",
                "calendar",
                "capability_request",
                "result_report",
                "work_discuss",
                "work_file",
                "work_plan",
                "work_status"
            ]
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

    #[test]
    fn a_planning_tool_in_a_delegated_ceiling_does_not_assign_the_planner_role() {
        let mut definition = LocalWorker::default_definition("delegate");
        definition.allowed_tools = Some(vec!["work_plan".into(), "calendar".into()]);
        let worker = LocalWorker::new(
            "delegate",
            definition,
            Recording::new(Vec::new()),
            vec![Arc::new(Named("calendar"))],
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        );
        assert!(!worker.planning_only());
        let tools = worker.capabilities().tools;
        for name in [
            "result_report",
            "work_file",
            "ask_user",
            "work_plan",
            "calendar",
        ] {
            assert!(tools.iter().any(|n| n == name), "{name}: {tools:?}");
        }
    }

    #[tokio::test]
    async fn the_planner_starts_with_its_tools_and_ends_on_an_accepted_plan() {
        let plan = json!({
            "root": { "tmp": "trip" },
            "items": [
                { "tmp": "trip", "title": "Trip", "objective": "o", "done_when": "d" },
                { "tmp": "flights", "parent": { "tmp": "trip" }, "title": "Flights",
                  "objective": "o", "done_when": "d" }
            ],
            "rationale": "book it"
        });
        let provider = Recording::new(vec![call("work_plan", plan), report("never reached")]);
        let stub = Arc::new(StubWorkBackend::new());
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Named("recall_search")),
            Arc::new(Named("memory_search")),
            Arc::new(Named("browser")),
        ];
        let w = LocalWorker::new(
            "planner",
            LocalWorker::planner_definition(),
            provider.clone(),
            tools,
            Arc::new(NoSandbox),
            stub.clone(),
        );
        let caps = w.capabilities();
        assert!(caps.tools.iter().any(|t| t == "result_report"));
        for tool in ["work_plan", "work_status", "recall_search", "memory_search"] {
            assert!(
                caps.tools.iter().any(|t| t == tool),
                "{tool}: {:?}",
                caps.tools
            );
        }
        assert!(
            !caps.tools.iter().any(|t| t == "browser"),
            "no write access"
        );

        let mut b = brief("plan-1");
        b.required_tools = vec!["work_plan".into()];
        let err = w.run(b).await.unwrap_err();
        // The planner's output is the accepted plan, which the controller
        // turns into its report; the run itself returns none.
        assert!(RunFailure::from_error(&err).is_some(), "{err}");
        assert_eq!(
            provider.requests().len(),
            1,
            "the accepted plan ended the run"
        );
        let (_, first) = &provider.requests()[0];
        for tool in ["work_plan", "work_status", "recall_search", "memory_search"] {
            assert!(
                first.iter().any(|t| t == tool),
                "{tool} at the first prefill: {first:?}"
            );
        }
        assert!(matches!(stub.calls()[0], WorkCall::Plan { .. }));

        // An ordinary worker can also plan; there is no reserved role.
        let (pinch, _) = worker(Recording::new(vec![]), Vec::new());
        assert!(pinch.capabilities().tools.iter().any(|t| t == "work_plan"));
    }

    #[tokio::test]
    async fn a_run_that_repeats_itself_is_told_and_goes_on() {
        let mut script: Vec<ModelResponse> = (0..5).map(|_| call("noop", json!({}))).collect();
        script.push(report("Tried noop five times; it never changes."));
        let provider = Recording::new(script);
        let (w, _) = worker(provider.clone(), vec![Arc::new(Named("noop"))]);
        let w = w.with_stall_steps(3);
        let mut b = brief("item-stall");
        b.required_tools = vec!["noop".into()];
        let report = w.run(b).await.unwrap();
        assert_eq!(report.summary, "Tried noop five times; it never changes.");
        assert_eq!(
            provider.requests().len(),
            6,
            "the stall did not stop the run"
        );
        let (last, _) = provider.requests()[5].clone();
        let told = last
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::ToolResult(r) => Some(r.output.to_string()),
                _ => None,
            })
            .any(|output| output.contains("repeats an earlier call"));
        assert!(told, "the repeat notice reached the model");
    }

    #[tokio::test]
    async fn a_run_still_repeating_at_its_cap_names_the_stall() {
        let provider = Recording::new((0..6).map(|_| call("noop", json!({}))).collect());
        let (w, _) = worker(provider, vec![Arc::new(Named("noop"))]);
        let w = w.with_stall_steps(2);
        let mut b = brief("item-stall-cap");
        b.required_tools = vec!["noop".into()];
        b.budget.iterations = 5;
        let err = w.run(b).await.unwrap_err();
        assert_eq!(classified(&err), (ErrorClass::Model, ErrorSubclass::Loop));
        assert!(err.to_string().contains("progress ledger"), "{err}");
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

    /// Tools that appeared after the worker was built.
    struct Late(Vec<&'static str>);

    impl LateTools for Late {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0
                .iter()
                .map(|n| Arc::new(Named(n)) as Arc<dyn Tool>)
                .collect()
        }
    }

    /// Section 8, rung 2b: a skill a build wrote at run time is a tool the
    /// resumed item can have active from its first step.
    #[tokio::test]
    async fn a_tool_written_at_run_time_can_be_activated_up_front() {
        let provider = Recording::new(vec![report("High water at 14:05.")]);
        let (w, _) = worker(provider.clone(), vec![Arc::new(Named("noop"))]);
        let mut b = brief("item-13");
        b.required_tools = vec!["tide_table".into()];
        let err = w.run(b.clone()).await.unwrap_err();
        assert!(
            matches!(RunFailure::from_error(&err), Some(RunFailure::Gap { .. })),
            "not there before the build: {err}"
        );

        let w = w.with_late_tools(Arc::new(Late(vec!["tide_table", "result_report"])));
        assert!(w.capabilities().tools.iter().any(|t| t == "tide_table"));
        w.run(b).await.unwrap();
        let (_, first) = provider.requests()[0].clone();
        assert!(first.iter().any(|t| t == "tide_table"), "{first:?}");
        assert_eq!(
            first.iter().filter(|t| *t == "result_report").count(),
            1,
            "a late tool never replaces a work tool"
        );
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Section 5: a local `code` run works and commits in its own
    /// worktree, which is gone afterwards; the branch keeps the commit.
    #[tokio::test]
    async fn a_code_run_commits_in_its_worktree() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "--initial-branch=main"]);
        std::fs::write(repo.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x.invalid",
                "commit",
                "-q",
                "--no-gpg-sign",
                "-m",
                "init",
            ],
        );
        let base = git(repo.path(), &["rev-parse", "HEAD"]);
        let root = tempfile::tempdir().unwrap();
        let ws = rustykrab_control::workspace::Workspace::plan(
            root.path(),
            repo.path(),
            &base,
            "item-17",
            "run-17",
        );
        let provider = Recording::new(vec![
            call(
                "exec",
                json!({ "command": "echo '// local' >> lib.rs && git add lib.rs && git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m local" }),
            ),
            call(
                "result_report",
                json!({ "summary": "Documented it.", "changed_paths": ["lib.rs"] }),
            ),
        ]);
        let (w, _) = worker(
            provider.clone(),
            vec![Arc::new(rustykrab_tools::ExecTool::new())],
        );
        let mut b = brief("item-17");
        b.kind = WorkKind::Code;
        b.required_tools = vec!["exec".into()];
        b.workspace = Some(ws.clone());

        let got = w.run(b).await.unwrap();
        assert_eq!(got.changed_paths, ["lib.rs"]);
        let tip = ws.tip().unwrap().expect("the branch exists");
        assert_ne!(tip, base, "the run committed on its branch");
        assert!(!ws.path.exists(), "the worktree is removed");
        assert_eq!(
            git(repo.path(), &["rev-parse", "HEAD"]),
            base,
            "the checkout never moved"
        );
        let (messages, _) = provider.requests()[0].clone();
        let rendered = messages[1].content.as_text().unwrap().to_string();
        assert!(rendered.contains("workspace: "), "{rendered}");
        assert!(rendered.contains(&ws.branch), "{rendered}");
    }

    #[tokio::test]
    async fn host_exec_commands_replace_forged_artifacts_before_report_forwarding() {
        let backend = Arc::new(StubWorkBackend::new());
        let recorder = Arc::new(RecordingBackend::new(backend.clone()));
        let tool = LedgerTool {
            inner: Arc::new(rustykrab_tools::ExecTool::new()),
            ledger: Arc::new(Mutex::new(StepLedger::new(DEFAULT_STALL_STEPS))),
            commands: recorder.commands.clone(),
        };
        tool.execute(json!({"command":"echo observed"}))
            .await
            .unwrap();
        assert!(tool
            .execute(json!({"command":"unregistered_command"}))
            .await
            .is_err());
        recorder
            .report(
                "fixture".into(),
                ResultReport {
                    summary: "done".into(),
                    artifacts: vec![
                        ArtifactRef {
                            kind: COMMAND_RUN.into(),
                            value: "cargo test --forged".into(),
                        },
                        ArtifactRef {
                            kind: OPERATION_PROBE.into(),
                            value: "model-forged successful operation and executor provenance"
                                .into(),
                        },
                    ],
                    ..ResultReport::default()
                },
                Provenance::default(),
            )
            .await
            .unwrap();
        let returned = recorder.take().pop().unwrap();
        assert_eq!(
            returned.artifacts,
            vec![ArtifactRef {
                kind: COMMAND_RUN.into(),
                value: "echo observed".into()
            }]
        );
        assert!(
            matches!(backend.calls().as_slice(),[rustykrab_tools::work_backend::WorkCall::Report { report,.. }] if report==&returned)
        );
    }

    /// `r` as if it cost `tokens` (prompt and completion together).
    fn costing(mut r: ModelResponse, tokens: u32) -> ModelResponse {
        r.usage = Usage {
            prompt_tokens: tokens - tokens / 10,
            completion_tokens: tokens / 10,
            ..Usage::default()
        };
        r
    }

    #[tokio::test]
    async fn tokens_are_counted_but_never_stop_the_run() {
        let provider = Recording::new(vec![
            costing(call("noop", json!({})), 700),
            costing(call("noop", json!({})), 700),
            costing(report("done"), 700),
        ]);
        let (w, _) = worker(provider.clone(), vec![Arc::new(Named("noop"))]);
        let mut b = brief("item-20");
        b.required_tools = vec!["noop".into()];
        b.budget.tokens = 1_000;
        b.run = Some(Uuid::new_v4().to_string());
        let run = b.run.clone().unwrap();

        let report = w.run(b).await.unwrap();

        assert_eq!(report.summary, "done");
        assert_eq!(provider.requests().len(), 3);
        let usage = w.usage(&run).expect("the run's spend");
        assert_eq!(usage.tokens, 2_100);
        assert_eq!(usage.iterations, 3);
        assert_eq!(w.usage(&run), None, "taken once");
    }

    /// Only the report is declared up front; the other work tools are in
    /// the ceiling and a run loads them when it needs one.
    #[tokio::test]
    async fn a_work_tool_left_out_of_the_front_block_can_still_be_loaded() {
        let draft = json!({
            "title": "Port the number",
            "objective": "Move the number to plan B",
            "done_when": "The number works on plan B",
        });
        let provider = Recording::new(vec![
            call("tools_load", json!({ "names": ["work_file"] })),
            call("work_file", draft),
            report("Filed the follow-up."),
        ]);
        let (w, stub) = worker(
            provider.clone(),
            vec![Arc::new(rustykrab_tools::ToolsLoadTool::new())],
        );
        w.run(brief("item-50")).await.unwrap();

        let (_, first) = provider.requests()[0].clone();
        assert!(first.iter().any(|t| t == "result_report"), "{first:?}");
        assert!(!first.iter().any(|t| t == "work_file"), "{first:?}");
        assert!(
            stub.calls()
                .iter()
                .any(|c| matches!(c, WorkCall::File { .. })),
            "the loaded work_file ran: {:?}",
            stub.calls()
        );
    }

    #[tokio::test]
    async fn without_the_reminder_text_ends_the_run() {
        let provider = Recording::new(vec![text("Plan B is cheapest.")]);
        let (w, _) = worker(provider.clone(), Vec::new());
        let w = w.with_result_report_reminder(false);
        let report = w.run(brief("item-30")).await.unwrap();
        assert_eq!(report.summary, "Plan B is cheapest.");
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(w.last_run().unwrap().reminders, 0);
    }

    #[tokio::test]
    async fn a_contract_in_the_final_text_is_the_report() {
        let provider = Recording::new(vec![text(
            "Done.\n```json\n{\"summary\": \"Plan B\", \"checks_run\": [\"pytest\"]}\n```",
        )]);
        let (w, _) = worker(provider, Vec::new());
        let w = w.with_result_report_reminder(false);
        let report = w.run(brief("item-31")).await.unwrap();
        assert_eq!(report.summary, "Plan B");
        assert_eq!(report.checks_run, vec!["pytest".to_string()]);
    }

    #[test]
    fn a_trailing_json_object_without_a_summary_is_not_a_contract() {
        let report = report_from_text("Use this config: {\"retries\": 3}", false);
        assert_eq!(report.summary, "Use this config: {\"retries\": 3}");
    }

    #[tokio::test]
    async fn usage_counts_tokens_iterations_and_reminders() {
        let provider = Recording::new(vec![
            costing(text("Plan B, I think."), 300),
            costing(report("B is cheapest."), 200),
        ]);
        let (w, _) = worker(provider, Vec::new());
        let w = w.with_result_report_reminder(true);
        let mut b = brief("item-21");
        b.run = Some("run-21".into());
        w.run(b).await.unwrap();
        let usage = w.usage("run-21").unwrap();
        assert_eq!(usage.tokens, 500);
        assert_eq!(usage.iterations, 2);
        assert_eq!(usage.reminders, 1);
        assert_eq!(w.usage("never-ran"), None);
    }

    /// Answers `check_model` with whatever the test set, counting asks.
    struct Checked {
        answer: Mutex<ModelCheck>,
        asked: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ModelProvider for Checked {
        fn name(&self) -> &str {
            "checked"
        }
        async fn check_model(&self) -> ModelCheck {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.answer.lock().unwrap().clone()
        }
        async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> Result<ModelResponse> {
            Err(Error::ModelEmptyResponse("never called".into()))
        }
    }

    #[tokio::test]
    async fn health_follows_the_providers_model_check() {
        let provider = Arc::new(Checked {
            answer: Mutex::new(ModelCheck::Missing("no model `nope:1b`".into())),
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        let w = LocalWorker::new(
            "snapper",
            LocalWorker::default_definition("snapper"),
            provider.clone(),
            Vec::new(),
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        );
        assert!(w.healthy(), "healthy until the provider has been asked");

        assert!(w.refresh().await);
        assert!(!w.healthy(), "a missing model makes the worker unhealthy");
        assert_eq!(
            w.unhealthy_reason().as_deref(),
            Some("no model `nope:1b`"),
            "the health line says why"
        );
        assert!(!w.refresh().await, "asked too recently to ask again");
        let asked = |p: &Checked| p.asked.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(asked(&provider), 1);

        // Once due again, the model is back.
        *provider.answer.lock().unwrap() = ModelCheck::Available;
        let long_ago = Instant::now()
            .checked_sub(MODEL_CHECK_EVERY)
            .expect("the clock is past one check interval");
        w.model.lock().unwrap().as_mut().unwrap().0 = long_ago;
        assert!(w.refresh().await);
        assert!(w.healthy());

        // A server that cannot say does not make the worker unhealthy.
        *provider.answer.lock().unwrap() = ModelCheck::Unknown;
        w.model.lock().unwrap().as_mut().unwrap().0 = long_ago;
        assert!(w.refresh().await);
        assert!(w.healthy());
        assert_eq!(w.unhealthy_reason(), None);
        assert_eq!(asked(&provider), 3);

        // Nor does it make a missing model healthy: the last answer stands.
        *provider.answer.lock().unwrap() = ModelCheck::Missing("gone again".into());
        w.model.lock().unwrap().as_mut().unwrap().0 = long_ago;
        assert!(w.refresh().await);
        assert!(!w.healthy());
        *provider.answer.lock().unwrap() = ModelCheck::Unknown;
        w.model.lock().unwrap().as_mut().unwrap().0 = long_ago;
        assert!(w.refresh().await);
        assert!(!w.healthy(), "an outage keeps a missing model missing");
        assert_eq!(w.unhealthy_reason().as_deref(), Some("gone again"));
        assert_eq!(asked(&provider), 5);
    }

    /// A kept conversation every run under its id continues.
    struct JobConversation {
        kept: Conversation,
        saves: Mutex<Vec<Conversation>>,
    }

    #[async_trait]
    impl RunTranscripts for JobConversation {
        async fn save(&self, conversation: &Conversation) -> Result<()> {
            self.saves.lock().unwrap().push(conversation.clone());
            Ok(())
        }

        async fn resume(&self, id: Uuid, _brief: &Brief) -> Result<Option<Resumed>> {
            Ok((id == self.kept.id).then(|| Resumed {
                conversation: self.kept.clone(),
                guidance: Some(
                    "<skill_instructions name=\"plants\">water them</skill_instructions>".into(),
                ),
                preface: Some("[Scheduled task] Your scheduled task is due again.".into()),
            }))
        }
    }

    #[tokio::test]
    async fn a_run_under_a_kept_conversations_id_continues_it() {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let kept = Conversation {
            id,
            messages: vec![
                Message::stamped(Role::System, MessageContent::Text("old prompt".into())),
                Message::stamped(Role::User, MessageContent::Text("first run".into())),
                Message::stamped(
                    Role::System,
                    MessageContent::Text("You have reached the iteration limit.".into()),
                ),
                Message::stamped(Role::Assistant, MessageContent::Text("watered".into())),
            ],
            created_at: now,
            updated_at: now,
            title: Some("job".into()),
            summary: None,
            detected_profile: None,
            channel_source: Some("telegram".into()),
            channel_id: Some("42".into()),
            channel_thread_id: None,
        };
        let transcripts = Arc::new(JobConversation {
            kept,
            saves: Mutex::new(Vec::new()),
        });
        let provider = Recording::new(vec![report("Watered the plants.")]);
        let (w, _) = worker(provider.clone(), Vec::new());
        let w = w.with_transcripts(transcripts.clone());
        let mut b = brief("item-22");
        b.run = Some(id.to_string());

        w.run(b).await.unwrap();

        let (messages, _) = &provider.requests()[0];
        assert_eq!(messages[0].role, Role::System);
        let system = messages[0].content.as_text().unwrap();
        assert!(system.starts_with("You are pinch"), "{system}");
        assert!(
            system.ends_with("water them</skill_instructions>"),
            "{system}"
        );
        assert!(
            messages[1..].iter().all(|m| m.role != Role::System),
            "one leading system message: {messages:?}"
        );
        assert!(messages.iter().any(|m| m.content.as_text()
            == Some("[System notice] You have reached the iteration limit.")));
        assert_eq!(messages[1].content.as_text(), Some("first run"));
        let turn = messages.last().unwrap().content.as_text().unwrap();
        assert!(
            turn.starts_with(
                "[Scheduled task] Your scheduled task is due again.\n\nwork item: #item-22"
            ),
            "{turn}"
        );

        let saves = transcripts.saves.lock().unwrap().clone();
        assert!(saves.iter().all(|c| c.id == id), "kept under its own id");
        assert_eq!(saves[0].channel_source.as_deref(), Some("telegram"));
        assert!(!saves.iter().any(|c| c
            .messages
            .iter()
            .any(|m| m.content.as_text() == Some("old prompt"))));

        // Any other id starts fresh.
        let provider = Recording::new(vec![report("done")]);
        let (w, _) = worker(provider.clone(), Vec::new());
        let w = w.with_transcripts(transcripts);
        w.run(brief("item-23")).await.unwrap();
        assert_eq!(provider.requests()[0].0.len(), 2);
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
            "budget: 100 steps\n",
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
    fn the_brief_shows_where_a_blocked_report_puts_its_question() {
        let prompt = render_brief(&brief("item-1"));
        assert!(
            prompt.contains(
                r#"{"reason": "needs_decision", "detail": "the question or what you need", "needs": []}"#
            ),
            "{prompt}"
        );
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
    #[tokio::test]
    async fn read_only_probe_context_never_executes_a_registered_resource_tool() {
        use rustykrab_control::capability_proof::*;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resources = Arc::new(ProbeResources {
            calls: calls.clone(),
            output: json!({"workers":[]}),
            delay: Duration::ZERO,
        });
        let (worker, _) = worker(Recording::new(vec![]), vec![resources]);
        let mut request = probe_request(
            &worker,
            RegisteredAction::ToolCallable {
                tool: "work_resources".into(),
                args: json!({}),
            },
        );
        request.context.requirements.read_only_review = true;
        request.scope = worker.probe_scope(&request.context);
        request.expected.inputs_digest =
            probe_inputs_digest(&request.action, &request.scope, &request.context);
        assert!(request.valid());
        assert_eq!(
            worker.observe_probe(&request).await.outcome,
            ProbeOutcome::Unobserved
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
