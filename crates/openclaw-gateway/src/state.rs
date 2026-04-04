use openclaw_agent::HarnessProfile;
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
    pub harness_profile: HarnessProfile,
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
}
