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
        .route(
            "/api/conversations/{id}",
            axum::routing::delete(delete_conversation),
        )
        .route("/api/conversations/{id}/messages", post(send_message))
        .route(
            "/api/conversations/{id}/messages/stream",
            post(send_message_stream),
        )
        .route("/api/media/{project_id}/{filename}", get(serve_media))
        .route("/api/health", get(health))
        .route("/api/logout", post(logout))
}

#[derive(Deserialize)]
struct SendMessageRequest {
    content: String,
}

async fn health() -> &'static str {
    "ok"
}

async fn logout(State(state): State<AppState>) -> StatusCode {
    let new_token = state.rotate_token();
    tracing::info!("auth token rotated via /api/logout");
    // Print to stderr to avoid capture by structured logging infrastructure.
    eprintln!("\n  New RUSTYKRAB_AUTH_TOKEN={new_token}\n");
    StatusCode::NO_CONTENT
}

async fn serve_media(State(state): State<AppState>, Path((project_id, filename)): Path<(String, String)>) -> Result<axum::response::Response, StatusCode> {
    use axum::body::Body; use axum::http::header;
    let video = state.video.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    if project_id.contains("..") || filename.contains("..") || filename.contains('/') { return Err(StatusCode::BAD_REQUEST); }
    let base = video.projects_dir();
    let fp = base.join(&project_id).join(&filename);
    let c = fp.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    let cb = base.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !c.starts_with(&cb) { return Err(StatusCode::FORBIDDEN); }
    if !c.is_file() { return Err(StatusCode::NOT_FOUND); }
    let ct = match fp.extension().and_then(|e| e.to_str()).unwrap_or("") { "mp4"=>"video/mp4","webm"=>"video/webm","wav"=>"audio/wav","mp3"=>"audio/mpeg","png"=>"image/png","jpg"|"jpeg"=>"image/jpeg","html"=>"text/html",_=>"application/octet-stream" };
    let d = tokio::fs::read(&c).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    axum::http::Response::builder().header(header::CONTENT_TYPE,ct).header(header::CONTENT_LENGTH,d.len()).header(header::CONTENT_DISPOSITION,format!("inline; filename=\"{filename}\"")).body(Body::from(d)).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn create_conversation(
    State(state): State<AppState>,
) -> Result<Json<Conversation>, StatusCode> {
    state
        .store
        .conversations()
        .create()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_conversations(State(state): State<AppState>) -> Result<Json<Vec<Uuid>>, StatusCode> {
    state
        .store
        .conversations()
        .list_ids()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn get_conversation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Conversation>, StatusCode> {
    state
        .store
        .conversations()
        .get(id)
        .map(Json)
        .map_err(|_| StatusCode::NOT_FOUND)
}

async fn delete_conversation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    state
        .store
        .conversations()
        .delete(id)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| match e {
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<Message>, StatusCode> {
    if body.content.len() > MAX_MESSAGE_SIZE {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Load the conversation.
    let mut conv = state
        .store
        .conversations()
        .get(id)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Clone content before moving into the message (needed for profile classification).
    let user_content = body.content.clone();

    // Add the user message.
    let user_msg = Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(body.content),
        created_at: Utc::now(),
    };
    conv.messages.push(user_msg);
    conv.updated_at = Utc::now();

    // Run the full agent pipeline.
    let assistant_msg = crate::orchestrate::run_agent(&state, &mut conv, &user_content).await?;

    // Persist the full conversation (including intermediate tool call messages).
    conv.updated_at = Utc::now();
    state
        .store
        .conversations()
        .save(&conv)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assistant_msg))
}

enum SsePayload {
    Event(AgentEvent),
    Done(Result<Message, StatusCode>),
}

include!("routes_ext.rs");
