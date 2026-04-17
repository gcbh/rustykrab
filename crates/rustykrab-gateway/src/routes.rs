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
async fn logout(State(state): State<AppState>) -> StatusCode { let t = state.rotate_token(); tracing::info!("token rotated"); eprintln!("\n  New RUSTYKRAB_AUTH_TOKEN={t}\n"); StatusCode::NO_CONTENT }

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

include!("routes_ext.rs");
