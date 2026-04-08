use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;

use crate::AppState;

pub fn telegram_routes() -> Router<AppState> {
    Router::new().route("/webhook/telegram", post(telegram_webhook))
}

/// Receives Telegram webhook updates, validates them, and forwards
/// to the TelegramChannel for processing.
///
/// Telegram sends a `X-Telegram-Bot-Api-Secret-Token` header that we
/// validate against the configured webhook secret. This prevents the
/// Telegram webhook validation failures from the original RustyKrab.
async fn telegram_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let telegram = match &state.telegram {
        Some(tg) => tg,
        None => {
            tracing::warn!("received Telegram webhook but no Telegram channel configured");
            return StatusCode::NOT_FOUND;
        }
    };

    let secret_header = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|v| v.to_str().ok());

    match telegram.parse_webhook_update(&body, secret_header).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::warn!("Telegram webhook rejected: {e}");
            StatusCode::FORBIDDEN
        }
    }
}
