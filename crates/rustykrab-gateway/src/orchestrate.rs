use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

use crate::AppState;
use rustykrab_agent::{AgentEvent, AgentRunner};
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::orchestration::TaskComplexity;
use rustykrab_core::session::Session;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_skills::SystemPromptBuilder;

/// Build the system prompt and inject it as the first message in the conversation.
///
/// This is shared between the orchestration pipeline path and the
/// direct agent loop so both get the same identity, tool guidance,
/// security policy, memory recall, and skills catalog.
async fn build_and_inject_system_prompt(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
) {
    // 1. Resolve the harness profile.
    let profile = if let Some(ref detected) = conv.detected_profile {
        let profile = state.profile_for_name(detected);
        tracing::info!(profile = %profile.name, source = "self-classified", "harness profile selected");
        profile
    } else {
        let profile = state.profile_for(user_content).await;
        tracing::info!(profile = %profile.name, source = "router", "harness profile selected");
        profile
    };

    // 2. Collect tool schemas for the prompt builder (skip unavailable tools).
    let schemas: Vec<_> = state
        .tools
        .iter()
        .filter(|t| t.available())
        .map(|t| t.schema())
        .collect();

    // 3. Build the system prompt.
    let mut builder = SystemPromptBuilder::new()
        .with_identity(&profile.agent_name, &profile.agent_description)
        .with_tool_guidance(&schemas)
        .with_security_policy();

    // Always ask for self-classification — the model tags every response
    // so the profile stays current as the conversation evolves.
    builder = builder.with_self_classification();

    if profile.chain_of_thought {
        builder = builder.with_chain_of_thought();
    }
    if let Some(task_guidance) = profile.task_type_guidance() {
        builder = builder.with_task_guidance(task_guidance);
    }
    // Memory recall is now handled by the hybrid memory system
    // (rustykrab-memory) via the memory_search tool, not injected
    // into the system prompt from the old sled-based MemoryStore.

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
    let profile = if let Some(ref detected) = conv.detected_profile {
        state.profile_for_name(detected)
    } else {
        state.profile_for(user_content).await
    };

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
/// If the orchestration pipeline is enabled, routes through
/// decompose → execute → synthesize → refine. Otherwise falls
/// back to the simple agent loop.
pub async fn run_agent(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
) -> Result<Message, StatusCode> {
    // Try the orchestration pipeline first if enabled.
    if let Some(ref pipeline) = state.orchestration_pipeline {
        tracing::info!("routing through orchestration pipeline");

        // Build and inject the system prompt so the pipeline has full
        // agent identity, tool guidance, security policy, and memory.
        build_and_inject_system_prompt(state, conv, user_content).await;

        let context = conv
            .messages
            .first()
            .filter(|m| m.role == Role::System)
            .and_then(|m| m.content.as_text())
            .map(|s| s.to_string());

        let result = pipeline
            .run(user_content, TaskComplexity::Complex, context.as_deref())
            .await
            .map_err(|e| {
                tracing::error!("orchestration pipeline error: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        tracing::info!(
            stages = ?result.stages_executed,
            sub_tasks = result.sub_task_count,
            refinement_iterations = result.refinement_iterations,
            "orchestration pipeline completed"
        );

        // Add the pipeline result as an assistant message.
        let msg = Message {
            id: Uuid::new_v4(),
            role: Role::Assistant,
            content: MessageContent::Text(result.response),
            created_at: Utc::now(),
        };
        conv.messages.push(msg.clone());
        conv.updated_at = Utc::now();

        return Ok(msg);
    }

    // Fallback: simple agent loop.
    let (runner, session) = prepare_agent(state, conv, user_content).await?;

    runner.run(conv, &session).await.map_err(|e| {
        tracing::error!("agent error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    extract_assistant_message(conv)
}

/// Run the full agent pipeline with streaming events.
///
/// If the orchestration pipeline is enabled, routes through
/// decompose → execute → synthesize → refine (non-streaming,
/// since the pipeline doesn't support event callbacks yet).
/// Otherwise uses the streaming agent loop.
pub async fn run_agent_streaming(
    state: &AppState,
    conv: &mut Conversation,
    user_content: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
) -> Result<Message, StatusCode> {
    // Try the orchestration pipeline first if enabled.
    if let Some(ref pipeline) = state.orchestration_pipeline {
        tracing::info!("routing through orchestration pipeline (streaming path)");

        // Build and inject the system prompt so the pipeline has full
        // agent identity, tool guidance, security policy, and memory.
        build_and_inject_system_prompt(state, conv, user_content).await;

        on_event(AgentEvent::ToolCallStart {
            tool_name: "orchestration_pipeline".to_string(),
            call_id: "pipeline".to_string(),
        });

        let context = conv
            .messages
            .first()
            .filter(|m| m.role == Role::System)
            .and_then(|m| m.content.as_text())
            .map(|s| s.to_string());

        // The pipeline runs synchronously and can take many minutes.
        // Emit periodic heartbeat events so the SSE timeout doesn't
        // kill the connection while the pipeline is working.
        // Uses tokio::select! with an interval instead of unsafe transmute,
        // avoiding a potential use-after-free if the pipeline future is
        // cancelled before the heartbeat task is aborted.
        let result = {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            interval.tick().await; // first tick is immediate, consume it

            let pipeline_future =
                pipeline.run(user_content, TaskComplexity::Complex, context.as_deref());
            tokio::pin!(pipeline_future);

            loop {
                tokio::select! {
                    result = &mut pipeline_future => break result,
                    _ = interval.tick() => {
                        on_event(AgentEvent::Compressing); // heartbeat
                    }
                }
            }
        };

        let result = result.map_err(|e| {
            tracing::error!("orchestration pipeline error: {e}");
            on_event(AgentEvent::ToolCallEnd {
                tool_name: "orchestration_pipeline".to_string(),
                call_id: "pipeline".to_string(),
                success: false,
                error_message: Some(e.to_string()),
            });
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        on_event(AgentEvent::ToolCallEnd {
            tool_name: "orchestration_pipeline".to_string(),
            call_id: "pipeline".to_string(),
            success: true,
            error_message: None,
        });

        tracing::info!(
            stages = ?result.stages_executed,
            sub_tasks = result.sub_task_count,
            refinement_iterations = result.refinement_iterations,
            "orchestration pipeline completed"
        );

        // Emit the result as text deltas so the client sees it stream.
        on_event(AgentEvent::TextDelta(result.response.clone()));
        on_event(AgentEvent::Done);

        let msg = Message {
            id: Uuid::new_v4(),
            role: Role::Assistant,
            content: MessageContent::Text(result.response),
            created_at: Utc::now(),
        };
        conv.messages.push(msg.clone());
        conv.updated_at = Utc::now();

        return Ok(msg);
    }

    // Fallback: streaming agent loop.
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
