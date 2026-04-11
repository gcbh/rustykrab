pub mod auth;
mod logging;
mod orchestrate;
pub mod origin;
pub mod rate_limit;
mod routes;
mod signal_webhook;
mod state;
mod telegram_webhook;
mod webchat;

pub use auth::generate_token;
pub use orchestrate::{run_agent, run_agent_streaming};
pub use origin::OriginPolicy;
pub use rate_limit::RateLimitConfig;
pub use state::AppState;

use axum::extract::Request;
use axum::http::header;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;

/// Middleware that adds security headers to every response, including
/// error responses from auth/origin/rate-limit middleware.
async fn security_headers_middleware(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(
        header::HeaderName::from_static("x-xss-protection"),
        "1; mode=block".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:"
            .parse()
            .unwrap(),
    );
    response
}

/// Build the main application router with all security middleware.
///
/// Security headers are applied as the outermost middleware so they
/// cover all responses, including errors from auth/origin/rate-limit.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::api_routes())
        .merge(telegram_webhook::telegram_routes())
        .merge(signal_webhook::signal_routes())
        .merge(webchat::static_routes())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            origin::origin_check_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit::rate_limit_middleware,
        ))
        .layer(middleware::from_fn(logging::request_logging_middleware))
        .layer(middleware::from_fn(security_headers_middleware))
        .with_state(state)
}
