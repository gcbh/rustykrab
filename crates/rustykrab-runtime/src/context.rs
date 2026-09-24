//! Everything needed to run an agent turn, and nothing about how the turn
//! was requested.
//!
//! This was the non-HTTP half of the gateway's `AppState`. Splitting it out
//! is what lets a Telegram or Slack loop — or a scheduled job, or a test —
//! run a turn without constructing a web server's state, and without
//! depending on an Axum crate to do work that has nothing to do with HTTP.

use std::sync::Arc;

use rustykrab_agent::{HarnessProfile, HarnessRouter, ProcessSandbox, Sandbox};
use rustykrab_core::active_tools::ActiveToolsRegistry;
use rustykrab_core::activity::ActivityTracker;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::recall::RecallStore;
use rustykrab_core::retrieval_log::RetrievalLog;
use rustykrab_core::todo::TodoStore;
use rustykrab_memory::MemorySystem;
use rustykrab_skills::SkillRegistry;
use rustykrab_store::Store;
use uuid::Uuid;

/// The agent's world: what it can call, what it remembers, what it is
/// allowed to do, and where its history lives.
///
/// Cheap to clone — every field is an `Arc`, a handle, or a flag.
#[derive(Clone)]
pub struct AgentContext {
    pub store: Store,
    pub tools: Vec<Arc<dyn rustykrab_core::Tool>>,
    pub provider: Arc<dyn ModelProvider>,
    /// Model used to decide what an inbound message is worth remembering.
    ///
    /// Separate from `provider` because the two want opposite settings: the
    /// agent wants thinking on, and a one-sentence classification wants it
    /// off — measured at 0.4–2.6s without it against tens of seconds with.
    /// `think` is provider-level config in the Ollama client rather than a
    /// per-call flag, so sharing one handle cannot express both.
    ///
    /// Pointing this at the same model and `num_ctx` as `provider` keeps it
    /// free: no second model resident, and no `num_ctx` switch, which costs
    /// a reload and a full re-prefill. It is also the seam for moving
    /// distillation to a bigger model on another node later.
    ///
    /// `None` disables distillation; working-memory capture is unaffected.
    pub distiller: Option<Arc<dyn ModelProvider>>,
    /// Sandbox for tool execution isolation.
    pub sandbox: Arc<dyn Sandbox>,
    /// Base harness profile, used as fallback and as the template a routed
    /// preset is overlaid on.
    pub harness_profile: HarnessProfile,
    /// Classifies each message and selects a profile. `None` means static
    /// profile mode, using `harness_profile` directly.
    pub harness_router: Option<Arc<HarnessRouter>>,
    pub orchestration_config: OrchestrationConfig,
    pub skill_registry: Arc<SkillRegistry>,
    /// How to observe the effects a skill's `[outcome]` block declares.
    /// `None` means this deployment can produce no ground truth, which is
    /// the honest state rather than a degraded one.
    pub probes: Option<Arc<rustykrab_core::ProbeRegistry>>,
    /// Hybrid memory. `None` when no memory backend is configured.
    pub memory: Option<Arc<MemorySystem>>,
    /// Persistent agent identifier, the owner for memory writes. `None`
    /// when memory is not configured.
    pub agent_id: Option<Uuid>,
    /// Backs the `tools_load` / `tools_list` meta-tools: which tools are
    /// active per conversation, so the schemas sent to the model stay
    /// compact until the agent asks for more.
    pub active_tools: Arc<ActiveToolsRegistry>,
    /// Per-conversation archive of compaction-displaced history, backing
    /// the `recall_*` tools.
    pub recall: Arc<RecallStore>,
    /// Per-conversation todo list backing the `todo_*` tools. In memory
    /// only: a todo list is short-horizon working state, not durable
    /// history like `recall`.
    pub todos: Arc<TodoStore>,
    /// Whether sub-agent / session-management tools are granted to sessions
    /// created here. Off by default — a sub-agent can spawn nested agent
    /// loops, which amplifies any prompt-injection blast radius.
    pub subagents_enabled: bool,
    /// Whether the computer-use tool is granted. Off by default.
    pub computer_use_enabled: bool,
    /// When each agent last saw inbound activity. Gates the downtime
    /// analysis worker. See `DREAMING.md`.
    pub activity: ActivityTracker,
    /// Records which memories were surfaced into each conversation so a
    /// completed run's outcome can be attributed to them.
    pub retrieval_log: RetrievalLog,
    /// Whether completed runs report their outcome to the store. Off by
    /// default: instrumentation is opt-in.
    pub outcome_capture_enabled: bool,
}

