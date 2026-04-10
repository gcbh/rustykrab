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

use axum::http::header;
use axum::middleware;
use axum::response::Response;
use axum::Router;

/// Add security headers to every response.
async fn add_security_headers(mut response: Response) -> Response {
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
        .with_state(state)
        .layer(middleware::map_response(add_security_headers))
}
