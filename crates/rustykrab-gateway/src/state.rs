use rustykrab_agent::{HarnessProfile, HarnessRouter, Sandbox};
use rustykrab_channels::{SignalChannel, SlackChannel, TelegramChannel, VideoChannel};
use rustykrab_core::activity::ActivityTracker;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::retrieval_log::RetrievalLog;
use rustykrab_memory::MemorySystem;
use rustykrab_runtime::AgentContext;
use rustykrab_skills::SkillRegistry;
use rustykrab_store::Store;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

use crate::origin::OriginPolicy;
use crate::rate_limit::{RateLimitConfig, RateLimiter};

/// Shared application state threaded through axum handlers.
///
/// Two halves, deliberately separated. `agent` is everything a turn needs
/// and is [`AgentContext`], which lives in `rustykrab-runtime` so a caller
/// with no HTTP in sight — a Telegram loop, a scheduled job, a test — can
/// hold one without constructing a web server's state. The rest of this
/// struct is the web server: auth, rate limiting, origin policy, the
/// credential page, and the channel handles the webhook routes deliver to.
#[derive(Clone)]
pub struct AppState {
    /// Everything needed to run a turn. See [`AgentContext`].
    pub agent: AgentContext,

    // --- HTTP-only ---
    pub auth_token: Arc<RwLock<String>>,
    pub rate_limiter: Arc<RateLimiter>,
    pub origin_policy: OriginPolicy,
    /// Who may open the credential page served at `/c/{token}`.
    pub credential_page_policy: crate::PageIdentityPolicy,
    /// Wake-up channel and cancellation registry for the delegated-task
    /// queue. Shared between the `/api/tasks` handlers, which enqueue and
    /// cancel, and the worker, which drains.
    pub task_signal: crate::tasks::TaskQueueSignal,

    // --- Outbound channels, delivered to by the webhook routes ---
    pub telegram: Option<Arc<TelegramChannel>>,
    pub signal: Option<Arc<SignalChannel>>,
    pub slack: Option<Arc<SlackChannel>>,
    /// Video communication channel (hyperframes MCP).
    pub video: Option<Arc<VideoChannel>>,
}

impl AppState {
    pub fn new(
        store: Store,
        tools: Vec<Arc<dyn rustykrab_core::Tool>>,
        provider: Arc<dyn ModelProvider>,
        auth_token: String,
    ) -> Self {
        Self {
            agent: AgentContext::new(store, tools, provider),
            auth_token: Arc::new(RwLock::new(auth_token)),
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig::from_env())),
            origin_policy: OriginPolicy::default(),
            credential_page_policy: crate::PageIdentityPolicy::default(),
            task_signal: crate::tasks::TaskQueueSignal::new(),
            telegram: None,
            signal: None,
            slack: None,
            video: None,
        }
    }

    /// Replace the agent context wholesale, for callers that build one up
    /// with `AgentContext`'s own builders.
    pub fn with_agent_context(mut self, agent: AgentContext) -> Self {
        self.agent = agent;
        self
    }

    // --- Delegating builders -------------------------------------------
    //
    // Kept so existing wiring reads the same. Each defers to the
    // `AgentContext` builder of the same name.

    pub fn with_outcome_capture(mut self, enabled: bool) -> Self {
        self.agent = self.agent.with_outcome_capture(enabled);
        self
    }

    pub fn with_activity_tracker(mut self, activity: ActivityTracker) -> Self {
        self.agent = self.agent.with_activity_tracker(activity);
        self
    }

    pub fn with_retrieval_log(mut self, log: RetrievalLog) -> Self {
        self.agent = self.agent.with_retrieval_log(log);
        self
    }

    pub fn with_subagents_enabled(mut self, enabled: bool) -> Self {
        self.agent = self.agent.with_subagents_enabled(enabled);
        self
    }

    pub fn with_computer_use_enabled(mut self, enabled: bool) -> Self {
        self.agent = self.agent.with_computer_use_enabled(enabled);
        self
    }

    pub fn with_memory(mut self, memory: Arc<MemorySystem>, agent_id: Uuid) -> Self {
        self.agent = self.agent.with_memory(memory, agent_id);
        self
    }

    /// Set the model that decides what an inbound message is worth
    /// remembering. See `AgentContext::distiller`.
    pub fn with_distiller(mut self, distiller: Arc<dyn ModelProvider>) -> Self {
        self.agent = self.agent.with_distiller(distiller);
        self
    }

    /// Set the distiller, or leave distillation off when `None`. The caller
    /// decides whether a distiller is available; this keeps that decision
    /// out of the builder chain.
    pub fn with_distiller_opt(mut self, distiller: Option<Arc<dyn ModelProvider>>) -> Self {
        if let Some(d) = distiller {
            self.agent = self.agent.with_distiller(d);
        }
        self
    }

    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.agent = self.agent.with_sandbox(sandbox);
        self
    }

    pub fn with_harness_profile(mut self, profile: HarnessProfile) -> Self {
        self.agent = self.agent.with_harness_profile(profile);
        self
    }

    pub fn with_harness_router(mut self, router: Arc<HarnessRouter>) -> Self {
        self.agent = self.agent.with_harness_router(router);
        self
    }

    pub fn with_harness_router_opt(mut self, router: Option<Arc<HarnessRouter>>) -> Self {
        self.agent = self.agent.with_harness_router_opt(router);
        self
    }

    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.agent = self.agent.with_skill_registry(registry);
        self
    }

    pub fn with_orchestration_config(mut self, config: OrchestrationConfig) -> Self {
        self.agent = self.agent.with_orchestration_config(config);
        self
    }

    // --- HTTP-only builders ---------------------------------------------

    pub fn with_origin_policy(mut self, policy: OriginPolicy) -> Self {
        self.origin_policy = policy;
        self
    }

    pub fn with_rate_limit(mut self, config: RateLimitConfig) -> Self {
        self.rate_limiter = Arc::new(RateLimiter::new(config));
        self
    }

    /// Who may open the credential page. Fails closed by default.
    pub fn with_credential_page_policy(mut self, p: crate::PageIdentityPolicy) -> Self {
        self.credential_page_policy = p;
        self
    }

    pub fn with_telegram(mut self, telegram: Arc<TelegramChannel>) -> Self {
        self.telegram = Some(telegram);
        self
    }

    pub fn with_signal(mut self, signal: Arc<SignalChannel>) -> Self {
        self.signal = Some(signal);
        self
    }

    pub fn with_slack(mut self, slack: Arc<SlackChannel>) -> Self {
        self.slack = Some(slack);
        self
    }

    pub fn with_video(mut self, video: Arc<VideoChannel>) -> Self {
        self.video = Some(video);
        self
    }

    /// Rotate the auth token: generates a new random token, stores it, and
    /// returns the new value. The old token is immediately invalidated.
    pub fn rotate_token(&self) -> String {
        let new_token = crate::auth::generate_token();
        let mut guard = self.auth_token.write().unwrap_or_else(|e| e.into_inner());
        *guard = new_token.clone();
        new_token
    }
}
