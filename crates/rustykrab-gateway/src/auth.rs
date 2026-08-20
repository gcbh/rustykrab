use axum::extract::Request;
use axum::http::{header, StatusCode};
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

    // Pairing is the one authenticated-adjacent exception: a device has no
    // token yet, which is the entire point of pairing. It is protected by
    // the code itself — single use, five-minute TTL, hashed at rest — and
    // by the rate-limit middleware in front of this one.
    if request.uri().path() == "/api/pair" {
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
    let is_master = {
        let token_guard = state.auth_token.read().unwrap_or_else(|e| e.into_inner());
        token.is_some_and(|t| constant_time_eq(t, &token_guard))
    };

    // A per-device token is accepted anywhere the master token is. The
    // difference is attribution: decisions record which device made them,
    // and a single device can be revoked without rotating everything.
    let principal = if is_master {
        Some(rustykrab_store::Principal::Master)
    } else if let Some(candidate) = token {
        match state.store.devices().authenticate(candidate).await {
            Ok(found) => found,
            Err(e) => {
                tracing::error!(error = %e, "device token lookup failed");
                None
            }
        }
    } else {
        None
    };

    match principal {
        Some(principal) => {
            // Downstream handlers read this to attribute what they do.
            let mut request = request;
            request.extensions_mut().insert(principal);
            Ok(next.run(request).await)
        }
        None => {
            tracing::warn!(
                path = %request.uri().path(),
                "rejected unauthenticated request"
            );
            Err(StatusCode::UNAUTHORIZED)
        }
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
