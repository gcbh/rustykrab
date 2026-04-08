use std::collections::HashSet;

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// Set of allowed WebSocket/HTTP origins.
///
/// Prevents the ClawJacked class of attacks (CVE-2026-32025) where a
/// malicious website opens a WebSocket to localhost and hijacks the agent.
#[derive(Debug, Clone)]
pub struct OriginPolicy {
    allowed: HashSet<String>,
}

impl OriginPolicy {
    /// Create a policy that only allows the given origins.
    /// An empty set means *no* cross-origin requests are permitted
    /// (only same-origin / missing Origin header from non-browser clients).
    pub fn new(allowed: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
        }
    }

    /// Check whether a given origin string is permitted.
    pub fn is_allowed(&self, origin: &str) -> bool {
        // Always allow loopback origins served by us.
        if origin.starts_with("http://127.0.0.1:") || origin.starts_with("http://localhost:") {
            return true;
        }
        self.allowed.contains(origin)
    }
}

impl Default for OriginPolicy {
    fn default() -> Self {
        // By default, only allow our own loopback origins.
        Self::new(std::iter::empty::<String>())
    }
}

/// Axum middleware that validates the Origin header.
///
/// Non-browser clients (curl, SDKs) typically don't send an Origin header,
/// so a missing header is allowed. But if an Origin header IS present,
/// it must match the policy — this is what blocks browser-based attacks.
pub async fn origin_check_middleware(
    state: axum::extract::State<crate::AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin_str = origin.to_str().unwrap_or("");
        if !state.origin_policy.is_allowed(origin_str) {
            tracing::warn!(origin = origin_str, "rejected request from disallowed origin");
            return Err(StatusCode::FORBIDDEN);
        }
    }

    Ok(next.run(request).await)
}