impl AgentContext {
    pub fn new(
        store: Store,
        tools: Vec<Arc<dyn rustykrab_core::Tool>>,
        provider: Arc<dyn ModelProvider>,
    ) -> Self {
        // Back the recall archive with SQLite so compaction-displaced
        // history survives restarts. The in-memory `RecallStore` acts as a
        // write-through cache, lazily hydrated per conversation.
        let recall = Arc::new(RecallStore::with_persistence(Arc::new(
            store.recall_archive(),
        )));
        Self {
            store,
            tools,
            provider,
            sandbox: Arc::new(ProcessSandbox::new()),
            harness_profile: HarnessProfile::default(),
            harness_router: None,
            orchestration_config: OrchestrationConfig::default(),
            skill_registry: Arc::new(SkillRegistry::new()),
            distiller: None,
            probes: None,
            memory: None,
            agent_id: None,
            active_tools: Arc::new(ActiveToolsRegistry::new()),
            recall,
            todos: Arc::new(TodoStore::new()),
            subagents_enabled: false,
            computer_use_enabled: false,
            activity: ActivityTracker::new(),
            retrieval_log: RetrievalLog::new(),
            outcome_capture_enabled: false,
        }
    }

    pub fn with_outcome_capture(mut self, enabled: bool) -> Self {
        self.outcome_capture_enabled = enabled;
        self
    }

    pub fn with_activity_tracker(mut self, activity: ActivityTracker) -> Self {
        self.activity = activity;
        self
    }

    pub fn with_retrieval_log(mut self, log: RetrievalLog) -> Self {
        self.retrieval_log = log;
        self
    }

    pub fn with_subagents_enabled(mut self, enabled: bool) -> Self {
        self.subagents_enabled = enabled;
        self
    }

    pub fn with_computer_use_enabled(mut self, enabled: bool) -> Self {
        self.computer_use_enabled = enabled;
        self
    }

    pub fn with_memory(mut self, memory: Arc<MemorySystem>, agent_id: Uuid) -> Self {
        self.memory = Some(memory);
        self.agent_id = Some(agent_id);
        self
    }

    /// Set the model that decides what is worth remembering. See
    /// [`AgentContext::distiller`].
    pub fn with_distiller(mut self, distiller: Arc<dyn ModelProvider>) -> Self {
        self.distiller = Some(distiller);
        self
    }

    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_harness_profile(mut self, profile: HarnessProfile) -> Self {
        self.harness_profile = profile;
        self
    }

    pub fn with_harness_router(mut self, router: Arc<HarnessRouter>) -> Self {
        self.harness_router = Some(router);
        self
    }

    /// Set the router, or leave static-profile mode in place when `None`.
    pub fn with_harness_router_opt(mut self, router: Option<Arc<HarnessRouter>>) -> Self {
        self.harness_router = router;
        self
    }

    pub fn with_probes(mut self, probes: Arc<rustykrab_core::ProbeRegistry>) -> Self {
        self.probes = Some(probes);
        self
    }

    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = registry;
        self
    }

    pub fn with_orchestration_config(mut self, config: OrchestrationConfig) -> Self {
        self.orchestration_config = config;
        self
    }

    /// The harness profile for a given user message. Classifies via the
    /// router when one is configured, otherwise returns the base profile.
    pub async fn profile_for(&self, user_message: &str) -> HarnessProfile {
        if let Some(router) = &self.harness_router {
            router.route(user_message).await
        } else {
            self.harness_profile.clone()
        }
    }

    /// A harness profile by name.
    pub fn profile_for_name(&self, name: &str) -> HarnessProfile {
        match name {
            "coding" => HarnessProfile::coding(),
            "research" => HarnessProfile::research(),
            "creative" => HarnessProfile::creative(),
            _ => self.harness_profile.clone(),
        }
    }
}
