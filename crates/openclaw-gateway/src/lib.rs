pub mod auth;
pub mod origin;
pub mod rate_limit;
mod routes;
mod state;
mod telegram_webhook;
mod webchat;

pub use auth::generate_token;
pub use origin::OriginPolicy;
pub use rate_limit::RateLimitConfig;
pub use state::AppState;

use axum::middleware;
use axum::Router;

/// Build the main application router with all security middleware.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::api_routes())
        .merge(telegram_webhook::telegram_routes())
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
        .with_state(state)
}
