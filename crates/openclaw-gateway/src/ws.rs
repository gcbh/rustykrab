use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use openclaw_agent::AgentEvent;
use openclaw_core::types::{Message, MessageContent, Role};

use crate::AppState;

pub fn ws_routes() -> Router<AppState> {
    Router::new().route("/ws/chat", get(ws_handler))
}

async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

#[derive(Deserialize)]
struct WsIncoming {
    /// Conversation ID (create one first via POST /api/conversations).
    conversation_id: Uuid,
    content: String,
}

#[derive(Serialize)]
struct WsOutgoing {
    #[serde(rename = "type")]
    msg_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Message>,
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            WsMessage::Text(t) => t,
            WsMessage::Close(_) => break,
            _ => continue,
        };

        let incoming: WsIncoming = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::json!({"type": "error", "delta": e.to_string()}).to_string().into(),
                    ))
                    .await;
                continue;
            }
        };

        // Load conversation.
        let mut conv = match state.store.conversations().get(incoming.conversation_id) {
            Ok(c) => c,
            Err(_) => {
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::json!({"type": "error", "delta": "conversation not found"})
                            .to_string().into(),
                    ))
                    .await;
                continue;
            }
        };

        // Add user message.
        let user_content = incoming.content.clone();
        let user_msg = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(incoming.content),
            created_at: Utc::now(),
        };
        conv.messages.push(user_msg);
        conv.updated_at = Utc::now();

        // Stream agent events to the WebSocket via a channel.
        // The agent loop runs in a spawned task so we can forward
        // events to the socket concurrently as they arrive.
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<AgentEvent>(128);
        let agent_state = state.clone();
        let agent_content = user_content.clone();

        let agent_handle = tokio::spawn(async move {
            let on_event = move |event: AgentEvent| {
                let _ = event_tx.try_send(event);
            };
            crate::orchestrate::run_agent_streaming(
                &agent_state,
                &mut conv,
                &agent_content,
                &on_event,
            )
            .await
            .map(|msg| (msg, conv))
        });

        // Forward events to the WebSocket as they arrive.
        while let Some(event) = event_rx.recv().await {
            let ws_msg = match event {
                AgentEvent::TextDelta(delta) => WsOutgoing {
                    msg_type: "delta",
                    delta: Some(delta),
                    message: None,
                },
                AgentEvent::ToolCallStart { tool_name, .. } => WsOutgoing {
                    msg_type: "tool_start",
                    delta: Some(tool_name),
                    message: None,
                },
                AgentEvent::ToolCallEnd { tool_name, success, .. } => WsOutgoing {
                    msg_type: if success { "tool_end" } else { "tool_error" },
                    delta: Some(tool_name),
                    message: None,
                },
                AgentEvent::Reflecting => WsOutgoing {
                    msg_type: "thinking",
                    delta: Some("reflecting on errors".to_string()),
                    message: None,
                },
                AgentEvent::Compressing => WsOutgoing {
                    msg_type: "thinking",
                    delta: Some("compressing memory".to_string()),
                    message: None,
                },
                AgentEvent::Done => continue, // handled below
            };
            if socket
                .send(WsMessage::Text(
                    serde_json::to_string(&ws_msg).unwrap().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }

        // Collect the final result.
        match agent_handle.await {
            Ok(Ok((assistant_msg, finished_conv))) => {
                // Persist the full conversation from the agent task.
                let _ = state.store.conversations().save(&finished_conv);

                let done_out = WsOutgoing {
                    msg_type: "done",
                    delta: None,
                    message: Some(assistant_msg),
                };
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::to_string(&done_out).unwrap().into(),
                    ))
                    .await;
            }
            Ok(Err(_)) => {
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::json!({"type": "error", "delta": "agent pipeline error"})
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
            Err(e) => {
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::json!({"type": "error", "delta": e.to_string()})
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    }
}
