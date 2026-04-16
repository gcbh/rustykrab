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
    let t = state.rotate_token(); tracing::info!("token rotated"); eprintln!("\n  New RUSTYKRAB_AUTH_TOKEN={t}\n"); StatusCode::NO_CONTENT
}

async fn serve_media(State(state): State<AppState>, Path((project_id, filename)): Path<(String, String)>) -> Result<axum::response::Response, StatusCode> {
    use axum::body::Body; use axum::http::header;
    let video = state.video.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    if project_id.contains("..") || filename.contains("..") || filename.contains('/') { return Err(StatusCode::BAD_REQUEST); }
    let base = video.config().projects_dir.clone();
    let fp = base.join(&project_id).join(&filename);
    let c = fp.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    let cb = base.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !c.starts_with(&cb) { return Err(StatusCode::FORBIDDEN); }
    if !c.is_file() { return Err(StatusCode::NOT_FOUND); }
    let ct = match fp.extension().and_then(|e| e.to_str()).unwrap_or("") { "mp4"=>"video/mp4","webm"=>"video/webm","wav"=>"audio/wav","mp3"=>"audio/mpeg","png"=>"image/png","jpg"|"jpeg"=>"image/jpeg","html"=>"text/html",_=>"application/octet-stream" };
    let d = tokio::fs::read(&c).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    axum::http::Response::builder().header(header::CONTENT_TYPE,ct).header(header::CONTENT_LENGTH,d.len()).header(header::CONTENT_DISPOSITION,format!("inline; filename=\"{filename}\"")).body(Body::from(d)).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn create_conversation(State(state): State<AppState>) -> Result<Json<Conversation>, StatusCode> { state.store.conversations().create().map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR) }
async fn list_conversations(State(state): State<AppState>) -> Result<Json<Vec<Uuid>>, StatusCode> { state.store.conversations().list_ids().map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR) }
async fn get_conversation(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Conversation>, StatusCode> { state.store.conversations().get(id).map(Json).map_err(|_| StatusCode::NOT_FOUND) }
async fn delete_conversation(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, StatusCode> { state.store.conversations().delete(id).map(|_| StatusCode::NO_CONTENT).map_err(|e| match e { rustykrab_core::Error::NotFound(_) => StatusCode::NOT_FOUND, _ => StatusCode::INTERNAL_SERVER_ERROR }) }

async fn send_message(State(state): State<AppState>, Path(id): Path<Uuid>, Json(body): Json<SendMessageRequest>) -> Result<Json<Message>, StatusCode> {
    if body.content.len() > MAX_MESSAGE_SIZE { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let mut conv = state.store.conversations().get(id).map_err(|_| StatusCode::NOT_FOUND)?;
    let uc = body.content.clone();
    conv.messages.push(Message { id: Uuid::new_v4(), role: Role::User, content: MessageContent::Text(body.content), created_at: Utc::now() });
    conv.updated_at = Utc::now();
    let am = crate::orchestrate::run_agent(&state, &mut conv, &uc).await?;
    conv.updated_at = Utc::now();
    state.store.conversations().save(&conv).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(am))
}

enum SsePayload { Event(AgentEvent), Done(Result<Message, StatusCode>) }

async fn send_message_stream(State(state): State<AppState>, Path(id): Path<Uuid>, Json(body): Json<SendMessageRequest>) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if body.content.len() > MAX_MESSAGE_SIZE { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let mut conv = state.store.conversations().get(id).map_err(|_| StatusCode::NOT_FOUND)?;
    let uc = body.content.clone();
    conv.messages.push(Message { id: Uuid::new_v4(), role: Role::User, content: MessageContent::Text(body.content), created_at: Utc::now() });
    conv.updated_at = Utc::now();
    let (tx, rx) = tokio::sync::mpsc::channel::<SsePayload>(128);
    let as2 = state.clone(); let ptx = tx.clone();
    let ah = tokio::spawn(async move {
        let hb = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64));
        let h2 = hb.clone(); let etx = tx.clone();
        let on_event = move |ev: AgentEvent| { h2.store(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64, std::sync::atomic::Ordering::Relaxed); let _ = etx.try_send(SsePayload::Event(ev)); };
        let h3 = hb.clone();
        let mut mon = tokio::spawn(async move { loop { tokio::time::sleep(tokio::time::Duration::from_secs(30)).await; let l = h3.load(std::sync::atomic::Ordering::Relaxed); let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64; if n.saturating_sub(l) > 300_000 { break; } } });
        let af = crate::orchestrate::run_agent_streaming(&as2, &mut conv, &uc, &on_event);
        let r = tokio::select! { r = af => r, _ = &mut mon => { tracing::warn!("timeout"); Err(StatusCode::REQUEST_TIMEOUT) } };
        mon.abort(); conv.updated_at = Utc::now(); let _ = as2.store.conversations().save(&conv); let _ = tx.send(SsePayload::Done(r)).await;
    });
    tokio::spawn(async move { if let Err(e) = ah.await { tracing::error!("panic: {e}"); let _ = ptx.send(SsePayload::Done(Err(StatusCode::INTERNAL_SERVER_ERROR))).await; } });
    let stream = ReceiverStream::new(rx).map(|p| { Ok(match p {
        SsePayload::Event(ae) => match ae { AgentEvent::TextDelta(d)=>Event::default().event("delta").data(serde_json::json!({"type":"delta","delta":d}).to_string()), AgentEvent::ToolCallStart{tool_name,..}=>Event::default().event("tool_start").data(serde_json::json!({"type":"tool_start","delta":tool_name}).to_string()), AgentEvent::ToolCallEnd{tool_name,success,error_message,..}=>{let t=if success{"tool_end"}else{"tool_error"};let mut p=serde_json::json!({"type":t,"delta":tool_name});if let Some(ref e)=error_message{p["error"]=serde_json::json!(e);}Event::default().event(t).data(p.to_string())} AgentEvent::Reflecting=>Event::default().event("thinking").data(serde_json::json!({"type":"thinking","delta":"reflecting"}).to_string()), AgentEvent::Compressing=>Event::default().event("thinking").data(serde_json::json!({"type":"thinking","delta":"compressing"}).to_string()), AgentEvent::Done=>Event::default().event("done").data(serde_json::json!({"type":"done"}).to_string()) },
        SsePayload::Done(Ok(m))=>Event::default().event("done").data(serde_json::json!({"type":"done","message":m}).to_string()),
        SsePayload::Done(Err(e))=>{tracing::error!(error=%e,"error");Event::default().event("error").data(serde_json::json!({"type":"error","delta":format!("{e}")}).to_string())}
    }) });
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("ping")))
}
