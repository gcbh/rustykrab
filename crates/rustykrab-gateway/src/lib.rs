pub mod auth;
mod credential_page;
pub mod evaluate_routes;
pub mod logging;
pub mod monitor_routes;
pub mod origin;
mod payment_page;
mod project_routes;
pub mod push;
pub mod question_routes;
pub mod rate_limit;
mod routes;
mod schedule_routes;
mod signal_webhook;
mod state;
mod tailnet_auth;
pub mod tasks;
mod telegram_webhook;
mod version_route;
mod webchat;
pub mod work_routes;
pub mod worker_routes;

pub use auth::generate_token;
pub mod resources;
pub mod run;
pub use origin::OriginPolicy;
pub use push::{ApnsClient, ApnsConfig, ApnsEnvironment, PushNotifier};
pub use rate_limit::RateLimitConfig;
pub use run::{
    run_agent, run_agent_interactive, run_agent_streaming, run_agent_streaming_with_options,
    run_agent_with_options,
};
pub use rustykrab_runtime::{AgentContext, RunOptions, RuntimeError};
pub use state::{AppState, BuildInfo};
pub use tailnet_auth::TailnetAuthPolicy;
pub use tasks::{run_task_worker, TaskQueueSignal};

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
    headers.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::HeaderName::from_static("x-xss-protection"),
        header::HeaderValue::from_static("1; mode=block"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:",
        ),
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
        .merge(tailnet_auth::routes())
        .merge(version_route::routes())
        .merge(monitor_routes::routes())
        .merge(resources::routes())
        .merge(schedule_routes::routes())
        .merge(project_routes::routes())
        .merge(work_routes::routes())
        .merge(worker_routes::routes())
        .merge(evaluate_routes::routes())
        .merge(question_routes::routes())
        .merge(telegram_webhook::telegram_routes())
        .merge(signal_webhook::signal_routes())
        .merge(credential_page::routes())
        .merge(payment_page::routes())
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

pub use credential_page::PageIdentityPolicy;
