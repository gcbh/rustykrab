//! `/api/workers`: the worker registry's HTTP surface
//! (`docs/plans/control-layer-and-worker-fleet.md`, sections 5 and 14).
//!
//! - `GET /api/workers` lists every named worker with its kind,
//!   capabilities, health, cost tier and routing record (`{ "workers": [..]
//!   }`), health checked on the way.
//! - `GET /api/workers/{name}` is one of them.
//! - `POST /api/workers` adds an external worker from a
//!   [`WorkerSpec`] (`kind`: `claude_code` or `codex`, optional `name`,
//!   `repos`, `command`, `model`, `allowed_tools`, `denied_tools`, `max_turns`,
//!   `permission_mode`, `timeout_seconds`, `concurrency`, `cost_tier`,
//!   `env`): 201 with the worker, 409 when the name is taken, 400 when the
//!   spec is refused.
//! - `DELETE /api/workers/{name}` removes an external worker; its history
//!   keeps the name.
//!
//! The routing record is written by the controller from verified results
//! and is read-only here: no route sets it, and moving a class's default
//! tier is a proposal (Phase 6). Without a registry wired into
//! [`AppState`], every route answers 503 `workers_unavailable`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use rustykrab_control::registry::{WorkerRegistry, WorkerSpec, WorkerView};
use rustykrab_core::Error;

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workers", get(list).post(add))
        .route("/api/workers/{name}", get(one).delete(remove))
}

/// `GET /api/workers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerList {
    pub workers: Vec<WorkerView>,
}

/// `DELETE /api/workers/{name}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Removed {
    pub name: String,
    pub removed: bool,
}

struct ApiError(StatusCode, &'static str, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1, "message": self.2 }))).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::AlreadyExists(m) => ApiError(StatusCode::CONFLICT, "conflict", m),
            Error::NotFound(m) => ApiError(StatusCode::NOT_FOUND, "not_found", m),
            Error::Config(m) => ApiError(StatusCode::BAD_REQUEST, "invalid_request", m),
            other => {
                tracing::error!(error = %other, "worker API operation failed");
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "worker operation failed".into(),
                )
            }
        }
    }
}

fn registry(state: &AppState) -> Result<&Arc<WorkerRegistry>, ApiError> {
    state.workers.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "workers_unavailable",
            "the worker registry is not running in this daemon".into(),
        )
    })
}

async fn list(State(state): State<AppState>) -> Result<Json<WorkerList>, ApiError> {
    let workers = registry(&state)?.views().await?;
    Ok(Json(WorkerList { workers }))
}

async fn one(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<WorkerView>, ApiError> {
    registry(&state)?
        .view(&name)
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no worker {name}"),
            )
        })
}

async fn add(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<WorkerView>), ApiError> {
    let spec: WorkerSpec = serde_json::from_slice(&body).map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("not a worker spec: {e}"),
        )
    })?;
    let view = registry(&state)?.add(spec).await?;
    tracing::info!(worker = %view.name, kind = %view.kind, "worker added");
    Ok((StatusCode::CREATED, Json(view)))
}

async fn remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Removed>, ApiError> {
    let removed = registry(&state)?.remove(&name).await?;
    if !removed {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no worker {name}"),
        ));
    }
    Ok(Json(Removed { name, removed }))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use async_trait::async_trait;
    use reqwest::header::{HeaderMap, HeaderValue, ORIGIN};
    use rustykrab_control::registry::WorkerFactory;
    use rustykrab_control::worker::{Brief, Worker, WorkerCapabilities};
    use rustykrab_core::model::{ModelProvider, ModelResponse};
    use rustykrab_core::types::{Message, ToolSchema};
    use rustykrab_core::work::{ResultReport, WorkerKind};
    use serde_json::Value;

    use super::*;

    const TOKEN: &str = "worker-routes-test-token";

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

    struct Idle {
        name: String,
        kind: WorkerKind,
    }

    #[async_trait]
    impl Worker for Idle {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> WorkerKind {
            self.kind
        }
        fn capabilities(&self) -> WorkerCapabilities {
            WorkerCapabilities::default()
        }
        async fn run(&self, _brief: Brief) -> Result<ResultReport, Error> {
            Ok(ResultReport::default())
        }
    }

    struct Factory;

    impl WorkerFactory for Factory {
        fn build(&self, name: &str, spec: &WorkerSpec) -> Result<Arc<dyn Worker>, String> {
            Ok(Arc::new(Idle {
                name: name.to_string(),
                kind: spec.kind,
            }))
        }
    }

    /// A real router on a loopback port, behind the real middleware, with
    /// a registry whose workers are never run.
    async fn serve() -> (String, reqwest::Client) {
        let dir = std::env::temp_dir().join(format!("rk-worker-routes-{}", uuid::Uuid::new_v4()));
        let store = rustykrab_store::Store::open(&dir, vec![9u8; 32]).expect("store opens");
        let registry = WorkerRegistry::new(store.clone()).with_factory(Arc::new(Factory));
        let state = AppState::new(store, vec![], Arc::new(UnusedProvider), TOKEN.into())
            .with_workers(Arc::new(registry));
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
        (base, client)
    }

    #[tokio::test]
    async fn denied_tools_round_trip_through_add_and_show() {
        let (base, client) = serve().await;
        let add = |body: Value| {
            let request = client
                .post(format!("{base}/api/workers"))
                .bearer_auth(TOKEN)
                .json(&body);
            async move {
                let r = request.send().await.unwrap();
                assert_eq!(r.status().as_u16(), 201);
                r.json::<Value>().await.unwrap()
            }
        };
        let denied = json!(["Read(~/.config/**)", "Edit(//Users/someone/secrets/**)"]);
        let added = add(json!({
            "kind": "claude_code",
            "name": "pinch",
            "repos": ["/src/app"],
            "denied_tools": denied,
        }))
        .await;
        assert_eq!(added["spec"]["denied_tools"], denied);
        let plain = add(json!({
            "kind": "claude_code",
            "name": "coral",
            "repos": ["/src/app"],
        }))
        .await;
        assert!(plain["spec"].get("denied_tools").is_none(), "{plain}");

        let shown: Value = client
            .get(format!("{base}/api/workers/pinch"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(shown["spec"]["denied_tools"], denied);
        let view: WorkerView = serde_json::from_value(shown).unwrap();
        assert_eq!(
            view.spec.unwrap().denied_tools,
            ["Read(~/.config/**)", "Edit(//Users/someone/secrets/**)"]
        );
    }
}
