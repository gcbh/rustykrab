use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use openclaw_core::types::{Conversation, Message, MessageContent, Role};

use crate::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations", post(create_conversation))
        .route("/api/conversations", get(list_conversations))
        .route("/api/conversations/{id}", get(get_conversation))
        .route("/api/conversations/{id}", axum::routing::delete(delete_conversation))
        .route("/api/conversations/{id}/messages", post(send_message))
        .route("/api/health", get(health))
}

#[derive(Deserialize)]
struct SendMessageRequest {
    content: String,
}

async fn health() -> &'static str {
    "ok"
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

async fn list_conversations(
    State(state): State<AppState>,
) -> Result<Json<Vec<Uuid>>, StatusCode> {
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
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Send a user message to a conversation and get an assistant response.
async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<Message>, StatusCode> {
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
    let assistant_msg =
        crate::orchestrate::run_agent(&state, &mut conv, &user_content).await?;

    // Persist the full conversation (including intermediate tool call messages).
    conv.updated_at = Utc::now();
    state
        .store
        .conversations()
        .save(&conv)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assistant_msg))
}
