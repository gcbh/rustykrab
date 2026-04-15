use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

use crate::AppState;
use rustykrab_agent::{AgentEvent, AgentRunner};
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::session::Session;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_skills::SystemPromptBuilder;

/// Build the system prompt and inject it as the first message in the conversation.
async fn build_and_inject_system_prompt(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
) {
    // 1. Resolve the harness profile (for agent loop config, not prompt injection).
    let profile = state.profile_for(user_content).await;
    tracing::info!(profile = %profile.name, "harness profile selected");

    // 2. Build the minimal system prompt.
    let mut builder = SystemPromptBuilder::new()
        .with_identity(&profile.agent_name)
        .with_security_policy();

    // Inject SKILL.md catalog (only satisfied skills).
    let satisfied: Vec<_> = state
        .skill_registry
        .md_skills()
        .into_iter()
        .filter(|s| s.validation.is_satisfied())
        .collect();
    if !satisfied.is_empty() {
        let refs: Vec<&rustykrab_skills::SkillMd> = satisfied.iter().map(|s| s.as_ref()).collect();
        builder = builder.with_available_skills(&refs);
    }

    let mut system_prompt = builder.build();

    // Append channel context so the agent knows where this conversation lives.
    if let Some(ref source) = conv.channel_source {
        system_prompt.push_str("\n\n## Channel context\n");
        system_prompt.push_str(&format!("- Source: {source}\n"));
        if let Some(ref cid) = conv.channel_id {
            system_prompt.push_str(&format!("- Chat ID: {cid}\n"));
        }
        if let Some(ref tid) = conv.channel_thread_id {
            system_prompt.push_str(&format!("- Thread ID: {tid}\n"));
        }
    }

    // 3. Inject system prompt as first message.
    if conv
        .messages
        .first()
        .map(|m| m.role == Role::System)
        .unwrap_or(false)
    {
        conv.messages[0].content = MessageContent::Text(system_prompt);
    } else {
        conv.messages.insert(
            0,
            Message {
                id: Uuid::new_v4(),
                role: Role::System,
                content: MessageContent::Text(system_prompt),
                created_at: Utc::now(),
            },
        );
    }
}

/// Shared setup: build system prompt, inject it, create session and runner.
/// Returns `(AgentRunner, Session)`.
async fn prepare_agent(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
) -> Result<(AgentRunner, Session), StatusCode> {
    build_and_inject_system_prompt(state, conv, user_content).await;

    // Create an ephemeral session with capabilities for available registered tools.
    let tool_names: Vec<&str> = state
        .tools
        .iter()
        .filter(|t| t.available())
        .map(|t| t.name())
        .collect();
    let caps = CapabilitySet::for_tools_permissive(&tool_names);
    let session = Session::with_capabilities(conv.id, caps);

    // Resolve profile again for agent config (cheap — no LLM call on cache hit).
    let profile = state.profile_for(user_content).await;

    let runner = AgentRunner::new(
        state.provider.clone(),
        state.tools.clone(),
        state.sandbox.clone(),
    )
    .with_config(profile.to_agent_config());

    Ok((runner, session))
}

/// Extract the last assistant text message from a conversation.
fn extract_assistant_message(conv: &Conversation) -> Result<Message, StatusCode> {
    conv.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.content.as_text().is_some())
        .cloned()
        .ok_or_else(|| {
            tracing::error!("agent loop completed but no assistant text message found");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Run the agent loop on a conversation (non-streaming).
pub async fn run_agent(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
) -> Result<Message, StatusCode> {
    let (runner, session) = prepare_agent(state, conv, user_content).await?;

    runner.run(conv, &session).await.map_err(|e| {
        tracing::error!("agent error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    extract_assistant_message(conv)
}

/// Run the agent loop with streaming events.
pub async fn run_agent_streaming(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
) -> Result<Message, StatusCode> {
    let (runner, session) = prepare_agent(state, conv, user_content).await?;

    runner
        .run_streaming(conv, &session, on_event)
        .await
        .map_err(|e| {
            tracing::error!("agent error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    extract_assistant_message(conv)
}
