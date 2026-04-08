use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;

use crate::AppState;

pub fn signal_routes() -> Router<AppState> {
    Router::new().route("/webhook/signal", post(signal_webhook))
}

/// Receives webhook payloads from signal-cli-rest-api.
///
/// signal-cli-rest-api POSTs incoming messages as JSON envelopes.
/// We validate an optional shared secret header before processing.
async fn signal_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let signal = match &state.signal {
        Some(s) => s,
        None => {
            tracing::warn!("received Signal webhook but no Signal channel configured");
            return StatusCode::NOT_FOUND;
        }
    };

    let secret_header = headers
        .get("x-signal-webhook-secret")
        .and_then(|v| v.to_str().ok());

    match signal.parse_webhook_payload(&body, secret_header).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::warn!("Signal webhook rejected: {e}");
            StatusCode::FORBIDDEN
        }
    }
}
