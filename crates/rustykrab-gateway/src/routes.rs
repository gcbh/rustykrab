use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use rustykrab_agent::AgentEvent;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};

use crate::AppState;

const MAX_MESSAGE_SIZE: usize = 100_000;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations", post(create_conversation))
        .route("/api/conversations", get(list_conversations))
        .route("/api/conversations/{id}", get(get_conversation))
        .route("/api/conversations/{id}", axum::routing::delete(delete_conversation))
        .route("/api/conversations/{id}/messages", post(send_message))
        .route("/api/conversations/{id}/messages/stream", post(send_message_stream))
        .route("/api/media/{project_id}/{filename}", get(serve_media))
        .route("/api/health", get(health))
        .route("/api/logout", post(logout))
}

#[derive(Deserialize)]
struct SendMessageRequest { content: String }

async fn health() -> &'static str { "ok" }

async fn logout(State(state): State<AppState>) -> StatusCode {
    let new_token = state.rotate_token();
    tracing::info!("auth token rotated via /api/logout");
    eprintln!("\n  New RUSTYKRAB_AUTH_TOKEN={new_token}\n");
    StatusCode::NO_CONTENT
}

async fn serve_media(
    State(state): State<AppState>,
    Path((project_id, filename)): Path<(String, String)>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::body::Body;
    use axum::http::header;
    let video = state.video.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    if project_id.contains("..") || filename.contains("..") || filename.contains('/') { return Err(StatusCode::BAD_REQUEST); }
    let projects_dir = &video.projects_dir();
    let file_path = projects_dir.join(&project_id).join(&filename);
    let canonical = file_path.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    let canonical_base = projects_dir.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !canonical.starts_with(&canonical_base) { return Err(StatusCode::FORBIDDEN); }
    if !canonical.is_file() { return Err(StatusCode::NOT_FOUND); }
    let content_type = match file_path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "mp4" => "video/mp4", "webm" => "video/webm", "wav" => "audio/wav",
        "mp3" => "audio/mpeg", "png" => "image/png", "jpg" | "jpeg" => "image/jpeg",
        "html" => "text/html", _ => "application/octet-stream",
    };
    let file_data = tokio::fs::read(&canonical).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let response = axum::http::Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, file_data.len())
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{filename}\""))
        .body(Body::from(file_data))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(response)
}

