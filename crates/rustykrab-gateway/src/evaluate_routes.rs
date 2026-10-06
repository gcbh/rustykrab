//! `/api/work/evaluate` and `/api/work/metrics`: the control plan's
//! evaluation (`docs/plans/control-layer-and-worker-fleet.md`, sections 1.1,
//! 10 and 11, Phase 6).
//!
//! The nightly pass runs on a timer in the daemon; `POST /api/work/evaluate`
//! runs it now, through the [`EvaluationHandle`] the composition root wires
//! into [`AppState`]: the expectation metrics, the section 10 criteria and
//! the proposals they file, the review surface's decisions synced back and
//! its projection written out. It answers with the pass's
//! [`EvaluationReport`], or 503 `evaluation_unavailable` when no handle is
//! wired. `GET /api/work/metrics` reads the newest pass's metrics straight
//! from the store, `{ "metrics": [MetricValue], "computed_at" }`, empty
//! before the first pass.

use std::future::Future;
use std::pin::Pin;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use rustykrab_core::proposal::{EvaluationReport, MetricValue};
use rustykrab_core::Error;

use crate::AppState;

/// The future [`EvaluationHandle::evaluate`] returns. Spelled out rather
/// than through `async-trait`, which the gateway takes only as a test
/// dependency.
pub type EvaluationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<EvaluationReport, Error>> + Send + 'a>>;

/// One evaluation pass on demand, as the composition root assembles it
/// (the review surface's decisions, dreaming's pass, the projection).
pub type DreamingFuture<'a> = Pin<
    Box<dyn Future<Output = Result<rustykrab_core::dream_review::DreamingView, Error>> + Send + 'a>,
>;
pub trait EvaluationHandle: Send + Sync {
    fn evaluate(&self) -> EvaluationFuture<'_>;
    fn dreaming_status(&self) -> DreamingFuture<'_> {
        Box::pin(async { Err(Error::NotFound("Project dreaming is unavailable".into())) })
    }
    fn dreaming_run(&self) -> DreamingFuture<'_> {
        self.dreaming_status()
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/work/evaluate", post(evaluate))
        .route("/api/work/metrics", get(metrics))
        .route("/api/dreaming", get(dreaming_status).post(dreaming_run))
        .route("/api/dreaming/reviews/{id}", get(dreaming_receipt))
}

async fn dreaming_status(State(state): State<AppState>) -> Response {
    let Some(h) = state.evaluation.as_ref() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "dreaming_unavailable",
            "Dreaming is unavailable".into(),
        );
    };
    match h.dreaming_status().await {
        Ok(v) => ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Err(e) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "dreaming_unavailable",
            e.to_string(),
        ),
    }
}
async fn dreaming_run(State(state): State<AppState>) -> Response {
    let Some(h) = state.evaluation.as_ref() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "dreaming_unavailable",
            "Dreaming is unavailable".into(),
        );
    };
    match h.dreaming_run().await {
        Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Err(e) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "dreaming_failed",
            e.to_string(),
        ),
    }
}
async fn dreaming_receipt(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.agent.store.dream_review_get(&id).await {
        Ok(Some(v)) => ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Ok(None) => failure(
            StatusCode::NOT_FOUND,
            "review_not_found",
            "Review not found".into(),
        ),
        Err(e) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "review_unavailable",
            e.to_string(),
        ),
    }
}

/// `GET /api/work/metrics`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsReply {
    pub metrics: Vec<MetricValue>,
    /// When the newest pass computed them; `None` before the first.
    pub computed_at: Option<DateTime<Utc>>,
}

fn failure(status: StatusCode, code: &str, message: String) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

async fn evaluate(State(state): State<AppState>) -> Response {
    let Some(handle) = state.evaluation.clone() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "evaluation_unavailable",
            "the evaluation pass is not running in this daemon".to_string(),
        );
    };
    match handle.evaluate().await {
        Ok(report) => Json(report).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "evaluation pass failed");
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "evaluation_failed",
                e.to_string(),
            )
        }
    }
}

