use openclaw_agent::{HarnessProfile, HarnessRouter};
use openclaw_channels::{SignalChannel, TelegramChannel};
use openclaw_store::Store;
use std::sync::Arc;

use crate::origin::OriginPolicy;
use crate::rate_limit::{RateLimitConfig, RateLimiter};

/// Shared application state threaded through axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub tools: Arc<dyn openclaw_core::Tool>,
    pub auth_token: String,
    pub rate_limiter: Arc<RateLimiter>,
    pub origin_policy: OriginPolicy,
    pub telegram: Option<Arc<TelegramChannel>>,
    pub signal: Option<Arc<SignalChannel>>,
    /// Base harness profile (used as fallback and template).
    pub harness_profile: HarnessProfile,
    /// Auto-router that classifies messages and selects profiles on-the-fly.
    /// None = static profile mode (uses harness_profile directly).
    pub harness_router: Option<Arc<HarnessRouter>>,
}

impl AppState {
    pub fn new(
        store: Store,
        tools: Arc<dyn openclaw_core::Tool>,
        auth_token: String,
    ) -> Self {
        Self {
            store,
            tools,
            auth_token,
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig::default())),
            origin_policy: OriginPolicy::default(),
            telegram: None,
            signal: None,
            harness_profile: HarnessProfile::default(),
            harness_router: None,
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
