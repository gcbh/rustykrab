use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

use openclaw_agent::{AgentEvent, AgentRunner};
use openclaw_core::capability::CapabilitySet;
use openclaw_core::session::Session;
use openclaw_core::types::{Conversation, Message, MessageContent, Role};
use openclaw_skills::SystemPromptBuilder;
use openclaw_store::memory::extract_keywords;

use crate::AppState;

/// Shared setup: resolve profile, build system prompt, inject it,
/// create session and runner. Returns `(AgentRunner, Session)`.
async fn prepare_agent(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
) -> Result<(AgentRunner, Session), StatusCode> {
    // 1. Resolve the harness profile.
    // If the model self-classified on a previous turn, use that.
    // Otherwise, fall back to the router (keyword or LLM based).
    let profile = if let Some(ref detected) = conv.detected_profile {
        let profile = state.profile_for_name(detected);
        tracing::info!(profile = %profile.name, source = "self-classified", "harness profile selected");
        profile
    } else {
        let profile = state.profile_for(user_content).await;
        tracing::info!(profile = %profile.name, source = "router", "harness profile selected");
        profile
    };

    // 2. Collect tool schemas for the prompt builder.
    let schemas: Vec<_> = state.tools.iter().map(|t| t.schema()).collect();

    // 3. Build the system prompt.
    let mut builder = SystemPromptBuilder::new()
        .with_identity(&profile.agent_name, &profile.agent_description)
        .with_tool_guidance(&schemas)
        .with_security_policy();

    // Only ask for self-classification if we don't have one yet.
    if conv.detected_profile.is_none() {
        builder = builder.with_self_classification();
    }

    if profile.chain_of_thought {
        builder = builder.with_chain_of_thought();
    }
    if let Some(task_guidance) = profile.task_type_guidance() {
        builder = builder.with_task_guidance(task_guidance);
    }
    // Associative memory recall: extract keywords from the user's message,
    // match against stored memory tags, and inject relevant facts.
    // No LLM call — just keyword extraction + tag matching.
    let keywords = extract_keywords(user_content);
    if !keywords.is_empty() {
        match state.store.memories().recall(conv.id, &keywords) {
            Ok(memories) if !memories.is_empty() => {
                let mut recall_text = String::from("RECALLED MEMORIES (relevant to this message):\n");
                for mem in memories.iter().take(10) {
                    recall_text.push_str(&format!("- [{}] {}\n", mem.id, mem.fact));
                }
                builder = builder.with_memory(&recall_text);
                tracing::info!(
                    count = memories.len(),
                    keywords = ?keywords,
                    "associative recall matched memories"
                );
            }
            Ok(_) => {} // no matches — that's fine
            Err(e) => {
                tracing::warn!("associative recall failed: {e}");
            }
        }
    }

    // Inject SKILL.md catalog (only satisfied skills).
    let satisfied: Vec<_> = state
        .skill_registry
        .md_skills()
        .into_iter()
        .filter(|s| s.validation.is_satisfied())
        .collect();
    if !satisfied.is_empty() {
        let refs: Vec<&openclaw_skills::SkillMd> = satisfied.iter().map(|s| s.as_ref()).collect();
        builder = builder.with_available_skills(&refs);
    }

    let system_prompt = builder.build();

    // 4. Inject system prompt as first message.
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

    // 5. Create an ephemeral session with capabilities for all registered tools.
    let tool_names: Vec<&str> = state.tools.iter().map(|t| t.name()).collect();
    let caps = CapabilitySet::for_tools_permissive(&tool_names);
    let session = Session::with_capabilities(conv.id, caps);

    // 6. Create the agent runner with profile-derived config.
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

/// Run the full agent pipeline on a conversation (non-streaming).
///
/// Used by the HTTP handler where the full response is returned at once.
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

/// Run the full agent pipeline with streaming events.
///
/// Used by the WebSocket handler to forward text deltas and tool
/// lifecycle events to the client in real time.
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
