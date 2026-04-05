use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use openclaw_core::model::StreamEvent;
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
        let user_msg = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(incoming.content),
            created_at: Utc::now(),
        };
        conv.messages.push(user_msg);
        conv.updated_at = Utc::now();

        // Stream the response using chat_stream.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);
        let provider = state.provider.clone();
        let messages = conv.messages.clone();

        let stream_handle = tokio::spawn(async move {
            let callback = move |event: StreamEvent| {
                let _ = tx.try_send(event);
            };
            provider
                .chat_stream(&messages, &[], &callback)
                .await
        });

        // Forward stream events to the WebSocket.
        let mut final_message: Option<Message> = None;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => {
                    let out = WsOutgoing {
                        msg_type: "delta",
                        delta: Some(delta),
                        message: None,
                    };
                    if socket
                        .send(WsMessage::Text(serde_json::to_string(&out).unwrap().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                StreamEvent::Done(resp) => {
                    final_message = Some(resp.message);
                }
            }
        }

        // Wait for the provider task to complete.
        match stream_handle.await {
            Ok(Ok(resp)) => {
                if final_message.is_none() {
                    final_message = Some(resp.message);
                }
            }
            Ok(Err(e)) => {
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::json!({"type": "error", "delta": e.to_string()})
                            .to_string().into(),
                    ))
                    .await;
                continue;
            }
            Err(e) => {
                let _ = socket
                    .send(WsMessage::Text(
                        serde_json::json!({"type": "error", "delta": e.to_string()})
                            .to_string().into(),
                    ))
                    .await;
                continue;
            }
        }

        // Add assistant message and persist.
        if let Some(ref assistant_msg) = final_message {
            conv.messages.push(assistant_msg.clone());
            conv.updated_at = Utc::now();
            let _ = state.store.conversations().save(&conv);

            let out = WsOutgoing {
                msg_type: "done",
                delta: None,
                message: Some(assistant_msg.clone()),
            };
            let _ = socket
                .send(WsMessage::Text(serde_json::to_string(&out).unwrap().into()))
                .await;
        }
    }
}
