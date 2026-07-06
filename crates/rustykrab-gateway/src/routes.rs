use std::convert::Infallible;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use rustykrab_agent::AgentEvent;
use rustykrab_core::types::{
    ContentBlock, Conversation, Message, MessageContent, Role, ToolCall, ToolResult,
};
use rustykrab_store::ConversationSummary;

use crate::logging::TraceId;
use crate::AppState;

/// Maximum allowed message size in bytes (100 KB).
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
        .route(
            "/api/conversations/{id}/messages",
            get(list_messages).post(send_message),
        )
        .route(
            "/api/conversations/{id}/messages/stream",
            post(send_message_stream),
        )
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets", post(set_secret))
        .route("/api/secrets/{name}", axum::routing::delete(delete_secret))
        .route("/api/health", get(health))
        .route("/api/logout", post(logout))
}

// ---------------------------------------------------------------------------
// Apollo integration DTOs
// ---------------------------------------------------------------------------
//
// These shapes match the Apollo BFF contract documented in
// `docs/integrations/apollo.md`. The internal `Conversation` /
// `Message` types embed multi-modal content, tool calls and tool
// results that Apollo doesn't model — the DTOs project those down to
// the simple `{id, title, createdAt, updatedAt}` and
// `{id, conversationId, role, content, createdAt}` shapes Apollo
// expects, emitting timestamps as epoch milliseconds.
//
// Apollo's client accepts both camelCase and snake_case on the wire;
// we emit camelCase since that is what the contract documents.

/// Conversation summary returned by `/api/conversations` and friends.
#[derive(Debug, Serialize)]
struct ApolloConversation {
    id: String,
    title: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: i64,
    #[serde(rename = "updatedAt")]
    updated_at: i64,
}

impl From<&Conversation> for ApolloConversation {
    fn from(conv: &Conversation) -> Self {
        Self {
            id: conv.id.to_string(),
            title: conv.title.clone(),
            created_at: epoch_millis(conv.created_at),
            updated_at: epoch_millis(conv.updated_at),
        }
    }
}

impl From<&ConversationSummary> for ApolloConversation {
    fn from(s: &ConversationSummary) -> Self {
        Self {
            id: s.id.to_string(),
            title: s.title.clone(),
            created_at: epoch_millis(s.created_at),
            updated_at: epoch_millis(s.updated_at),
        }
    }
}

