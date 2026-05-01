use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

/// Newtype wrapper so handlers can extract the trace id from request
/// extensions without colliding with bare `Uuid` extensions.
#[derive(Debug, Clone, Copy)]
pub struct TraceId(pub Uuid);

/// HTTP request/response logging middleware.
///
/// Logs method, path, status, latency, client IP, and content lengths
/// for every request. Skips `/api/health` to reduce noise.
/// Uses `info!` for successful responses and `warn!` for 4xx/5xx.
///
/// Generates a `trace_id` per request and stashes it in request extensions
/// so downstream handlers can thread it into agent runs — prompt-log rows
/// and agent-loop logs then share the same id as the HTTP completion log.
pub async fn request_logging_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();

    // Skip health checks — too noisy.
    if path == "/api/health" {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let query = request.uri().query().map(|q| q.to_owned());
    let trace_id = Uuid::new_v4();
    request.extensions_mut().insert(TraceId(trace_id));
    let client_ip = addr.ip();
    let user_agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();
    let req_content_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let start = Instant::now();
    let response = next.run(request).await;
    let latency_ms = start.elapsed().as_millis();

    let status = response.status().as_u16();
    let resp_content_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    if status >= 400 {
        tracing::warn!(
            %trace_id,
            %method,
            %path,
            ?query,
            %client_ip,
            %user_agent,
            ?req_content_length,
            status,
            ?resp_content_length,
            latency_ms,
            "request completed"
        );
    } else {
        tracing::info!(
            %trace_id,
            %method,
            %path,
            ?query,
            %client_ip,
            %user_agent,
            ?req_content_length,
            status,
            ?resp_content_length,
            latency_ms,
            "request completed"
        );
    }

    response
}
