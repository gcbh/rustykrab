use std::collections::HashSet;

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
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
    ///
    /// Allows HTTP and HTTPS variants of loopback addresses, including
    /// IPv6 `[::1]`, so that HTTPS-enabled local dev servers and IPv6
    /// clients are not rejected.
    pub fn is_allowed(&self, origin: &str) -> bool {
        // Always allow loopback origins served by us.
        if origin.starts_with("http://127.0.0.1:")
            || origin.starts_with("https://127.0.0.1:")
            || origin.starts_with("http://localhost:")
            || origin.starts_with("https://localhost:")
            || origin.starts_with("http://[::1]:")
            || origin.starts_with("https://[::1]:")
        {
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

/// Axum middleware that validates the Origin header and adds CORS response headers.
///
/// For sensitive endpoints (/api/ and /webhook/), the Origin header is
/// mandatory. This prevents non-browser tools from bypassing origin
/// protection by simply omitting the header.
///
/// When the origin is allowed, CORS headers are added to the response
/// so that legitimate cross-origin requests from browsers succeed.
pub async fn origin_check_middleware(
    state: axum::extract::State<crate::AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path();
    let is_sensitive = path.starts_with("/api/") || path.starts_with("/webhook/");

    let allowed_origin = match request.headers().get(header::ORIGIN) {
        Some(origin) => {
            let origin_str = origin.to_str().unwrap_or("");
            if !state.origin_policy.is_allowed(origin_str) {
                tracing::warn!(
                    origin = origin_str,
                    "rejected request from disallowed origin"
                );
                return Err(StatusCode::FORBIDDEN);
            }
            // Clone the already-parsed HeaderValue to echo back later —
            // cheaper than re-parsing the origin string per response.
            Some(origin.clone())
        }
        None if is_sensitive => {
            // Server-to-server clients (e.g. the Apollo BFF) do not send an
            // Origin header. Permit them only when they present a valid
            // bearer service token: possession of the secret proves
            // authorization, and a cross-origin browser cannot forge it
            // (it can't set Authorization without a CORS preflight we reject).
            if crate::auth::has_valid_bearer_token(&state.0, request.headers()) {
                None
            } else {
                tracing::warn!(
                    path = %path,
                    "rejected request without Origin header on sensitive endpoint"
                );
                return Err(StatusCode::FORBIDDEN);
            }
        }
        None => None,
    };

    let mut response = next.run(request).await;

    // Add CORS headers when the origin was validated.
    if let Some(origin) = allowed_origin {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PUT, DELETE, PATCH, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type, Authorization"),
        );
    }

    Ok(response)
}