async fn metrics(State(state): State<AppState>) -> Response {
    match state.agent.store.expectation_metrics_latest().await {
        Ok(metrics) => {
            let computed_at = metrics.iter().map(|m| m.computed_at).max();
            Json(MetricsReply {
                metrics,
                computed_at,
            })
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "reading expectation metrics failed");
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "could not read the metrics".to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use reqwest::header::{HeaderMap, HeaderValue, ORIGIN};
    use rustykrab_core::model::{ModelProvider, ModelResponse};
    use rustykrab_core::outcome::SignalClass;
    use rustykrab_core::proposal::Direction;
    use rustykrab_core::types::{Message, ToolSchema};
    use serde_json::Value;

    use super::*;

    const TOKEN: &str = "evaluate-routes-test-token";

    struct UnusedProvider;

    #[async_trait]
    impl ModelProvider for UnusedProvider {
        fn name(&self) -> &str {
            "unused"
        }

        async fn chat(
            &self,
            _: &[Message],
            _: &[ToolSchema],
        ) -> rustykrab_core::Result<ModelResponse> {
            Err(Error::ModelProvider("not used by these tests".into()))
        }
    }

    struct Canned;

    impl EvaluationHandle for Canned {
        fn evaluate(&self) -> EvaluationFuture<'_> {
            Box::pin(async {
                Ok(EvaluationReport {
                    at: Utc::now(),
                    metrics: vec![],
                    regressions: vec!["unknown_error_rate".into()],
                    filed: vec![],
                    skipped: vec![],
                    decisions: vec![],
                    probation: vec![],
                    projection: None,
                })
            })
        }
    }

    fn metric(name: &str) -> MetricValue {
        MetricValue {
            name: name.into(),
            expectation: "Know what went wrong".into(),
            direction: Direction::ToZero,
            unit: "rate".into(),
            signal: SignalClass::Verifiable,
            value: 0.25,
            numerator: 1.0,
            denominator: 4.0,
            sample: 4,
            computed_at: Utc::now(),
            window_days: 7,
            breakdown: vec![],
        }
    }

    /// A real router on a loopback port, behind the real middleware.
    async fn serve(
        handle: Option<Arc<dyn EvaluationHandle>>,
    ) -> (String, reqwest::Client, rustykrab_store::Store) {
        let dir = std::env::temp_dir().join(format!("rk-evaluate-routes-{}", uuid::Uuid::new_v4()));
        let store = rustykrab_store::Store::open(&dir, vec![9u8; 32]).expect("store opens");
        let mut state = AppState::new(
            store.clone(),
            vec![],
            Arc::new(UnusedProvider),
            TOKEN.into(),
        );
        if let Some(h) = handle {
            state = state.with_evaluation(h);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::router(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let base = format!("http://{addr}");
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&base).unwrap());
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        (base, client, store)
    }

    #[tokio::test]
    async fn evaluate_answers_503_without_a_handle_and_the_report_with_one() {
        let (base, client, _) = serve(None).await;
        let r = client
            .post(format!("{base}/api/work/evaluate"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 503);
        let (base, client, _) = serve(Some(Arc::new(Canned))).await;
        let r = client
            .post(format!("{base}/api/work/evaluate"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["regressions"][0], "unknown_error_rate");
    }

    #[tokio::test]
    async fn metrics_serve_the_newest_pass_from_the_store() {
        let (base, client, store) = serve(None).await;
        let get = || async {
            let r = client
                .get(format!("{base}/api/work/metrics"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 200);
            r.json::<Value>().await.unwrap()
        };
        assert_eq!(get().await["metrics"], serde_json::json!([]));
        store
            .expectation_metrics_record("p1", &[metric("unknown_error_rate")])
            .await
            .unwrap();
        let body = get().await;
        assert_eq!(body["metrics"][0]["name"], "unknown_error_rate");
        assert_eq!(body["metrics"][0]["value"], 0.25);
        assert!(body["computed_at"].is_string());
    }
}
