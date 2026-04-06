use openclaw_agent::{HarnessProfile, HarnessRouter, OrchestrationPipeline, ProcessSandbox, Sandbox};
use openclaw_channels::{SignalChannel, TelegramChannel};
use openclaw_core::model::ModelProvider;
use openclaw_core::orchestration::OrchestrationConfig;
use openclaw_skills::SkillRegistry;
use openclaw_store::Store;
use std::sync::{Arc, RwLock};

use crate::origin::OriginPolicy;
use crate::rate_limit::{RateLimitConfig, RateLimiter};

/// Shared application state threaded through axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub tools: Vec<Arc<dyn openclaw_core::Tool>>,
    pub provider: Arc<dyn ModelProvider>,
    pub auth_token: Arc<RwLock<String>>,
    pub rate_limiter: Arc<RateLimiter>,
    pub origin_policy: OriginPolicy,
    pub telegram: Option<Arc<TelegramChannel>>,
    pub signal: Option<Arc<SignalChannel>>,
    /// Sandbox for tool execution isolation.
    pub sandbox: Arc<dyn Sandbox>,
    /// Base harness profile (used as fallback and template).
    pub harness_profile: HarnessProfile,
    /// Auto-router that classifies messages and selects profiles on-the-fly.
    /// None = static profile mode (uses harness_profile directly).
    pub harness_router: Option<Arc<HarnessRouter>>,
    /// Orchestration pipeline for recursive agentic patterns.
    /// None = disabled (direct agent loop only).
    pub orchestration_pipeline: Option<Arc<OrchestrationPipeline>>,
    /// Orchestration configuration.
    pub orchestration_config: OrchestrationConfig,
    /// Skill registry for SKILL.md-based skills.
    pub skill_registry: Arc<SkillRegistry>,
}

impl AppState {
    pub fn new(
        store: Store,
        tools: Vec<Arc<dyn openclaw_core::Tool>>,
        provider: Arc<dyn ModelProvider>,
        auth_token: String,
    ) -> Self {
        Self {
            store,
            tools,
            provider,
            auth_token: Arc::new(RwLock::new(auth_token)),
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig::default())),
            origin_policy: OriginPolicy::default(),
            telegram: None,
            signal: None,
            sandbox: Arc::new(ProcessSandbox::new()),
            harness_profile: HarnessProfile::default(),
            harness_router: None,
            orchestration_pipeline: None,
            orchestration_config: OrchestrationConfig::default(),
            skill_registry: Arc::new(SkillRegistry::new()),
        }
    }

    /// Override the origin policy.
    pub fn with_origin_policy(mut self, policy: OriginPolicy) -> Self {
        self.origin_policy = policy;
        self
    }

    /// Override the rate limit configuration.
    pub fn with_rate_limit(mut self, config: RateLimitConfig) -> Self {
        self.rate_limiter = Arc::new(RateLimiter::new(config));
        self
    }

    /// Attach a Telegram channel.
    pub fn with_telegram(mut self, telegram: Arc<TelegramChannel>) -> Self {
        self.telegram = Some(telegram);
        self
    }

    /// Attach a Signal channel.
    pub fn with_signal(mut self, signal: Arc<SignalChannel>) -> Self {
        self.signal = Some(signal);
        self
    }

    /// Override the sandbox implementation.
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Set the harness profile.
    pub fn with_harness_profile(mut self, profile: HarnessProfile) -> Self {
        self.harness_profile = profile;
        self
    }

    /// Enable auto-routing: a cheap model classifies each message and
    /// selects the right harness profile on-the-fly.
    pub fn with_harness_router(mut self, router: Arc<HarnessRouter>) -> Self {
        self.harness_router = Some(router);
        self
    }

    /// Enable the orchestration pipeline for recursive agentic patterns.
    pub fn with_orchestration_pipeline(mut self, pipeline: Arc<OrchestrationPipeline>) -> Self {
        self.orchestration_pipeline = Some(pipeline);
        self
    }

    /// Set the skill registry.
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = registry;
        self
    }

    /// Set the orchestration configuration.
    pub fn with_orchestration_config(mut self, config: OrchestrationConfig) -> Self {
        self.orchestration_config = config;
        self
    }

    /// Rotate the auth token: generates a new random token, stores it,
    /// and returns the new value. The old token is immediately invalidated.
    pub fn rotate_token(&self) -> String {
        let new_token = crate::auth::generate_token();
        let mut guard = self.auth_token.write().unwrap_or_else(|e| e.into_inner());
        *guard = new_token.clone();
        new_token
    }

    /// Get the harness profile for a given user message.
    /// If a router is configured, classifies the message automatically.
    /// Otherwise, returns the static base profile.
    pub async fn profile_for(&self, user_message: &str) -> HarnessProfile {
        if let Some(router) = &self.harness_router {
            router.route(user_message).await
        } else {
            self.harness_profile.clone()
        }
    }
}