/// Message shape exposed to Apollo. Tool calls / multi-part content
/// collapse to a textual rendering — Apollo treats messages as plain
/// strings.
#[derive(Debug, Serialize)]
struct ApolloMessage {
    id: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    role: ApolloRole,
    content: String,
    #[serde(rename = "createdAt")]
    created_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ApolloRole {
    User,
    Assistant,
    System,
}

impl ApolloMessage {
    fn from_message(conv_id: Uuid, msg: &Message) -> Self {
        Self {
            id: msg.id.to_string(),
            conversation_id: conv_id.to_string(),
            role: apollo_role(msg.role),
            content: render_message_content(&msg.content),
            created_at: epoch_millis(msg.created_at),
        }
    }
}

fn apollo_role(role: Role) -> ApolloRole {
    match role {
        Role::User => ApolloRole::User,
        Role::Assistant => ApolloRole::Assistant,
        // `Tool` role is internal to RustyKrab; coerce to `assistant`
        // so Apollo doesn't see an unknown value. Apollo's own client
        // applies the same coercion defensively.
        Role::System => ApolloRole::System,
        Role::Tool => ApolloRole::Assistant,
    }
}

/// Render any `MessageContent` to a plain string for Apollo. For text
/// content this is the raw text; tool calls and tool results render
/// to a compact, human-readable form so they don't surface as empty
/// turns in the chat UI.
fn render_message_content(c: &MessageContent) -> String {
    match c {
        MessageContent::Text(s) => s.clone(),
        MessageContent::ToolCall(tc) => format_tool_call(tc),
        MessageContent::MultiToolCall(tcs) => tcs
            .iter()
            .map(format_tool_call)
            .collect::<Vec<_>>()
            .join("\n"),
        MessageContent::ToolResult(tr) => format_tool_result(tr),
        MessageContent::MultiPart(blocks) => blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::Image { media_type, .. } => format!("[image:{media_type}]"),
                ContentBlock::ToolUse { name, .. } => format!("[tool_use:{name}]"),
                ContentBlock::ToolResult { content, .. } => content.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn format_tool_call(tc: &ToolCall) -> String {
    format!("[tool_call:{}({})]", tc.name, tc.arguments)
}

fn format_tool_result(tr: &ToolResult) -> String {
    let prefix = if tr.is_error {
        "[tool_error]"
    } else {
        "[tool_result]"
    };
    format!("{prefix} {}", tr.output)
}

fn epoch_millis(ts: DateTime<Utc>) -> i64 {
    ts.timestamp_millis()
}

#[derive(Default, Deserialize)]
struct CreateConversationRequest {
    #[serde(default)]
    title: Option<String>,
}

/// Body of `POST /api/conversations/{id}/messages` and the stream
/// variant. Apollo always sends `{ "content": "..." }`.
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
    // Print to stderr to avoid capture by structured logging infrastructure.
    eprintln!("\n  New RUSTYKRAB_AUTH_TOKEN={new_token}\n");
    StatusCode::NO_CONTENT
}

async fn create_conversation(
    State(state): State<AppState>,
    body: Option<Json<CreateConversationRequest>>,
) -> Result<Json<ApolloConversation>, StatusCode> {
    let title = body.and_then(|Json(b)| b.title).filter(|s| !s.is_empty());
    state
        .store
        .conversations()
        .create_with_title(title)
        .map(|c| Json(ApolloConversation::from(&c)))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_conversations(
    State(state): State<AppState>,
) -> Result<Json<Vec<ApolloConversation>>, StatusCode> {
    state
        .store
        .conversations()
        .list_summaries()
        .map(|summaries| {
            Json(
                summaries
                    .iter()
                    .map(ApolloConversation::from)
                    .collect::<Vec<_>>(),
            )
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn get_conversation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ApolloConversation>, StatusCode> {
    state
        .store
        .conversations()
        .get(id)
        .map(|c| Json(ApolloConversation::from(&c)))
        .map_err(|e| match e {
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })
}

async fn delete_conversation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    state
        .store
        .conversations()
        .delete(id)
        .map_err(|e| match e {
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;
    // Drop the recall archive too (cache + durable row) so a deleted
    // conversation leaves nothing behind.
    state.recall.purge(id);
    // Drop the conversation's todo list as well.
    state.todos.clear(id);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/conversations/{id}/messages`.
///
/// Returns every persisted message in the conversation projected to
/// the Apollo wire shape. System messages and tool/result turns are
/// included so transcript replays match what the model saw, but
/// downstream clients (Apollo) typically filter to user/assistant
/// before rendering.
async fn list_messages(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ApolloMessage>>, StatusCode> {
    let conv = state.store.conversations().get(id).map_err(|e| match e {
        rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    })?;
    let msgs: Vec<ApolloMessage> = conv
        .messages
        .iter()
        .map(|m| ApolloMessage::from_message(conv.id, m))
        .collect();
    Ok(Json(msgs))
}

/// Response body for `POST /api/conversations/{id}/messages`.
///
/// Apollo accepts either a bare `Message` or the envelope form
/// `{ message, apps }`. We use the envelope whenever the agent
/// produced one or more app specs during the turn (today the runner
/// never does, so this is always the bare-message form — but the
/// shape is here so a future tool emitting apps can flip the switch).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SendMessageResponse {
    Bare(ApolloMessage),
    Envelope {
        message: ApolloMessage,
        apps: Vec<Value>,
    },
}

/// Send a user message to a conversation and get an assistant response.
async fn send_message(
    State(state): State<AppState>,
    Extension(TraceId(trace_id)): Extension<TraceId>,
    Path(id): Path<Uuid>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, StatusCode> {
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
        crate::orchestrate::run_agent(&state, &mut conv, &user_content, trace_id).await?;

    // Persist the full conversation (including intermediate tool call messages).
    conv.updated_at = Utc::now();
    state
        .store
        .conversations()
        .save(&conv)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let apollo_msg = ApolloMessage::from_message(conv.id, &assistant_msg);
    let apps = extract_apps_from_text(&assistant_msg);
    let response = if apps.is_empty() {
        SendMessageResponse::Bare(apollo_msg)
    } else {
        SendMessageResponse::Envelope {
            message: apollo_msg,
            apps,
        }
    };
    Ok(Json(response))
}

/// Look for embedded app specs in an assistant message. Today the
/// agent doesn't produce them, so this always returns an empty vector
/// and Apollo gets the bare-`Message` form. The hook is here so a
/// future `app_render` tool can stash specs on the message and have
/// them surface in the envelope form without further routing changes.
fn extract_apps_from_text(_msg: &Message) -> Vec<Value> {
    Vec::new()
}

/// Payload sent through the MPSC channel from the agent task to the SSE stream.
enum SsePayload {
    Event(AgentEvent),
    Done(Result<Message, StatusCode>),
}

/// Wire shape for the high-frequency `text` SSE event. Serialized
/// directly with `serde_json::to_string` (no intermediate `Value` tree
/// per token). Field order matches the previous `json!` output
/// (alphabetical) so the wire format is byte-identical.
#[derive(Serialize)]
struct TextDeltaPayload<'a> {
    delta: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Send a user message and stream the assistant response as SSE events.
async fn send_message_stream(
    State(state): State<AppState>,
    Extension(TraceId(trace_id)): Extension<TraceId>,
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
    let conv_id = conv.id;

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

    // Spawn the agent loop in a background task with a heartbeat-based timeout.
    // The agent can run indefinitely as long as it emits events (tool calls,
    // text deltas, etc.) within each 5-minute window. This prevents the 408
    // timeout that killed long-running orchestration tasks while still
    // catching genuinely stuck agents.
    // Wrap agent task in a panic-logging outer task so panics in the
    // streaming agent are surfaced instead of silently swallowed when
    // the JoinHandle is dropped (fixes ASYNC-H4).
    let agent_state = state.clone();
    let panic_tx = tx.clone();
    let agent_handle = tokio::spawn(async move {
        // Heartbeat bookkeeping uses a monotonic Instant origin; the atomic
        // stores milliseconds elapsed since `start` (cheaper and steadier
        // than a SystemTime/UNIX_EPOCH read per streamed event).
        let start = std::time::Instant::now();
        let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        let hb = heartbeat.clone();
        let event_tx = tx.clone();
        let on_event = move |event: AgentEvent| {
            // Reset heartbeat on every event.
            hb.store(
                start.elapsed().as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            if let Err(e) = event_tx.try_send(SsePayload::Event(event)) {
                tracing::warn!("SSE event dropped (channel full): {e}");
            }
        };

        // Heartbeat monitor: checks every 30s if we've gone 5 minutes without an event.
        let hb_monitor = heartbeat.clone();
        let timeout_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tf = timeout_flag.clone();
        let mut monitor = tokio::spawn(async move {
            const HEARTBEAT_TIMEOUT_MS: u64 = 300_000; // 5 minutes
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                let last = hb_monitor.load(std::sync::atomic::Ordering::Relaxed);
                let now = start.elapsed().as_millis() as u64;
                if now.saturating_sub(last) > HEARTBEAT_TIMEOUT_MS {
                    tf.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        });

        let agent_future = crate::orchestrate::run_agent_streaming(
            &agent_state,
            &mut conv,
            &user_content,
            &on_event,
            trace_id,
        );

        let result = tokio::select! {
            r = agent_future => r,
            _ = &mut monitor => {
                tracing::warn!("streaming agent timed out (no activity for 5 minutes)");
                Err(StatusCode::REQUEST_TIMEOUT)
            }
        };

        // Abort the monitor task to prevent it from leaking for up to
        // 5 minutes after the agent completes normally.
        monitor.abort();

        // Persist conversation regardless of outcome to preserve the user message.
        conv.updated_at = Utc::now();
        if let Err(e) = agent_state.store.conversations().save(&conv) {
            tracing::error!("failed to save conversation: {e}");
        }

        let _ = tx.send(SsePayload::Done(result)).await;
    });
    // Spawn a lightweight watcher that logs if the agent task panics
    // and sends an error event to the client so the result is not silently lost.
    tokio::spawn(async move {
        if let Err(e) = agent_handle.await {
            tracing::error!("streaming agent task panicked: {e}");
            let _ = panic_tx
                .send(SsePayload::Done(Err(StatusCode::INTERNAL_SERVER_ERROR)))
                .await;
        }
    });

    // Map channel messages to SSE events.
    //
    // The Apollo contract recognises three event types — `text`,
    // `apps`, `done` — and ignores anything else. Internal tool
    // events still flow through so the WebChat UI (which understands
    // `tool_start`, `tool_end`, `thinking`, etc.) keeps working; the
    // Apollo client treats those frames as no-ops.
    let stream = ReceiverStream::new(rx).map(move |payload| {
        let event = match payload {
            SsePayload::Event(agent_event) => match agent_event {
                AgentEvent::TextDelta(delta) => Event::default().event("text").data(
                    serde_json::to_string(&TextDeltaPayload {
                        delta: &delta,
                        kind: "text",
                    })
                    .unwrap_or_default(),
                ),
                AgentEvent::ToolCallStart { tool_name, .. } => {
                    Event::default().event("tool_start").data(
                        serde_json::json!({"type": "tool_start", "delta": tool_name}).to_string(),
                    )
                }
                AgentEvent::ToolHeartbeat {
                    tool_name,
                    elapsed_secs,
                    ..
                } => Event::default().event("tool_heartbeat").data(
                    serde_json::json!({
                        "type": "tool_heartbeat",
                        "delta": tool_name,
                        "elapsed_secs": elapsed_secs,
                    })
                    .to_string(),
                ),
                AgentEvent::ToolCallEnd {
                    tool_name,
                    success,
                    error_message,
                    ..
                } => {
                    let t = if success { "tool_end" } else { "tool_error" };
                    let mut payload = serde_json::json!({"type": t, "delta": tool_name});
                    if let Some(ref err) = error_message {
                        payload["error"] = serde_json::json!(err);
                    }
                    Event::default().event(t).data(payload.to_string())
                }
                AgentEvent::Reflecting => Event::default().event("thinking").data(
                    serde_json::json!({"type": "thinking", "delta": "reflecting on errors"})
                        .to_string(),
                ),
                AgentEvent::Compressing => Event::default().event("thinking").data(
                    serde_json::json!({"type": "thinking", "delta": "compressing memory"})
                        .to_string(),
                ),
                AgentEvent::UserMessageQueued { message_id } => {
                    Event::default().event("user_message_queued").data(
                        serde_json::json!({
                            "type": "user_message_queued",
                            "message_id": message_id.to_string()
                        })
                        .to_string(),
                    )
                }
                AgentEvent::Done => Event::default()
                    .event("done")
                    .data(serde_json::json!({"type": "done"}).to_string()),
            },
            SsePayload::Done(Ok(message)) => {
                let apollo_msg = ApolloMessage::from_message(conv_id, &message);
                let apps = extract_apps_from_text(&message);
                // Emit a single Apollo-shaped terminal `done` event.
                // The optional `apps` field is omitted when empty so
                // the wire stays close to the documented shape.
                let mut payload = serde_json::json!({
                    "type": "done",
                    "message": apollo_msg,
                });
                if !apps.is_empty() {
                    payload["apps"] = serde_json::Value::Array(apps);
                }
                Event::default().event("done").data(payload.to_string())
            }
            SsePayload::Done(Err(e)) => {
                tracing::error!(error = %e, "agent stream ended with error");
                // The Apollo contract says the cleanest behaviour on
                // mid-stream failure is to close the response, and that
                // Apollo will synthesise an "agent unavailable" frame
                // itself. We still emit an explicit `error` event so
                // clients (WebChat, debug consumers) get a clear signal
                // before the stream ends.
                Event::default().event("error").data(
                    serde_json::json!({
                        "type": "error",
                        "message": "The agent is unavailable.",
                        "delta": format!("{e}"),
                    })
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

// ---------------------------------------------------------------------------
// Secrets management
// ---------------------------------------------------------------------------
//
// These endpoints let a trusted caller (the local `rustykrab chat` CLI, or
// future settings UI) write credentials directly into the encrypted store
// or the OS keychain without the value passing through the model.
//
// All `/api/*` endpoints are already gated by the bearer-token middleware,
// so callers must hold `RUSTYKRAB_AUTH_TOKEN`.

const MAX_SECRET_VALUE_SIZE: usize = 64 * 1024;

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SecretDest {
    #[default]
    Store,
    Keychain,
}

#[derive(Deserialize)]
struct SetSecretRequest {
    /// Identifier in the encrypted store. For MCP credentials, by
    /// convention `mcp.<server>.<field>`.
    name: String,
    value: String,
    #[serde(default)]
    dest: SecretDest,
    /// macOS Keychain service name (required when `dest == "keychain"`).
    #[serde(default)]
    service: Option<String>,
    /// macOS Keychain account name (required when `dest == "keychain"`).
    #[serde(default)]
    account: Option<String>,
}

#[derive(serde::Serialize)]
struct ListSecretsResponse {
    names: Vec<String>,
    keychain_available: bool,
}

async fn list_secrets(
    State(state): State<AppState>,
) -> Result<Json<ListSecretsResponse>, StatusCode> {
    let names = state.store.secrets().list_names().map_err(|e| {
        tracing::error!(error = %e, "list_secrets failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(ListSecretsResponse {
        names,
        keychain_available: rustykrab_store::keychain::keychain_available(),
    }))
}

async fn set_secret(
    State(state): State<AppState>,
    Json(body): Json<SetSecretRequest>,
) -> Result<StatusCode, StatusCode> {
    if body.value.len() > MAX_SECRET_VALUE_SIZE {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    if body.value.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    match body.dest {
        SecretDest::Store => {
            state
                .store
                .secrets()
                .set(&body.name, &body.value)
                .map_err(|e| {
                    tracing::warn!(error = %e, name = %body.name, "set_secret: store write failed");
                    StatusCode::BAD_REQUEST
                })?;
            tracing::info!(name = %body.name, dest = "store", "secret stored");
        }
        SecretDest::Keychain => {
            if !rustykrab_store::keychain::keychain_available() {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            let service = body.service.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
            let account = body.account.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
            rustykrab_store::keychain::set_credential(service, account, &body.value).map_err(
                |e| {
                    tracing::error!(error = %e, "set_secret: keychain write failed");
                    StatusCode::INTERNAL_SERVER_ERROR
                },
            )?;
            tracing::info!(
                service = %service,
                account = %account,
                dest = "keychain",
                "secret stored"
            );
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state.store.secrets().delete(&name).map_err(|e| {
        tracing::error!(error = %e, "delete_secret failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    tracing::info!(name = %name, "secret deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn text_delta_payload_matches_previous_json_wire_format() {
        let direct = serde_json::to_string(&TextDeltaPayload {
            delta: "hi \"there\"",
            kind: "text",
        })
        .unwrap();
        let via_value = serde_json::json!({"type": "text", "delta": "hi \"there\""}).to_string();
        assert_eq!(direct, via_value);
    }

    #[test]
    fn apollo_conversation_serializes_camel_case_epoch_millis() {
        let conv = Conversation {
            id: Uuid::nil(),
            messages: Vec::new(),
            created_at: ts("2024-01-01T00:00:00Z"),
            updated_at: ts("2024-01-02T00:00:00Z"),
            title: Some("hello".into()),
            summary: None,
            detected_profile: None,
            channel_source: None,
            channel_id: None,
            channel_thread_id: None,
        };
        let json = serde_json::to_value(ApolloConversation::from(&conv)).unwrap();
        assert_eq!(json["id"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(json["title"], "hello");
        assert_eq!(json["createdAt"], 1_704_067_200_000_i64);
        assert_eq!(json["updatedAt"], 1_704_153_600_000_i64);
        // No internal-only fields leak.
        assert!(json.get("messages").is_none());
        assert!(json.get("channel_source").is_none());
    }

    #[test]
    fn apollo_message_collapses_to_string_content() {
        let conv_id = Uuid::nil();
        let plain = Message {
            id: Uuid::nil(),
            role: Role::Assistant,
            content: MessageContent::Text("hi there".into()),
            created_at: ts("2024-01-01T00:00:00Z"),
        };
        let json = serde_json::to_value(ApolloMessage::from_message(conv_id, &plain)).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "hi there");
        assert_eq!(json["conversationId"], conv_id.to_string());
        assert_eq!(json["createdAt"], 1_704_067_200_000_i64);
    }

    #[test]
    fn apollo_message_renders_tool_call_and_result() {
        let call = Message {
            id: Uuid::nil(),
            role: Role::Assistant,
            content: MessageContent::ToolCall(ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"msg": "hi"}),
            }),
            created_at: ts("2024-01-01T00:00:00Z"),
        };
        let rendered = render_message_content(&call.content);
        assert!(rendered.starts_with("[tool_call:echo("));
        assert!(rendered.contains("msg"));

        let result = MessageContent::ToolResult(ToolResult {
            call_id: "1".into(),
            output: serde_json::json!("ok"),
            is_error: false,
            images: Vec::new(),
        });
        assert!(render_message_content(&result).starts_with("[tool_result]"));

        let err = MessageContent::ToolResult(ToolResult {
            call_id: "1".into(),
            output: serde_json::json!("boom"),
            is_error: true,
            images: Vec::new(),
        });
        assert!(render_message_content(&err).starts_with("[tool_error]"));
    }

    #[test]
    fn apollo_role_coerces_tool_to_assistant() {
        assert!(matches!(apollo_role(Role::Tool), ApolloRole::Assistant));
        assert!(matches!(apollo_role(Role::System), ApolloRole::System));
        assert!(matches!(apollo_role(Role::User), ApolloRole::User));
        assert!(matches!(
            apollo_role(Role::Assistant),
            ApolloRole::Assistant
        ));
    }

    #[test]
    fn send_message_response_serializes_bare_or_envelope() {
        let conv_id = Uuid::nil();
        let msg = ApolloMessage {
            id: Uuid::nil().to_string(),
            conversation_id: conv_id.to_string(),
            role: ApolloRole::Assistant,
            content: "ok".into(),
            created_at: 0,
        };
        let bare = serde_json::to_value(SendMessageResponse::Bare(msg)).unwrap();
        assert_eq!(bare["content"], "ok");
        assert!(bare.get("message").is_none());

        let env = SendMessageResponse::Envelope {
            message: ApolloMessage {
                id: Uuid::nil().to_string(),
                conversation_id: conv_id.to_string(),
                role: ApolloRole::Assistant,
                content: "ok".into(),
                created_at: 0,
            },
            apps: vec![serde_json::json!({"name": "x", "html": "<p/>"})],
        };
        let env = serde_json::to_value(env).unwrap();
        assert_eq!(env["message"]["content"], "ok");
        assert_eq!(env["apps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn create_conversation_request_accepts_missing_body() {
        let req: CreateConversationRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.title, None);
        let req: CreateConversationRequest =
            serde_json::from_str(r#"{"title":"my chat"}"#).unwrap();
        assert_eq!(req.title.as_deref(), Some("my chat"));
    }
}
