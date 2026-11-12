use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::http::{header, Method};
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
/// A completed request is logged when it completes, so a request that
/// never does would leave no trace: a mutating `/api/` request (anything
/// but `GET`, `HEAD` and `OPTIONS`) is also logged when it starts, and any
/// request whose client stops waiting before the response (its future is
/// dropped) is logged as abandoned, with how long it had run.
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

    if path.starts_with("/api/") && !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS)
    {
        tracing::info!(%trace_id, %method, %path, %client_ip, "request started");
    }

    let start = Instant::now();
    let mut pending = Abandoned {
        trace_id,
        method: method.clone(),
        path: path.clone(),
        start,
        armed: true,
    };
    let response = next.run(request).await;
    pending.armed = false;
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

/// Logs a request whose future was dropped before it produced a response:
/// its client disconnected or timed out. Disarmed once a response exists.
struct Abandoned {
    trace_id: Uuid,
    method: Method,
    path: String,
    start: Instant,
    armed: bool,
}

impl Drop for Abandoned {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(
                trace_id = %self.trace_id,
                method = %self.method,
                path = %self.path,
                elapsed_ms = self.start.elapsed().as_millis(),
                "request abandoned before a response"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::routing::post;
    use axum::{middleware, Router};
    use tower::ServiceExt;

    use super::request_logging_middleware;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn request(method: &str, path: &str) -> axum::http::Request<Body> {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))));
        request
    }

    /// The client gives up on a request the server is still working on
    /// (a cancel waiting for the control loop): its future is dropped, and
    /// the log shows that it started and that it was abandoned.
    #[tokio::test]
    async fn an_abandoned_request_is_logged_with_its_elapsed_time() {
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        let _default = tracing::subscriber::set_default(subscriber);
        let app = Router::new()
            .route(
                "/api/work/{id}/cancel",
                post(|| async {
                    std::future::pending::<()>().await;
                }),
            )
            .route("/api/fast", post(|| async { "ok" }))
            .layer(middleware::from_fn(request_logging_middleware));

        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            app.clone().oneshot(request("POST", "/api/work/w1/cancel")),
        )
        .await;
        assert!(outcome.is_err(), "the handler never answers");
        let fast = app.oneshot(request("POST", "/api/fast")).await.unwrap();
        assert_eq!(fast.status(), 200);

        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let line = |needle: &str| {
            text.lines()
                .find(|l| l.contains(needle) && l.contains("/api/work/w1/cancel"))
                .unwrap_or_else(|| panic!("no {needle:?} line in:\n{text}"))
                .to_string()
        };
        assert!(line("request started").contains("POST"));
        let abandoned = line("request abandoned before a response");
        assert!(abandoned.contains("elapsed_ms="), "{abandoned}");
        assert!(
            !text
                .lines()
                .any(|l| l.contains("abandoned") && l.contains("/api/fast")),
            "a completed request is not abandoned:\n{text}"
        );
        assert!(text
            .lines()
            .any(|l| l.contains("request completed") && l.contains("/api/fast")));
    }
}
