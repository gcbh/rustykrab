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
        .route("/api/credential-requests", get(list_credential_requests))
        .route(
            "/api/credential-requests/{id}/approve",
            post(approve_credential_request),
        )
        .route(
            "/api/credential-requests/{id}/deny",
            post(deny_credential_request),
        )
        .route(
            "/api/credential-requests/{id}/fulfil",
            post(fulfil_credential_request),
        )
        .route("/api/tasks", post(submit_task).get(list_tasks))
        .route("/api/tasks/{id}", get(get_task).delete(cancel_task))
        .route("/api/pair", post(pair_device))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}", axum::routing::delete(revoke_device))
        .route("/api/devices/{id}/push-token", post(set_push_token))
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
        .agent
        .store
        .conversations()
        .create_with_title(title)
        .await
        .map(|c| Json(ApolloConversation::from(&c)))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_conversations(
    State(state): State<AppState>,
) -> Result<Json<Vec<ApolloConversation>>, StatusCode> {
    state
        .agent
        .store
        .conversations()
        .list_summaries()
        .await
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
        .agent
        .store
        .conversations()
        .get(id)
        .await
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
        .agent
        .store
        .conversations()
        .delete(id)
        .await
        .map_err(|e| match e {
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;
    // Everything the conversation owns in the database — messages, recall
    // archive, channel bindings — goes with it by cascade, so this handler
    // no longer has to know the list. What is left is process state, which
    // the database cannot reach: the recall cache and the todo list.
    state.agent.recall.purge(id);
    state.agent.todos.clear(id);
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
    let conv = state
        .agent
        .store
        .conversations()
        .get(id)
        .await
        .map_err(|e| match e {
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

    // Load the conversation. Capture the ids of the already-persisted
    // messages so the post-turn save can append just the new tail —
    // save_turn falls back to a full rewrite if the agent compacted
    // history (the persisted prefix no longer matches).
    let mut conv = state
        .agent
        .store
        .conversations()
        .get(id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let persisted_ids: Vec<Uuid> = conv.messages.iter().map(|m| m.id).collect();

    // Clone content before moving into the message (needed for profile classification).
    let user_content = body.content.clone();

    // Add the user message.
    let user_msg = Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(body.content),
        created_at: Utc::now(),
        agent_version: Message::version_stamp(),
    };
    conv.messages.push(user_msg);
    conv.updated_at = Utc::now();

    // Run the full agent pipeline.
    let assistant_msg = crate::run::run_agent(&state, &mut conv, &user_content, trace_id).await?;

    // Persist the turn (including intermediate tool call messages):
    // appends the new messages and bumps updated_at, or rewrites the
    // whole conversation if compaction replaced the persisted prefix.
    conv.updated_at = Utc::now();
    state
        .agent
        .store
        .conversations()
        .save_turn(&conv, &persisted_ids)
        .await
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

// ---------------------------------------------------------------------------
// Delegated tasks
//
// The asynchronous half of peer delegation: a peer submits work and gets
// an id back at once, then polls. The synchronous alternative — holding
// the HTTP request open for the whole agent turn — cannot work here,
// because a delegated task on a local model routinely runs for minutes
// and the caller's own tool-call timeout fires long before it returns.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SubmitTaskRequest {
    /// The instruction to run. Self-contained: this node does not share
    /// the caller's conversation.
    message: String,
    /// Continue an earlier delegated thread instead of opening a fresh
    /// conversation. Worth passing whenever the work is a follow-up: a
    /// continued thread reuses its evaluated prompt prefix, where a new
    /// one re-prefills the whole system prompt and tool schemas.
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    /// How many further delegation hops this task may make. Absent or
    /// zero means the run may not delegate onward at all.
    #[serde(default, rename = "hopBudget")]
    hop_budget: Option<i64>,
    /// Tools the caller wants this task limited to. Intersected with this
    /// node's own policy, never substituted for it — a peer can ask for
    /// less than the node allows and never more.
    #[serde(default, rename = "allowedTools")]
    allowed_tools: Option<Vec<String>>,
    /// The caller's trace id, so one delegation is greppable across both
    /// machines' logs.
    #[serde(default, rename = "traceId")]
    trace_id: Option<String>,
}

#[derive(Serialize)]
struct TaskResponse {
    id: String,
    status: String,
    #[serde(rename = "conversationId", skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(rename = "finishedAt", skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    /// Seconds the task has been alive, so a caller can report progress
    /// without tracking submission time itself.
    #[serde(rename = "elapsedSecs")]
    elapsed_secs: i64,
}

impl TaskResponse {
    fn from_task(task: rustykrab_store::DelegatedTask) -> Self {
        let until = task.finished_at.unwrap_or_else(Utc::now);
        Self {
            id: task.id,
            status: task.status.as_str().to_string(),
            conversation_id: task.conversation_id,
            result: task.result,
            error: task.error,
            created_at: task.created_at.to_rfc3339(),
            started_at: task.started_at.map(|t| t.to_rfc3339()),
            finished_at: task.finished_at.map(|t| t.to_rfc3339()),
            elapsed_secs: (until - task.created_at).num_seconds().max(0),
        }
    }
}

/// Accept a delegated task and return its handle. Runs nothing inline —
/// the worker picks it up.
async fn submit_task(
    State(state): State<AppState>,
    Extension(TraceId(trace_id)): Extension<TraceId>,
    principal: Option<Extension<rustykrab_store::Principal>>,
    Json(body): Json<SubmitTaskRequest>,
) -> Result<(StatusCode, Json<TaskResponse>), StatusCode> {
    if body.message.len() > MAX_MESSAGE_SIZE {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    if body.message.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Attribute the task to the peer that sent it. Without this a
    // delegated turn is indistinguishable from local user input in the
    // node's own logs.
    let who = principal.map(|Extension(p)| p.describe());

    // Fall back to this request's own trace id so the task is always
    // correlatable, even from a caller that sends none.
    let trace = body
        .trace_id
        .clone()
        .unwrap_or_else(|| trace_id.to_string());

    let task = state
        .agent
        .store
        .tasks()
        .enqueue(
            &body.message,
            body.conversation_id.as_deref(),
            who.as_deref(),
            body.hop_budget.unwrap_or(0),
            body.allowed_tools.clone(),
            Some(&trace),
        )
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "could not enqueue delegated task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tracing::info!(
        task_id = %task.id,
        principal = task.principal.as_deref().unwrap_or("unknown"),
        "accepted delegated task"
    );
    state.task_signal.wake();

    Ok((StatusCode::ACCEPTED, Json(TaskResponse::from_task(task))))
}

async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskResponse>, StatusCode> {
    match state.agent.store.tasks().get(&id).await {
        Ok(Some(task)) => Ok(Json(TaskResponse::from_task(task))),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, "could not read delegated task");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Recent tasks, newest first. Bounded so a long-lived node cannot
/// return an unbounded history.
async fn list_tasks(State(state): State<AppState>) -> Result<Json<Vec<TaskResponse>>, StatusCode> {
    match state.agent.store.tasks().list(50).await {
        Ok(tasks) => Ok(Json(
            tasks.into_iter().map(TaskResponse::from_task).collect(),
        )),
        Err(e) => {
            tracing::error!(error = %e, "could not list delegated tasks");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Cancel a task. Marks the row terminal, and additionally aborts the
/// agent loop when the task is already running — otherwise a cancel
/// would return promptly while the node kept burning inference on work
/// nobody will collect.
async fn cancel_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskResponse>, StatusCode> {
    let previous = state.agent.store.tasks().cancel(&id).await.map_err(|e| {
        tracing::error!(error = %e, "could not cancel delegated task");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let Some(previous) = previous else {
        return Err(StatusCode::NOT_FOUND);
    };
    if previous == rustykrab_store::TaskStatus::Running {
        let aborted = state.task_signal.abort(&id);
        tracing::info!(task_id = %id, aborted, "cancelled a running delegated task");
    }

    match state.agent.store.tasks().get(&id).await {
        Ok(Some(task)) => Ok(Json(TaskResponse::from_task(task))),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, "could not read cancelled task");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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

    // Load the conversation. `persisted_ids` lets the post-turn
    // save_turn append only the new messages (full rewrite if the agent
    // compacted history mid-run).
    let mut conv = state
        .agent
        .store
        .conversations()
        .get(id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let persisted_ids: Vec<Uuid> = conv.messages.iter().map(|m| m.id).collect();
    let conv_id = conv.id;

    let user_content = body.content.clone();

    // Add the user message.
    let user_msg = Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(body.content),
        created_at: Utc::now(),
        agent_version: Message::version_stamp(),
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

        let agent_future = crate::run::run_agent_streaming(
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

        // Persist the turn regardless of outcome to preserve the user
        // message. Appends the new tail; full rewrite when compaction
        // replaced the persisted prefix.
        conv.updated_at = Utc::now();
        if let Err(e) = agent_state
            .agent
            .store
            .conversations()
            .save_turn(&conv, &persisted_ids)
            .await
        {
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
    /// Replace an existing credential rather than refusing with `409`.
    ///
    /// Clients send this only after an explicit confirmation — that flag
    /// *is* the user's approval for a user-initiated overwrite.
    #[serde(default)]
    overwrite: bool,
}

/// Per-secret metadata. Values are never included.
#[derive(serde::Serialize)]
struct SecretEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
    version: i64,
}

#[derive(serde::Serialize)]
struct ListSecretsResponse {
    /// Names only, kept so existing clients keep working.
    names: Vec<String>,
    /// Name + timestamps + version.
    secrets: Vec<SecretEntry>,
    keychain_available: bool,
    /// Same value under the camelCase name the app's contract uses. Both
    /// are emitted so the WebChat UI and Apollo can each read the shape
    /// they expect without a flag day.
    #[serde(rename = "keychainAvailable")]
    keychain_available_camel: bool,
}

async fn list_secrets(
    State(state): State<AppState>,
) -> Result<Json<ListSecretsResponse>, StatusCode> {
    let meta = state.agent.store.secrets().metadata().await.map_err(|e| {
        tracing::error!(error = %e, "list_secrets failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let available = rustykrab_store::keychain::keychain_available();
    Ok(Json(ListSecretsResponse {
        names: meta.iter().map(|m| m.name.clone()).collect(),
        secrets: meta
            .into_iter()
            .map(|m| SecretEntry {
                name: m.name,
                created_at: m.created_at,
                updated_at: m.updated_at,
                version: m.version,
            })
            .collect(),
        keychain_available: available,
        keychain_available_camel: available,
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
            let secrets = state.agent.store.secrets();
            // Create-only by default. Replacing an existing credential is a
            // separate, explicit act — the client must have asked the user
            // first and sent `overwrite: true`.
            let result = if body.overwrite {
                secrets
                    .overwrite(
                        &body.name,
                        &body.value,
                        rustykrab_store::WriteAuthority::User { device: None },
                    )
                    .await
            } else {
                secrets.create(&body.name, &body.value).await
            };
            result.map_err(|e| match e {
                rustykrab_core::Error::AlreadyExists(_) => {
                    tracing::info!(name = %body.name, "set_secret: refused, name exists");
                    StatusCode::CONFLICT
                }
                rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
                other => {
                    tracing::warn!(error = %other, name = %body.name, "set_secret: store write failed");
                    StatusCode::BAD_REQUEST
                }
            })?;
            tracing::info!(
                name = %body.name,
                dest = "store",
                overwrite = body.overwrite,
                "secret stored"
            );
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
    state
        .agent
        .store
        .secrets()
        .delete(
            &name,
            rustykrab_store::WriteAuthority::User { device: None },
        )
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "delete_secret failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    tracing::info!(name = %name, "secret deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ── credential change requests ──────────────────────────────────────
//
// The agent files these; only an authenticated user resolves them. The
// agent runs in-process and holds no HTTP credentials, so it physically
// cannot reach these routes.

/// A pending change, as the app and WebChat see it. Never the value.
#[derive(serde::Serialize)]
struct ChangeRequestResponse {
    id: String,
    name: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(rename = "conversationId", skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    status: String,
    #[serde(rename = "createdAt")]
    created_at: i64,
    /// Present on `fulfil` requests: what the credential is for, and the
    /// inputs to render. Omitted entirely for update/delete, which are a
    /// yes/no decision and carry no form.
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fields: Vec<FieldResponse>,
}

#[derive(Serialize)]
struct FieldResponse {
    key: String,
    label: String,
    secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

async fn list_credential_requests(
    State(state): State<AppState>,
) -> Result<Json<Vec<ChangeRequestResponse>>, StatusCode> {
    let pending = state
        .agent
        .store
        .credential_requests()
        .pending()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "listing credential requests failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(
        pending
            .into_iter()
            .map(|r| ChangeRequestResponse {
                id: r.id,
                name: r.name,
                action: r.action.as_str().to_string(),
                reason: r.reason,
                conversation_id: r.conversation_id,
                status: r.status,
                created_at: r.created_at,
                service: r.service,
                fields: r
                    .fields
                    .into_iter()
                    .map(|f| FieldResponse {
                        key: f.key,
                        label: f.label,
                        secret: f.secret,
                        hint: f.hint,
                    })
                    .collect(),
            })
            .collect(),
    ))
}

async fn approve_credential_request(
    State(state): State<AppState>,
    principal: Option<Extension<rustykrab_store::Principal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    decide_credential_request(state, id, true, principal.map(|p| p.0)).await
}

async fn deny_credential_request(
    State(state): State<AppState>,
    principal: Option<Extension<rustykrab_store::Principal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    decide_credential_request(state, id, false, principal.map(|p| p.0)).await
}

/// The values a user typed into a fulfil request's form, keyed by the
/// field's `key`. Sent once, over TLS, and never echoed back by any
/// endpoint.
#[derive(Deserialize)]
struct FulfilRequest {
    values: std::collections::HashMap<String, String>,
}

async fn fulfil_credential_request(
    State(state): State<AppState>,
    principal: Option<Extension<rustykrab_store::Principal>>,
    Path(id): Path<String>,
    Json(body): Json<FulfilRequest>,
) -> Result<StatusCode, StatusCode> {
    let decided_by = principal
        .map(|p| p.0.describe())
        .unwrap_or_else(|| "webchat".to_string());
    let values: Vec<(String, String)> = body.values.into_iter().collect();
    state
        .agent
        .store
        .credential_requests()
        .fulfil(&id, &values, &decided_by)
        .await
        .map_err(|e| match e {
            rustykrab_core::Error::AlreadyExists(reason) => {
                tracing::info!(%id, %reason, "fulfil refused as stale");
                StatusCode::CONFLICT
            }
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            // A field the request never asked for, or a blank answer. The
            // reason is deliberately not echoed: it is about credential
            // names, and this is an unauthenticated-shaped error path.
            other => {
                tracing::warn!(error = %other, %id, "fulfil rejected");
                StatusCode::BAD_REQUEST
            }
        })?;
    // Deliberately no value, no name, no count — a successful fulfil says
    // only that it happened.
    tracing::info!(%id, "credential request fulfilled");
    Ok(StatusCode::NO_CONTENT)
}

// ── device pairing ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    #[serde(rename = "deviceName")]
    device_name: String,
}

#[derive(Serialize)]
struct PairResponse {
    #[serde(rename = "deviceId")]
    device_id: String,
    /// Shown exactly once. Stored server-side only as a hash.
    #[serde(rename = "deviceToken")]
    device_token: String,
}

/// Exchange a one-time code for a device token.
///
/// The only unauthenticated `/api` route besides health: a device has no
/// token yet, which is the point. Protection is the code itself (single
/// use, five-minute TTL, hashed at rest) plus the rate limiter in front.
async fn pair_device(
    State(state): State<AppState>,
    Json(body): Json<PairRequest>,
) -> Result<Json<PairResponse>, StatusCode> {
    let (device, token) = state
        .agent
        .store
        .devices()
        .redeem_pairing_code(&body.code, &body.device_name)
        .await
        .map_err(|e| {
            // Deliberately uniform: a caller learns "that didn't work",
            // not whether the code was wrong, used, or merely expired.
            tracing::warn!(error = %e, "pairing attempt refused");
            StatusCode::FORBIDDEN
        })?;
    tracing::info!(device = %device.name, id = %device.id, "device paired");
    Ok(Json(PairResponse {
        device_id: device.id,
        device_token: token,
    }))
}

#[derive(Serialize)]
struct DeviceResponse {
    id: String,
    name: String,
    #[serde(rename = "createdAt")]
    created_at: i64,
    #[serde(rename = "lastSeenAt", skip_serializing_if = "Option::is_none")]
    last_seen_at: Option<i64>,
}

async fn list_devices(
    State(state): State<AppState>,
) -> Result<Json<Vec<DeviceResponse>>, StatusCode> {
    let devices = state.agent.store.devices().list().await.map_err(|e| {
        tracing::error!(error = %e, "listing devices failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(
        devices
            .into_iter()
            .map(|d| DeviceResponse {
                id: d.id,
                name: d.name,
                created_at: d.created_at,
                last_seen_at: d.last_seen_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PushTokenRequest {
    token: String,
}

/// Register the APNs token a device wants approval prompts on.
///
/// The app calls this after iOS hands it a token, and again whenever iOS
/// reissues one.
async fn set_push_token(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PushTokenRequest>,
) -> Result<StatusCode, StatusCode> {
    let token = body.token.trim();
    // An APNs token is hex; anything else is a client bug worth rejecting
    // rather than storing and failing on later.
    if token.is_empty() || token.len() > 200 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    state
        .agent
        .store
        .devices()
        .set_push_token(&id, token)
        .await
        .map_err(|e| match e {
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            other => {
                tracing::error!(error = %other, %id, "storing push token failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;
    tracing::info!(%id, "push token registered");
    Ok(StatusCode::NO_CONTENT)
}

/// Revoke a device — the lost-phone story. Its token stops working
/// immediately, without disturbing any other device.
async fn revoke_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state
        .agent
        .store
        .devices()
        .revoke(&id)
        .await
        .map_err(|e| match e {
            rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
            other => {
                tracing::error!(error = %other, %id, "revoking device failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;
    tracing::info!(%id, "device revoked");
    Ok(StatusCode::NO_CONTENT)
}

async fn decide_credential_request(
    state: AppState,
    id: String,
    approve: bool,
    principal: Option<rustykrab_store::Principal>,
) -> Result<StatusCode, StatusCode> {
    let requests = state.agent.store.credential_requests();
    // Which device decided, for the audit trail.
    let decided_by = principal
        .map(|p| p.describe())
        .unwrap_or_else(|| "webchat".to_string());
    let decided_by = decided_by.as_str();
    let result = if approve {
        requests.approve(&id, decided_by).await
    } else {
        requests.deny(&id, decided_by).await
    };
    result.map_err(|e| match e {
        // Already decided, or the credential moved since the request was
        // filed — approving now would undo whatever the user did instead.
        rustykrab_core::Error::AlreadyExists(reason) => {
            tracing::info!(%id, %reason, "credential request decision refused as stale");
            StatusCode::CONFLICT
        }
        rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND,
        other => {
            tracing::error!(error = %other, %id, "credential request decision failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;
    tracing::info!(%id, approved = approve, "credential request decided");
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
            agent_version: None,
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
            agent_version: None,
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
