//! `/api/workers`: the worker registry's HTTP surface
//! (`docs/plans/control-layer-and-worker-fleet.md`, sections 5 and 14).
//!
//! - `GET /api/workers` lists every named worker with its kind,
//!   capabilities, health, cost tier and routing record (`{ "workers": [..]
//!   }`), health checked on the way.
//! - `GET /api/workers/{name}` is one of them.
//! - `POST /api/workers` adds an external worker from a
//!   [`WorkerSpec`] (`kind`: `claude_code` or `codex`, optional `name`,
//!   `repos`, `command`, `model`, `allowed_tools`, `max_turns`,
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