async fn create_conversation(State(state): State<AppState>) -> Result<Json<Conversation>, StatusCode> {
    state.store.conversations().create().map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_conversations(State(state): State<AppState>) -> Result<Json<Vec<Uuid>>, StatusCode> {
    state.store.conversations().list_ids().map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn get_conversation(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Conversation>, StatusCode> {
    state.store.conversations().get(id).map(Json).map_err(|_| StatusCode::NOT_FOUND)
}

async fn delete_conversation(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, StatusCode> {
    state.store.conversations().delete(id).map(|_| StatusCode::NO_CONTENT).map_err(|e| match e {
        rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    })
}

async fn send_message(State(state): State<AppState>, Path(id): Path<Uuid>, Json(body): Json<SendMessageRequest>) -> Result<Json<Message>, StatusCode> {
    if body.content.len() > MAX_MESSAGE_SIZE { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let mut conv = state.store.conversations().get(id).map_err(|_| StatusCode::NOT_FOUND)?;
    let user_content = body.content.clone();
    let user_msg = Message { id: Uuid::new_v4(), role: Role::User, content: MessageContent::Text(body.content), created_at: Utc::now() };
    conv.messages.push(user_msg);
    conv.updated_at = Utc::now();
    let assistant_msg = crate::orchestrate::run_agent(&state, &mut conv, &user_content).await?;
    conv.updated_at = Utc::now();
    state.store.conversations().save(&conv).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(assistant_msg))
}

enum SsePayload { Event(AgentEvent), Done(Result<Message, StatusCode>) }

async fn send_message_stream(State(state): State<AppState>, Path(id): Path<Uuid>, Json(body): Json<SendMessageRequest>) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if body.content.len() > MAX_MESSAGE_SIZE { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let mut conv = state.store.conversations().get(id).map_err(|_| StatusCode::NOT_FOUND)?;
    let user_content = body.content.clone();
    let user_msg = Message { id: Uuid::new_v4(), role: Role::User, content: MessageContent::Text(body.content), created_at: Utc::now() };
    conv.messages.push(user_msg);
    conv.updated_at = Utc::now();
    let (tx, rx) = tokio::sync::mpsc::channel::<SsePayload>(128);
    let agent_state = state.clone();
    let panic_tx = tx.clone();
    let agent_handle = tokio::spawn(async move {
        let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64));
        let hb = heartbeat.clone();
        let event_tx = tx.clone();
        let on_event = move |event: AgentEvent| {
            hb.store(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
            if let Err(e) = event_tx.try_send(SsePayload::Event(event)) { tracing::warn!("SSE event dropped (channel full): {e}"); }
        };
        let hb_monitor = heartbeat.clone();
        let timeout_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tf = timeout_flag.clone();
        let mut monitor = tokio::spawn(async move {
            const HEARTBEAT_TIMEOUT_MS: u64 = 300_000;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                let last = hb_monitor.load(std::sync::atomic::Ordering::Relaxed);
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                if now.saturating_sub(last) > HEARTBEAT_TIMEOUT_MS { tf.store(true, std::sync::atomic::Ordering::Relaxed); break; }
            }
        });
        let agent_future = crate::orchestrate::run_agent_streaming(&agent_state, &mut conv, &user_content, &on_event);
        let result = tokio::select! { r = agent_future => r, _ = &mut monitor => { tracing::warn!("streaming agent timed out (no activity for 5 minutes)"); Err(StatusCode::REQUEST_TIMEOUT) } };
        monitor.abort();
        conv.updated_at = Utc::now();
        if let Err(e) = agent_state.store.conversations().save(&conv) { tracing::error!("failed to save conversation: {e}"); }
        let _ = tx.send(SsePayload::Done(result)).await;
    });
    tokio::spawn(async move { if let Err(e) = agent_handle.await { tracing::error!("streaming agent task panicked: {e}"); let _ = panic_tx.send(SsePayload::Done(Err(StatusCode::INTERNAL_SERVER_ERROR))).await; } });
    let stream = ReceiverStream::new(rx).map(|payload| {
        let event = match payload {
            SsePayload::Event(agent_event) => match agent_event {
                AgentEvent::TextDelta(delta) => Event::default().event("delta").data(serde_json::json!({"type": "delta", "delta": delta}).to_string()),
                AgentEvent::ToolCallStart { tool_name, .. } => Event::default().event("tool_start").data(serde_json::json!({"type": "tool_start", "delta": tool_name}).to_string()),
                AgentEvent::ToolCallEnd { tool_name, success, error_message, .. } => { let t = if success { "tool_end" } else { "tool_error" }; let mut p = serde_json::json!({"type": t, "delta": tool_name}); if let Some(ref err) = error_message { p["error"] = serde_json::json!(err); } Event::default().event(t).data(p.to_string()) }
                AgentEvent::Reflecting => Event::default().event("thinking").data(serde_json::json!({"type": "thinking", "delta": "reflecting on errors"}).to_string()),
                AgentEvent::Compressing => Event::default().event("thinking").data(serde_json::json!({"type": "thinking", "delta": "compressing memory"}).to_string()),
                AgentEvent::Done => Event::default().event("done").data(serde_json::json!({"type": "done"}).to_string()),
            },
            SsePayload::Done(Ok(message)) => Event::default().event("done").data(serde_json::json!({"type": "done", "message": message}).to_string()),
            SsePayload::Done(Err(e)) => { tracing::error!(error = %e, "agent stream ended with error"); Event::default().event("error").data(serde_json::json!({"type": "error", "delta": format!("{e}")}).to_string()) }
        };
        Ok(event)
    });
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("ping")))
}
