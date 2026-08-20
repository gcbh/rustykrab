use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use rustykrab_core::crypto::constant_time_eq;

use crate::AppState;

/// Bearer-token authentication middleware.
///
/// Validates the `Authorization: Bearer <token>` header against the
/// server's configured token using constant-time comparison.
///
/// Security: All endpoints except /api/health and static assets require
/// authentication. Webhook endpoints use their own auth mechanism.
pub async fn require_auth(
    state: axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Health endpoint is always public.
    if request.uri().path() == "/api/health" {
        return Ok(next.run(request).await);
    }

    // Static assets are public (the WebChat UI).
    if !request.uri().path().starts_with("/api/") && !request.uri().path().starts_with("/webhook/")
    {
        return Ok(next.run(request).await);
    }

    // Webhook endpoints use their own auth (e.g. Telegram secret token).
    if request.uri().path().starts_with("/webhook/") {
        return Ok(next.run(request).await);
    }

    // All /api/ endpoints require Bearer token. The token is compared
    // under the read lock and the guard released before the await point
    // (next.run), so the lock is never held across an async boundary and
    // there is no TOCTOU race with token rotation.
    let is_valid = has_valid_bearer_token(&state.0, request.headers());

    if is_valid {
        Ok(next.run(request).await)
    } else {
        tracing::warn!(
            path = %request.uri().path(),
            "rejected unauthenticated request"
        );
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Generate a cryptographically random 32-byte hex token.
pub fn generate_token() -> String {
    use rand::TryRngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG failed");
    hex::encode(bytes)
}

/// Returns true if `headers` carries an `Authorization: Bearer <token>`
/// matching `expected`, compared in constant time.
pub fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t, expected))
}

/// Returns true if the request bears a valid service token for this server.
///
/// Reads the configured token under the lock and compares before returning,
/// so the guard is never held across an await point.
pub fn has_valid_bearer_token(state: &AppState, headers: &HeaderMap) -> bool {
    let guard = state.auth_token.read().unwrap_or_else(|e| e.into_inner());
    bearer_matches(headers, &guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn matches_correct_bearer_token() {
        assert!(bearer_matches(
            &headers_with_auth("Bearer secret123"),
            "secret123"
        ));
    }

    #[test]
    fn rejects_wrong_token() {
        assert!(!bearer_matches(
            &headers_with_auth("Bearer wrong"),
            "secret123"
        ));
    }

    #[test]
    fn rejects_missing_header() {
        assert!(!bearer_matches(&HeaderMap::new(), "secret123"));
    }

    #[test]
    fn rejects_missing_bearer_prefix() {
        assert!(!bearer_matches(
            &headers_with_auth("secret123"),
            "secret123"
        ));
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert!(!bearer_matches(
            &headers_with_auth("Basic secret123"),
            "secret123"
        ));
    }
}
