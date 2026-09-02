//! Turn-running, adapted to this transport.
//!
//! The turn itself lives in `rustykrab-runtime`, which knows nothing about
//! HTTP. These wrappers supply its [`AgentContext`] out of [`AppState`] and
//! translate its errors into status codes — which is the only part of the
//! job that is actually the web server's.
//!
//! Signatures are unchanged, so callers that took `&AppState` still do.

use rustykrab_agent::{AgentEvent, AgentHandle};
use rustykrab_core::types::{Conversation, Message};
use rustykrab_runtime::{RunOptions, RuntimeError};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::AppState;

/// Every runtime failure is an internal one from the web's point of view:
/// the request was well-formed and the agent could not complete it.
fn to_status(e: RuntimeError) -> axum::http::StatusCode {
    match e {
        RuntimeError::Internal => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub async fn run_agent(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
    trace_id: Uuid,
) -> Result<Message, axum::http::StatusCode> {
    rustykrab_runtime::run_agent(&state.agent, conv, user_content, trace_id)
        .await
        .map_err(to_status)
}

pub async fn run_agent_with_options(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
    trace_id: Uuid,
    options: &RunOptions,
) -> Result<Message, axum::http::StatusCode> {
    rustykrab_runtime::run_agent_with_options(&state.agent, conv, user_content, trace_id, options)
        .await
        .map_err(to_status)
}

pub async fn run_agent_interactive(
    state: &AppState,
    conv: Conversation,
    user_content: &str,
    trace_id: Uuid,
) -> Result<
    (
        AgentHandle,
        mpsc::Receiver<AgentEvent>,
        JoinHandle<rustykrab_core::Result<Conversation>>,
    ),
    axum::http::StatusCode,
> {
    rustykrab_runtime::run_agent_interactive(&state.agent, conv, user_content, trace_id)
        .await
        .map_err(to_status)
}

pub async fn run_agent_streaming(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
    trace_id: Uuid,
) -> Result<Message, axum::http::StatusCode> {
    rustykrab_runtime::run_agent_streaming(&state.agent, conv, user_content, on_event, trace_id)
        .await
        .map_err(to_status)
}

pub async fn run_agent_streaming_with_options(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
    trace_id: Uuid,
    options: &RunOptions,
) -> Result<Message, axum::http::StatusCode> {
    rustykrab_runtime::run_agent_streaming_with_options(
        &state.agent,
        conv,
        user_content,
        on_event,
        trace_id,
        options,
    )
    .await
    .map_err(to_status)
}
