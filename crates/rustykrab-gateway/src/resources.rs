//! Cached infrastructure observations. The host owns probing and execution;
//! gateway reads are observational, and service actions enter the work graph.
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use chrono::{DateTime, Utc};
use rustykrab_core::work::{ArtifactRef, Budget, PlanOutcome, WorkItemDraft, WorkKind, WorkerKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SERVICE_TOOL: &str = "service_control";
pub const SERVICE_ACTION: &str = "service_action";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceObservation {
    pub id: String,
    pub role: String,
    pub version: Option<String>,
    pub healthy: Option<bool>,
    pub checked_at: Option<DateTime<Utc>>,
    pub process_id: Option<u32>,
    #[serde(default)]
    pub supervised: bool,
    #[serde(default)]
    pub identity_verified: bool,
    /// Probing established either supervisor ownership or a safely absent service.
    #[serde(default)]
    pub lifecycle_safe: bool,
    pub supervisor: String,
    pub ensure_running: bool,
    pub detail: String,
}

pub trait ResourceObserver: Send + Sync {
    /// Read the last probe, without advancing timestamps or doing I/O.
    fn services(&self) -> Vec<ServiceObservation>;
    fn registered(&self, id: &str) -> bool;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAction {
    pub resource: String,
    /// Only lifecycle operations on an operator-registered resource.
    pub action: String,
}
impl ServiceAction {
    pub fn valid(&self) -> bool {
        matches!(self.action.as_str(), "ensure_running" | "restart")
    }
    pub fn draft(&self) -> WorkItemDraft {
        WorkItemDraft {
            kind: Some(WorkKind::Personal),
            title: format!("{}: {}", self.action, self.resource),
            objective: format!("Apply {} to registered service {} and verify its health endpoint.",self.action,self.resource),
            done_when: "The registered service is running and a fresh health request succeeds. Record the lifecycle command and observation.".into(),
            artifact_refs: vec![ArtifactRef { kind: SERVICE_ACTION.into(), value: serde_json::to_string(self).expect("plain fields serialize") }],
            required_tools: vec![SERVICE_TOOL.into()],
            worker_kind: WorkerKind::Local,
            budget: Some(Budget { iterations: 3, tokens: 10_000, wall_seconds: 90, ..Default::default() }),
            ..Default::default()
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/resources", get(inventory))
        .route("/api/resources/{id}/actions", axum::routing::post(action))
}
async fn inventory(State(state): State<AppState>) -> Response {
    let result = async {
        let workers = match &state.workers { Some(r) => r.views().await?, None => vec![] };
        let projects = state.agent.store.projects().list().await?;
        let schedules = state.agent.store.jobs().list_jobs().await?;
        Ok::<_,rustykrab_core::Error>(json!({"work_manager":state.agent.work_manager,"workers":workers,"projects":projects,"schedules":schedules,"services":state.resources.as_ref().map(|r|r.services()).unwrap_or_default()}))
    }.await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"resources_unavailable"})),
        )
            .into_response(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    action: String,
}
async fn action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(principal): Extension<rustykrab_store::Principal>,
    Json(body): Json<ActionRequest>,
) -> Response {
    let requested = ServiceAction {
        resource: id,
        action: body.action,
    };
    if !requested.valid()
        || !state
            .resources
            .as_ref()
            .is_some_and(|r| r.registered(&requested.resource))
    {
        return (StatusCode::BAD_REQUEST,Json(json!({"error":"invalid_service_action","message":"Use ensure_running or restart on a registered service."}))).into_response();
    }
    let Some(control) = state.control.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match control
        .file_draft(
            requested.draft(),
            rustykrab_control::Provenance {
                conversation_id: None,
                filed_by_item: None,
                actor: format!("user:{}", principal.describe()),
            },
        )
        .await
    {
        Ok(outcome) => {
            let status = if matches!(outcome, PlanOutcome::Accepted(_)) {
                StatusCode::ACCEPTED
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            (status, Json(outcome)).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
