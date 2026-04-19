use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

use crate::AppState;
use rustykrab_agent::{AgentEvent, AgentRunner, OnMessageCallback};
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::session::Session;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_memory::types::{ConversationTurn, LifecycleStage, TurnMetadata};
use rustykrab_memory::MemorySystem;
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
        tracing::debug!(
            channel_source = source.as_str(),
            channel_id = ?conv.channel_id,
            channel_thread_id = ?conv.channel_thread_id,
            "injected channel context into system prompt"
        );
    } else {
        tracing::debug!("no channel_source on conversation — skipping channel context");
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

/// Translate an agent `Message` into a memory `ConversationTurn`.
fn message_to_turn(msg: &Message, session_id: Uuid, turn_number: u32) -> ConversationTurn {
    let speaker = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let content = match &msg.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::ToolCall(tc) => {
            format!("tool_call:{}({})", tc.name, tc.arguments)
        }
        MessageContent::MultiToolCall(tcs) => tcs
            .iter()
            .map(|tc| format!("tool_call:{}({})", tc.name, tc.arguments))
            .collect::<Vec<_>>()
            .join("\n"),
        MessageContent::ToolResult(tr) => format!("tool_result:{}", tr.output),
    };
    let involves_tool_use = matches!(
        msg.content,
        MessageContent::ToolCall(_)
            | MessageContent::MultiToolCall(_)
            | MessageContent::ToolResult(_)
    );
    // Same estimator the agent runner uses for compaction budgeting.
    let token_count = Some(((content.len() as f64 / 3.5).ceil() as u32).saturating_add(4));

    ConversationTurn {
        id: msg.id,
        session_id,
        turn_number,
        speaker: speaker.to_string(),
        content,
        token_count,
        metadata: TurnMetadata {
            involves_tool_use,
            user_flagged: false,
            tags: Vec::new(),
        },
    }
}

/// Build an `on_message` callback that auto-persists every conversation
/// turn into working memory.  Returns `None` when memory isn't wired —
/// the runner then behaves as it did before (no persistence).
///
/// The callback is sync (the agent loop is sync at the hook) but memory
/// writes are async, so each call spawns a detached task.  Failures are
/// logged but don't block the agent loop — memory is eventual-consistency
/// relative to the conversation. System messages are skipped; they are
/// infrastructure (agent prompt, warnings) rather than conversation content.
/// Duplicate content is de-duplicated on the memory side via SHA-256 hash,
/// so re-firing the callback for an already-persisted message is safe.
fn build_memory_callback(state: &AppState, conv: &Conversation) -> Option<OnMessageCallback> {
    let memory: Arc<MemorySystem> = state.memory.clone()?;
    let agent_id = state.agent_id?;
    let session_id = conv.id;
    // Start the turn counter from the current message count so turns
    // are numbered consistently across a multi-request conversation.
    let turn_counter = Arc::new(AtomicU32::new(conv.messages.len() as u32));

    Some(Arc::new(move |msg: &Message| {
        if msg.role == Role::System {
            return;
        }
        let turn_number = turn_counter.fetch_add(1, Ordering::Relaxed);
        let turn = message_to_turn(msg, session_id, turn_number);
        let memory = Arc::clone(&memory);
        tokio::spawn(async move {
            if let Err(e) = memory
                .retain_with_stage(turn, agent_id, LifecycleStage::Working)
                .await
            {
                tracing::warn!(error = %e, "failed to persist turn to working memory");
            }
        });
    }))
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
    tracing::debug!(
        tool_count = tool_names.len(),
        tools = ?tool_names,
        "granting session capabilities for available tools"
    );
    let caps = CapabilitySet::for_tools_permissive(&tool_names);
    let session = Session::with_capabilities(conv.id, caps);

    // Resolve profile again for agent config (cheap — no LLM call on cache hit).
    let profile = state.profile_for(user_content).await;

    let mut runner = AgentRunner::new(
        state.provider.clone(),
        state.tools.clone(),
        state.sandbox.clone(),
    )
    .with_config(profile.to_agent_config())
    .with_active_tools(state.active_tools.clone());

    if let Some(cb) = build_memory_callback(state, conv) {
        // The inbound user message was pushed onto conv.messages by
        // routes.rs before the runner was constructed, so it never goes
        // through push_message. Fire the callback once so the user turn
        // is persisted alongside everything the runner generates.
        if let Some(last) = conv.messages.last() {
            if last.role == Role::User {
                cb(last);
            }
        }
        runner = runner.with_on_message(cb);
    }

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
