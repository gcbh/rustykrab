use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

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

    // All /api/ endpoints require Bearer token.
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // Hold the read guard during comparison to prevent TOCTOU race
    // with token rotation. The guard is dropped before the await point
    // (next.run) to avoid holding the lock across an async boundary.
    let is_valid = {
        let token_guard = state.auth_token.read().unwrap_or_else(|e| e.into_inner());
        token.is_some_and(|t| constant_time_eq(t, &token_guard))
    };

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

/// Constant-time string comparison to prevent timing attacks.
/// Compares all bytes up to the length of the longer string
/// so that the length of neither input is leaked through timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let len = a_bytes.len().max(b_bytes.len());
    let mut result = (a_bytes.len() != b_bytes.len()) as u8;
    for i in 0..len {
        let x = a_bytes.get(i).copied().unwrap_or(0);
        let y = b_bytes.get(i).copied().unwrap_or(0);
        result |= x ^ y;
    }
    result == 0
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
