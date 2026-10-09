//! Private dated-note reads. Writes belong to the scoped integration worker.
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rustykrab_core::Error;
use serde_json::json;
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/briefings", get(list))
        .route("/api/briefings/{date}", get(read))
}
fn response(result: rustykrab_core::Result<serde_json::Value>) -> Response {
    let mut reply = match result {
        Ok(v) => Json(v).into_response(),
        Err(Error::NotFound(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"briefing_unavailable"})),
        )
            .into_response(),
    };
    reply.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    reply
}
async fn list(State(s): State<AppState>) -> Response {
    let Some(v) = s.briefing_vault else {
        return response(Ok(json!({"configured":false,"notes":[]})));
    };
    response(
        tokio::task::spawn_blocking(move || {
            v.list()
                .map(|notes| json!({"configured":true,"notes":notes}))
        })
        .await
        .unwrap_or_else(|_| Err(Error::Internal("vault read failed".into()))),
    )
}
async fn read(State(s): State<AppState>, Path(date): Path<String>) -> Response {
    let Some(v) = s.briefing_vault else {
        return response(Err(Error::NotFound("Briefing vault".into())));
    };
    response(
        tokio::task::spawn_blocking(move || {
            v.read(&date).and_then(|n| {
                serde_json::to_value(n).map_err(|_| Error::Internal("note serialization".into()))
            })
        })
        .await
        .unwrap_or_else(|_| Err(Error::Internal("vault read failed".into()))),
    )
}
