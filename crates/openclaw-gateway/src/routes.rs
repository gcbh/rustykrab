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

use openclaw_agent::AgentEvent;
use openclaw_core::types::{Conversation, Message, MessageContent, Role};

use crate::AppState;

/// Maximum allowed message size in bytes (100 KB).
const MAX_MESSAGE_SIZE: usize = 100_000;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations", post(create_conversation))
        .route("/api/conversations", get(list_conversations))
        .route("/api/conversations/{id}", get(get_conversation))
        .route("/api/conversations/{id}", axum::routing::delete(delete_conversation))
        .route("/api/conversations/{id}/messages", post(send_message))
        .route(
            "/api/conversations/{id}/messages/stream",
            post(send_message_stream),
        )
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

/// Rotate the auth token, invalidating the current session.
/// The new token is printed to the server's stdout so the operator can
/// retrieve it. The old token is immediately invalid.
async fn logout(State(state): State<AppState>) -> StatusCode {
    let new_token = state.rotate_token();
    tracing::info!("auth token rotated via /api/logout");
    println!("\n  New OPENCLAW_AUTH_TOKEN={new_token}\n");
    StatusCode::NO_CONTENT
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

/// Payload sent through the MPSC channel from the agent task to the SSE stream.
enum SsePayload {
    Event(AgentEvent),
    Done(Result<Message, StatusCode>),
}

/// Send a user message and stream the assistant response as SSE events.
async fn send_message_stream(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if body.content.len() > MAX_MESSAGE_SIZE {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Load the conversation.
    let mut conv = state
        .store
        .conversations()
        .get(id)
        .map_err(|_| StatusCode::NOT_FOUND)?;

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

    // Channel for streaming events from the agent task to the SSE response.
    let (tx, rx) = tokio::sync::mpsc::channel::<SsePayload>(128);

    // Spawn the agent loop in a background task with a 30-minute timeout.
    // Local models (Ollama) can take 1-5 minutes per LLM call, and multi-step
    // tool-use conversations may need 5+ iterations.
    let agent_state = state.clone();
    tokio::spawn(async move {
        let event_tx = tx.clone();
        let on_event = move |event: AgentEvent| {
            let _ = event_tx.try_send(SsePayload::Event(event));
        };

        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(1800),
            crate::orchestrate::run_agent_streaming(
                &agent_state,
                &mut conv,
                &user_content,
                &on_event,
            ),
        )
        .await;

        let result = match result {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!("streaming agent timed out after 1800s");
                Err(StatusCode::REQUEST_TIMEOUT)
            }
        };

        // Persist conversation on success.
        if result.is_ok() {
            conv.updated_at = Utc::now();
            let _ = agent_state.store.conversations().save(&conv);
        }

        let _ = tx.send(SsePayload::Done(result)).await;
    });

    // Map channel messages to SSE events.
    let stream = ReceiverStream::new(rx).map(|payload| {
        let event = match payload {
            SsePayload::Event(agent_event) => match agent_event {
                AgentEvent::TextDelta(delta) => Event::default()
                    .event("delta")
                    .data(serde_json::json!({"type": "delta", "delta": delta}).to_string()),
                AgentEvent::ToolCallStart { tool_name, .. } => Event::default()
                    .event("tool_start")
                    .data(
                        serde_json::json!({"type": "tool_start", "delta": tool_name}).to_string(),
                    ),
                AgentEvent::ToolCallEnd {
                    tool_name, success, error_message, ..
                } => {
                    let t = if success { "tool_end" } else { "tool_error" };
                    let mut payload = serde_json::json!({"type": t, "delta": tool_name});
                    if let Some(ref err) = error_message {
                        payload["error"] = serde_json::json!(err);
                    }
                    Event::default()
                        .event(t)
                        .data(payload.to_string())
                }
                AgentEvent::Reflecting => Event::default().event("thinking").data(
                    serde_json::json!({"type": "thinking", "delta": "reflecting on errors"})
                        .to_string(),
                ),
                AgentEvent::Compressing => Event::default().event("thinking").data(
                    serde_json::json!({"type": "thinking", "delta": "compressing memory"})
                        .to_string(),
                ),
                AgentEvent::Done => Event::default()
                    .event("done")
                    .data(serde_json::json!({"type": "done"}).to_string()),
            },
            SsePayload::Done(Ok(message)) => Event::default().event("done").data(
                serde_json::json!({"type": "done", "message": message}).to_string(),
            ),
            SsePayload::Done(Err(e)) => {
                tracing::error!(error = %e, "agent stream ended with error");
                Event::default().event("error").data(
                    serde_json::json!({"type": "error", "delta": format!("{e}")})
                        .to_string(),
                )
            }
        };
        Ok(event)
    });

    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    ))
}
