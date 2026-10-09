//! Schedule commands for the work manager. Firings enter the existing controller.
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rustykrab_core::{work::CronExecution, Error};
use serde::Deserialize;
use serde_json::json;
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/schedules", get(list).post(create))
        .route("/api/schedules/{id}", get(detail).delete(remove))
        .route("/api/schedules/{id}/enabled", axum::routing::post(enabled))
}
fn failure(e: Error) -> Response {
    let code = match &e {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::AlreadyExists(_) => StatusCode::CONFLICT,
        Error::Config(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (code,Json(json!({"error":"schedule_command_failed","message":if code==StatusCode::SERVICE_UNAVAILABLE { "Schedule storage is unavailable.".into() } else { e.to_string() }}))).into_response()
}
async fn list(State(s): State<AppState>) -> Response {
    match s.agent.store.jobs().list_jobs().await {
        Ok(j) => Json(json!({"managed":s.agent.work_manager,"schedules":j})).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    schedule: String,
    task: String,
    timezone: Option<String>,
    execution: Option<CronExecution>,
    channel: Option<String>,
    chat_id: Option<String>,
    thread_id: Option<String>,
    #[serde(default)]
    allow_duplicate: bool,
}
async fn create(State(s): State<AppState>, Json(b): Json<Create>) -> Response {
    if !s.agent.work_manager {
        return (StatusCode::CONFLICT,Json(json!({"error":"work_manager_disabled","message":"Enable the work manager before creating centrally managed schedules."}))).into_response();
    }
    if b.task.trim().is_empty() || b.task.len() > 32 * 1024 || b.schedule.len() > 256 {
        return failure(Error::Config(
            "Schedule and non-empty task must fit their bounds.".into(),
        ));
    }
    let zone = match b.timezone {
        Some(z) => z,
        None => rustykrab_core::timezone::configured().name().into(),
    };
    match s
        .agent
        .store
        .jobs()
        .create_managed_job(
            &b.schedule,
            &b.task,
            b.channel.as_deref(),
            b.chat_id.as_deref(),
            b.thread_id.as_deref(),
            &zone,
            b.allow_duplicate,
            b.execution,
        )
        .await
    {
        Ok(j) => (StatusCode::CREATED, Json(j)).into_response(),
        Err(e) => failure(e),
    }
}
async fn detail(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let jobs = s.agent.store.jobs();
    match jobs.get_job(&id).await {
        Ok(j) => match jobs.list_runs(&id, 20).await {
            Ok(r) => {
                let item = jobs.work_item_id(&id).await.ok().flatten();
                let delivery = match &item {
                    Some(i) => s
                        .agent
                        .store
                        .work_evidence_list(i)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| {
                            matches!(
                                e.kind.as_str(),
                                "scheduled_delivery_attempt" | "scheduled_delivery"
                            )
                        })
                        .collect::<Vec<_>>(),
                    None => vec![],
                };
                Json(json!({"schedule":j,"work_item_id":item,"runs":r,"delivery":delivery}))
                    .into_response()
            }
            Err(e) => failure(e),
        },
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}
async fn enabled(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<Enabled>,
) -> Response {
    if b.enabled && !s.agent.work_manager {
        return failure(Error::Config("Work manager must be enabled.".into()));
    }
    match s.agent.store.jobs().get_job(&id).await {
        Ok(_) => match s.agent.store.jobs().set_enabled(&id, b.enabled).await {
            Ok(()) => Json(json!({"enabled":b.enabled})).into_response(),
            Err(e) => failure(e),
        },
        Err(e) => failure(e),
    }
}
async fn remove(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match s.agent.store.jobs().delete_job(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => failure(e),
    }
}
