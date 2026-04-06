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
    if !request.uri().path().starts_with("/api/")
        && !request.uri().path().starts_with("/webhook/")
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

    let current_token = state.auth_token.read().unwrap().clone();
    match token {
        Some(t) if constant_time_eq(t, &current_token) => Ok(next.run(request).await),
        _ => {
            // Track auth failures for rate limiting
            tracing::warn!(
                path = %request.uri().path(),
                "rejected unauthenticated request"
            );
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// Constant-time string comparison to prevent timing attacks.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        // Still do the comparison to avoid leaking more timing info
        // than just the length difference.
        let dummy = "0".repeat(b.len());
        let _ = a
            .bytes()
            .chain(std::iter::repeat(0).take(b.len().saturating_sub(a.len())))
            .zip(dummy.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y));
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Generate a cryptographically random 32-byte hex token.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
