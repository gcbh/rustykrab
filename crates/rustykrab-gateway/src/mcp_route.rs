//! `POST /api/mcp`: RustyKrab's host tools for a native agent, over MCP
//! (`docs/plans/native-mcp-host-tools.md`).
//!
//! `rustykrab-cli mcp-bridge` runs beside a native agent (Claude Code or
//! Codex), speaks MCP to it over stdio, and posts each JSON-RPC message
//! here as it is, with the run's grant as its bearer token. This route
//! answers `initialize`, `ping`, `tools/list` and `tools/call` from the
//! grant ([`McpGrants`], which runs every call in the daemon), and any
//! other request with "method not found", which is also how Claude Code's
//! `server/discover` probe learns to fall back to `initialize`. A
//! notification is accepted with 202 and no body.
//!
//! The route has its own auth path, and only its own:
//!
//! - The caller is a process on this machine: the connection's own peer is
//!   loopback, and the request carries no forwarding header. A local proxy
//!   such as `tailscale serve` connects from loopback too, so a request it
//!   forwarded is refused rather than trusted.
//! - The bearer is a live grant. `auth::require_auth` passes this path
//!   through untouched, so the owner's token and device tokens are not
//!   accepted here, and a grant authenticates no other route.
//! - The rate limiter does not count a request bearing a live grant; the
//!   grant's quota and its one call in flight bound it.
//!
//! A token that is not a live grant's (never minted, revoked, expired) is
//! 401 whatever the message. What a live grant refuses (a tool outside it,
//! its quota spent, a second call in flight, an id reused for another call)
//! is a JSON-RPC error, and a tool's own failure is a result with
//! `isError`.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rustykrab_agent::mcp_grants::{CallError, McpGrants};
use rustykrab_channels::mcp::{
    InitializeResult, McpContent, McpToolDef, McpToolResult, ServerCapabilities,
};
use serde_json::{json, Value};

use crate::AppState;

/// The route's path.
pub const PATH: &str = "/api/mcp";

/// The bridge's session id. A JSON-RPC id names one call per session, so a
/// retry id is the pair, and a bridge that restarts starts new ids.
pub const SESSION_HEADER: &str = "x-rustykrab-mcp-session";

/// The MCP revisions served, newest first. Each has the same `initialize`
/// handshake and tools methods. Claude Code 2.1.295 asks for 2025-11-25
/// after its `server/discover` probe; Codex 0.159.3 asks for 2025-06-18.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Headers a proxy adds to name the client it forwards for.
const FORWARDING_HEADERS: [&str; 4] = [
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "tailscale-user-login",
];

/// The largest message read. MCP requests are small; tool arguments too.
const BODY_LIMIT: usize = 1 << 20;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_ERROR: i64 = -32000;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route(PATH, post(handle))
}

async fn handle(State(state): State<AppState>, request: Request) -> Response {
    if !local_caller(&request) {
        return refused(
            StatusCode::FORBIDDEN,
            "loopback_only",
            "the MCP route answers only a process on this machine",
        );
    }
    let Some(token) = bearer(request.headers()) else {
        return not_live();
    };
    if !state.mcp_grants.is_live(&token) {
        return not_live();
    }
    let session = request
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let Ok(body) = axum::body::to_bytes(request.into_body(), BODY_LIMIT).await else {
        return refused(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "an MCP message is at most 1 MiB",
        );
    };
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Json(reply(
            Value::Null,
            Err((PARSE_ERROR, "not a JSON-RPC message".into())),
        ))
        .into_response();
    };
    match answer(&state.mcp_grants, &token, &session, message).await {
        Ok(Some(reply)) => Json(reply).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(_) => not_live(),
    }
}

/// The JSON-RPC reply to `message`, `None` for a notification. Only a
/// grant that stops being live is an error: everything else is a reply.
async fn answer(
    grants: &McpGrants,
    token: &str,
    session: &str,
    message: Value,
) -> Result<Option<Value>, CallError> {
    let method = message.get("method").and_then(Value::as_str);
    let Some(id) = message.get("id").cloned() else {
        // A notification (`notifications/initialized`, a cancellation) or
        // a stray response: nothing to answer.
        return Ok(None);
    };
    let Some(method) = method else {
        return Ok(Some(reply(
            id,
            Err((INVALID_REQUEST, "not a JSON-RPC request".into())),
        )));
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            grants.initialize(token)?;
            Ok(initialize(&params))
        }
        "ping" if grants.is_live(token) => Ok(json!({})),
        "ping" => return Err(CallError::NotLive),
        "tools/list" => Ok(tools_list(grants.list(token)?)),
        "tools/call" => call(grants, token, session, &id, &params).await?,
        _ => Err((METHOD_NOT_FOUND, format!("`{method}` is not served here"))),
    };
    Ok(Some(reply(id, result)))
}

/// `initialize`: the client's revision when it is one served here, else
/// the newest, and tools as the one capability.
fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    serde_json::to_value(InitializeResult {
        protocol_version: version.into(),
        capabilities: ServerCapabilities {
            tools: Some(json!({"listChanged": false})),
            ..ServerCapabilities::default()
        },
        server_info: Some(json!({"name": "rustykrab", "version": rustykrab_core::VERSION})),
    })
    .expect("serializable initialize result")
}

fn tools_list(schemas: Vec<rustykrab_core::types::ToolSchema>) -> Value {
    let tools: Vec<McpToolDef> = schemas
        .into_iter()
        .map(|schema| McpToolDef {
            name: schema.name,
            description: Some(schema.description),
            input_schema: schema.parameters,
        })
        .collect();
    json!({ "tools": tools })
}

async fn call(
    grants: &McpGrants,
    token: &str,
    session: &str,
    id: &Value,
    params: &Value,
) -> Result<Result<Value, (i64, String)>, CallError> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Ok(Err((INVALID_PARAMS, "tools/call names no tool".into())));
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let retry = format!("{session}:{id}");
    Ok(match grants.call(token, &retry, name, args).await {
        Ok(Ok(value)) => Ok(tool_result(value, false)),
        Ok(Err(failure)) => Ok(tool_result(Value::String(failure), true)),
        Err(CallError::NotLive) => return Err(CallError::NotLive),
        Err(refusal @ (CallError::NotGranted | CallError::ReadOnly)) => {
            Err((INVALID_PARAMS, format!("`{name}`: {refusal}")))
        }
        Err(refusal) => Err((SERVER_ERROR, refusal.to_string())),
    })
}

/// A tool's result as MCP text content: a string as it is, anything else
/// as its JSON.
fn tool_result(value: Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(text) => text,
        other => other.to_string(),
    };
    serde_json::to_value(McpToolResult {
        content: vec![McpContent {
            content_type: "text".into(),
            text: Some(text),
            data: None,
            mime_type: None,
        }],
        is_error,
    })
    .expect("serializable tool result")
}

fn reply(id: Value, result: Result<Value, (i64, String)>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    }
}

/// Whether the connection itself is from this machine and nothing says it
/// was forwarded for someone else. Missing connection metadata fails closed.
fn local_caller(request: &Request) -> bool {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_canonical());
    peer.is_some_and(|ip| ip.is_loopback())
        && !FORWARDING_HEADERS
            .iter()
            .any(|name| request.headers().contains_key(*name))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn not_live() -> Response {
    refused(
        StatusCode::UNAUTHORIZED,
        "grant_not_live",
        "no live MCP grant: this route takes a native run's grant and nothing else",
    )
}

fn refused(status: StatusCode, error: &str, message: &str) -> Response {
    (status, Json(json!({"error": error, "message": message}))).into_response()
}

#[cfg(test)]
mod tests;
