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
//! notification is accepted with 202 and no body; a batch is refused.
//!
//! The route has its own auth path, and only its own:
//!
//! - The caller is a process on this machine: the connection's own peer is
//!   loopback, the `Host` it asked for is loopback, and the request carries
//!   no forwarding header. A local proxy such as `tailscale serve` connects
//!   from loopback too, but keeps the outside `Host` and says whom it
//!   forwards for, so a request it forwarded is refused rather than trusted.
//! - The bearer is a live grant. `auth::require_auth` passes this path
//!   through untouched, so the owner's token and device tokens are not
//!   accepted here, and a grant authenticates no other route.
//! - The rate limiter does not count a request bearing a live grant; the
//!   grant's quota and its one call in flight bound it.
//!
//! A token that is not a live grant's (never minted, revoked, expired) is
//! 401 whatever the message. What a live grant refuses (a tool outside it,
//! its quota spent, a second call in flight, a reused id, a call after the
//! run ended) is a JSON-RPC error, and a failed or refused call is a result
//! with `isError`.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rustykrab_agent::mcp_grants::{CallError, McpGrants};
use serde_json::{json, Value};

use crate::AppState;

/// The route's path.
pub const PATH: &str = "/api/mcp";

/// The bridge's session id. A JSON-RPC id names one call per session, so a
/// request id is the pair, and a bridge that restarts starts new ids.
pub const SESSION_HEADER: &str = "x-rustykrab-mcp-session";

/// The MCP revisions served, newest first: the ones the native CLIs ask
/// for. Claude Code 2.1.295 asks for 2025-11-25 after its
/// `server/discover` probe; Codex 0.159.3 asks for 2025-06-18.
const PROTOCOL_VERSIONS: [&str; 2] = ["2025-11-25", "2025-06-18"];

/// Headers a proxy adds to say what it forwarded and for whom.
const FORWARDING_HEADERS: [&str; 9] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "via",
    "cf-connecting-ip",
    "tailscale-user-login",
    "tailscale-user-name",
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
    let Some(token) = crate::rate_limit::bearer_token(request.headers()) else {
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
    let reply = match serde_json::from_slice::<Value>(&body) {
        Err(_) => Some(error(Value::Null, PARSE_ERROR, "not a JSON-RPC message")),
        Ok(message) if !message.is_object() => Some(error(
            Value::Null,
            INVALID_REQUEST,
            "one JSON-RPC message per request; batches are not served",
        )),
        Ok(message) => match answer(&state.mcp_grants, &token, &session, message).await {
            Ok(reply) => reply,
            Err(_) => return not_live(),
        },
    };
    match reply {
        Some(reply) => Json(reply).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
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
    let Some(id) = message.get("id").cloned() else {
        // A notification (`notifications/initialized`, a cancellation) or
        // a stray response: nothing to answer.
        return Ok(None);
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(Some(error(id, INVALID_REQUEST, "not a JSON-RPC request")));
    };
    let params = &message["params"];
    let result = match method {
        "initialize" => {
            grants.initialize(token)?;
            let asked = params["protocolVersion"].as_str();
            let version = asked
                .filter(|v| PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "rustykrab", "version": rustykrab_core::VERSION},
            })
        }
        "ping" if grants.is_live(token) => json!({}),
        "ping" => return Err(CallError::NotLive),
        "tools/list" => {
            let tools: Vec<Value> = grants
                .list(token)?
                .into_iter()
                .map(|schema| {
                    json!({
                        "name": schema.name,
                        "description": schema.description,
                        "inputSchema": schema.parameters,
                    })
                })
                .collect();
            json!({ "tools": tools })
        }
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Ok(Some(error(id, INVALID_PARAMS, "tools/call names no tool")));
            };
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match grants
                .call(token, &format!("{session}:{id}"), name, args)
                .await
            {
                Ok(Ok(value)) => tool_result(value, false),
                Ok(Err(failure)) => tool_result(Value::String(failure), true),
                Err(CallError::NotLive) => return Err(CallError::NotLive),
                Err(CallError::NotGranted) => {
                    return Ok(Some(error(
                        id,
                        INVALID_PARAMS,
                        &format!("`{name}`: {}", CallError::NotGranted),
                    )))
                }
                Err(refusal) => return Ok(Some(error(id, SERVER_ERROR, &refusal.to_string()))),
            }
        }
        _ => {
            let missing = format!("`{method}` is not served here");
            return Ok(Some(error(id, METHOD_NOT_FOUND, &missing)));
        }
    };
    Ok(Some(json!({"jsonrpc": "2.0", "id": id, "result": result})))
}

/// A call's result as MCP text content: a string as it is, anything else
/// as its JSON.
fn tool_result(value: Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(text) => text,
        other => other.to_string(),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Whether the connection itself is from this machine, it asked for this
/// machine by a loopback name, and nothing says it was forwarded. Missing
/// connection metadata fails closed.
fn local_caller(request: &Request) -> bool {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_canonical());
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::uri::Authority>().ok());
    let loopback_host = host.is_some_and(|host| {
        let name = host.host().trim_start_matches('[').trim_end_matches(']');
        name.eq_ignore_ascii_case("localhost")
            || name
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    peer.is_some_and(|ip| ip.is_loopback())
        && loopback_host
        && !FORWARDING_HEADERS
            .iter()
            .any(|name| request.headers().contains_key(*name))
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
