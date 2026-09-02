//! Assemble and run one agent turn.
//!
//! This was `rustykrab-gateway::orchestrate`. Nothing in it is about HTTP —
//! it builds the system prompt, derives the session's capabilities, installs
//! the memory write-back hook and drives the runner — but living in the Axum
//! crate meant a Telegram loop had to depend on a web server to run a turn.
//!
//! Errors are a plain enum rather than an HTTP status. The gateway maps them
//! at its own boundary, which is where that decision belongs.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::context::AgentContext;
use crate::error::RuntimeError;
use rustykrab_agent::{AgentEvent, AgentHandle, AgentRunner, HarnessProfile, OnMessageCallback};
use rustykrab_core::capability::{Capability, CapabilitySet};
use rustykrab_core::session::Session;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_memory::types::{ConversationTurn, LifecycleStage, TurnMetadata};
use rustykrab_memory::MemorySystem;
use rustykrab_skills::SystemPromptBuilder;

/// Optional knobs for an agent run. Most callers (HTTP, channels) use
/// `RunOptions::default()`; the cron scheduler uses these to inject a
/// SKILL.md body into the system prompt and force tool use on the first
/// iteration.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// When set, `(name, body)` is wrapped in `<skill_instructions>` and
    /// appended to the system prompt so the model has the full recipe
    /// from turn 0 without a `skills`-tool round-trip.
    pub active_skill: Option<(String, String)>,
    /// When `true`, the runner makes its first LLM call with
    /// `tool_choice = "any"`, forcing the model to invoke a tool. Used
    /// for scheduled tasks so the model can't waste the slot on a
    /// greeting.
    pub force_tool_use_first_iteration: bool,
    /// Ceiling on agent iterations for this run, overriding the profile.
    ///
    /// The profile default is 200 (100 on the coding profile), which is a
    /// budget rather than a safety net: a run that cannot make progress
    /// spends all of it. Callers that know their work should be short --
    /// a credential wake resuming one stalled turn -- say so here, so a
    /// stuck run reports quickly instead of grinding.
    pub max_iterations: Option<usize>,
    /// Tools this run may not use, withheld before capabilities are
    /// granted rather than rejected at call time — a tool the session
    /// has no capability for is never offered to the model, so it does
    /// not waste a turn discovering it is forbidden.
    ///
    /// Used by the delegated-task worker to deny onward delegation to a
    /// run whose hop budget is spent.
    pub denied_tools: Vec<String>,
}

/// Build the system prompt and inject it as the first message in the conversation.
///
/// `profile` is the harness profile already resolved by the caller —
/// resolving it once and passing it in avoids a second (potentially
/// LLM-backed) classification per turn.
async fn build_and_inject_system_prompt(
    ctx: &AgentContext,
    conv: &mut Conversation,
    profile: &HarnessProfile,
    options: &RunOptions,
) {
    // 1. Build the minimal system prompt.
    //
    // Date is rendered at day granularity so the system block stays
    // cache-friendly: it only changes once per UTC day. Models that need
    // sub-day precision should call a clock tool on demand.
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let mut builder = SystemPromptBuilder::new()
        .with_identity(&profile.agent_name)
        .with_current_date(&today)
        .with_security_policy();

    // Inject SKILL.md catalog (only satisfied skills).
    let all_md = ctx.skill_registry.md_skills();
    let (satisfied, unsatisfied): (Vec<_>, Vec<_>) = all_md
        .into_iter()
        .partition(|s| s.validation.is_satisfied());
    let included: Vec<&str> = satisfied
        .iter()
        .map(|s| s.frontmatter.name.as_str())
        .collect();
    let excluded: Vec<String> = unsatisfied
        .iter()
        .map(|s| {
            let mut reasons = Vec::new();
            if !s.validation.missing_env.is_empty() {
                reasons.push(format!("missing_env={:?}", s.validation.missing_env));
            }
            if !s.validation.missing_bins.is_empty() {
                reasons.push(format!("missing_bins={:?}", s.validation.missing_bins));
            }
            format!("{} ({})", s.frontmatter.name, reasons.join(", "))
        })
        .collect();
    tracing::info!(
        included_count = included.len(),
        excluded_count = excluded.len(),
        included = ?included,
        excluded = ?excluded,
        "SKILL.md catalog for system prompt"
    );
    if !satisfied.is_empty() {
        let refs: Vec<&rustykrab_skills::SkillMd> = satisfied.iter().map(|s| s.as_ref()).collect();
        builder = builder.with_available_skills(&refs);
    }

    // When the caller (cron) has pre-resolved a SKILL.md for this run,
    // inline its full body into the system prompt. The skill recipe is
    // instructions, not data — placing it in `system` rather than the
    // user message keeps it cached across iterations and clearly framed
    // as authoritative guidance for the model.
    if let Some((name, body)) = options.active_skill.as_ref() {
        tracing::info!(skill = %name, "injecting skill body into system prompt");
        builder = builder.with_active_skill(name, body);
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

    // 2. Inject system prompt as first message.
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
                agent_version: Message::version_stamp(),
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
        MessageContent::MultiPart(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                rustykrab_core::types::ContentBlock::Text { text } => Some(text.clone()),
                rustykrab_core::types::ContentBlock::Image { media_type, .. } => {
                    Some(format!("[image:{media_type}]"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    let involves_tool_use = matches!(
        msg.content,
        MessageContent::ToolCall(_)
            | MessageContent::MultiToolCall(_)
            | MessageContent::ToolResult(_)
    );
    // Literally the same estimator the agent runner uses, not a second copy
    // of it that has to be kept in step.
    let token_count = Some(rustykrab_core::estimate_message_bytes(content.len()) as u32);

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
fn build_memory_callback(ctx: &AgentContext, conv: &Conversation) -> Option<OnMessageCallback> {
    let memory: Arc<MemorySystem> = ctx.memory.clone()?;
    let agent_id = ctx.agent_id?;
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
/// Build the permissive capability set for an ephemeral session, honoring
/// the gateway's `subagents_enabled` policy. Sub-agent tools require
/// `Capability::Subagent` in addition to the per-tool grant, so we only
/// add it when the gateway has been opted in (see
/// `AppState::with_subagents_enabled`).
fn build_session_capabilities(ctx: &AgentContext, tool_names: &[&str]) -> CapabilitySet {
    let mut caps = CapabilitySet::for_tools_permissive(tool_names);
    if ctx.subagents_enabled {
        caps.grant(Capability::Subagent);
    }
    if ctx.computer_use_enabled {
        caps.grant(Capability::ComputerUse);
    }
    caps
}

async fn prepare_agent(
    ctx: &AgentContext,
    conv: &mut Conversation,
    user_content: &str,
    options: &RunOptions,
) -> Result<(AgentRunner, Session), RuntimeError> {
    // Mark the system busy. Every channel reaches the agent through here,
    // so this one call covers Telegram, Slack, WebChat and scheduled runs
    // alike. This advances the "last busy" timestamp; what actually keeps
    // the downtime worker out for the duration of the turn is the
    // `RunGuard` the caller holds across the run itself.
    if let Some(agent_id) = ctx.agent_id {
        ctx.activity.record(agent_id);
    }

    // Resolve the harness profile once; it drives both the system prompt
    // and the agent config below.
    let profile = ctx.profile_for(user_content).await;
    tracing::info!(profile = %profile.name, "harness profile selected");

    build_and_inject_system_prompt(ctx, conv, &profile, options).await;

    // Create an ephemeral session with capabilities for available registered tools.
    let tool_names: Vec<&str> = ctx
        .tools
        .iter()
        .filter(|t| t.available())
        .map(|t| t.name())
        .filter(|name| !options.denied_tools.iter().any(|denied| denied == name))
        .collect();
    tracing::debug!(
        tool_count = tool_names.len(),
        tools = ?tool_names,
        subagents_enabled = ctx.subagents_enabled,
        "granting session capabilities for available tools"
    );
    let caps = build_session_capabilities(ctx, &tool_names);
    let session = Session::with_capabilities(conv.id, caps);

    let mut agent_config = profile.to_agent_config();
    agent_config.force_tool_use_first_iteration = options.force_tool_use_first_iteration;
    if let Some(cap) = options.max_iterations {
        agent_config.max_iterations = cap;
    }

    let mut runner = AgentRunner::new(ctx.provider.clone(), ctx.tools.clone(), ctx.sandbox.clone())
        .with_config(agent_config)
        .with_active_tools(ctx.active_tools.clone())
        .with_recall_store(ctx.recall.clone())
        .with_todo_store(ctx.todos.clone())
        .with_retrieval_log(ctx.retrieval_log.clone());

    // Outcome instrumentation (see `DREAMING.md`). Observational only: the
    // runner records how the run went and to which artifacts it should be
    // credited. Attributing to the active skill needs its name, which the
    // runner does not otherwise know.
    if ctx.outcome_capture_enabled {
        runner = runner.with_outcome_sink(Arc::new(ctx.store.outcomes()));
        if let Some((name, _)) = options.active_skill.as_ref() {
            runner = runner.with_active_skill(name.clone());
        }
    }

    if let Some(cb) = build_memory_callback(ctx, conv) {
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
fn extract_assistant_message(conv: &Conversation) -> Result<Message, RuntimeError> {
    conv.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.content.as_text().is_some())
        .cloned()
        .ok_or_else(|| {
            tracing::error!("agent loop completed but no assistant text message found");
            RuntimeError::Internal
        })
}

/// Run the agent loop on a conversation (non-streaming).
///
/// `trace_id` correlates every log line and prompt-log row produced by
/// this run. Callers at HTTP boundaries thread the request's trace id in;
/// channel/scheduler entry points should mint a fresh one with
/// [`Uuid::new_v4`].
pub async fn run_agent(
    ctx: &AgentContext,
    conv: &mut Conversation,
    user_content: &str,
    trace_id: Uuid,
) -> Result<Message, RuntimeError> {
    run_agent_with_options(ctx, conv, user_content, trace_id, &RunOptions::default()).await
}

/// Like [`run_agent`] but accepts caller-supplied [`RunOptions`].
pub async fn run_agent_with_options(
    ctx: &AgentContext,
    conv: &mut Conversation,
    user_content: &str,
    trace_id: Uuid,
    options: &RunOptions,
) -> Result<Message, RuntimeError> {
    rustykrab_core::prompt_trace::with_trace_id(trace_id, async move {
        let (runner, session) = prepare_agent(ctx, conv, user_content, options).await?;

        // Held for the whole run. Timestamps alone cannot express "busy
        // right now": a turn that outlasts the idle threshold would read
        // as quiet while the agent was still working, and a background
        // pass could start underneath it. Dropping the guard — on the
        // error path too — also bumps the generation, so any pass that
        // overlapped this run is preempted rather than returned.
        let _busy = ctx
            .agent_id
            .map(|agent_id| ctx.activity.begin_run(agent_id));

        runner.run(conv, &session).await.map_err(|e| {
            tracing::error!(%trace_id, "agent error: {e}");
            RuntimeError::Internal
        })?;

        extract_assistant_message(conv)
    })
    .await
}

/// Start the event-driven agent loop, returning a handle for injecting
/// messages mid-run.
///
/// Callers (e.g. Telegram) use the `AgentHandle` to submit new user
/// messages while the agent is already processing, instead of dropping
/// them. The `Receiver<AgentEvent>` streams real-time progress events,
/// and the `JoinHandle` resolves to the final conversation.
pub async fn run_agent_interactive(
    ctx: &AgentContext,
    mut conv: Conversation,
    user_content: &str,
    trace_id: Uuid,
) -> std::result::Result<
    (
        AgentHandle,
        mpsc::Receiver<AgentEvent>,
        JoinHandle<rustykrab_core::Result<Conversation>>,
    ),
    RuntimeError,
> {
    rustykrab_core::prompt_trace::with_trace_id(trace_id, async move {
        // Resolve the harness profile once for both the system prompt and
        // the agent config.
        let profile = ctx.profile_for(user_content).await;
        tracing::info!(profile = %profile.name, "harness profile selected");

        build_and_inject_system_prompt(ctx, &mut conv, &profile, &RunOptions::default()).await;

        let tool_names: Vec<&str> = ctx
            .tools
            .iter()
            .filter(|t| t.available())
            .map(|t| t.name())
            .collect();
        let caps = build_session_capabilities(ctx, &tool_names);
        let session = Session::with_capabilities(conv.id, caps);

        let runner = AgentRunner::new(ctx.provider.clone(), ctx.tools.clone(), ctx.sandbox.clone())
            .with_config(profile.to_agent_config())
            .with_todo_store(ctx.todos.clone());

        // The agent loop runs in a tokio::spawn'd task inside `start`, so
        // the task-local trace id won't follow it. Re-scope the spawned
        // future from inside the runner is not possible without changing
        // the runner API; for the interactive path we accept that the
        // agent task itself logs without trace_id. The caller can still
        // correlate by the conversation id printed at start.
        Ok(runner.start(conv, session))
    })
    .await
}

/// Run the agent loop with streaming events.
pub async fn run_agent_streaming(
    ctx: &AgentContext,
    conv: &mut Conversation,
    user_content: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
    trace_id: Uuid,
) -> Result<Message, RuntimeError> {
    run_agent_streaming_with_options(
        ctx,
        conv,
        user_content,
        on_event,
        trace_id,
        &RunOptions::default(),
    )
    .await
}

/// Like [`run_agent_streaming`] but accepts caller-supplied [`RunOptions`].
pub async fn run_agent_streaming_with_options(
    ctx: &AgentContext,
    conv: &mut Conversation,
    user_content: &str,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
    trace_id: Uuid,
    options: &RunOptions,
) -> Result<Message, RuntimeError> {
    rustykrab_core::prompt_trace::with_trace_id(trace_id, async move {
        let (runner, session) = prepare_agent(ctx, conv, user_content, options).await?;

        // See `run_agent_with_options` -- a long turn must not read as
        // idle while it is still running.
        let _busy = ctx
            .agent_id
            .map(|agent_id| ctx.activity.begin_run(agent_id));

        runner
            .run_streaming(conv, &session, on_event)
            .await
            .map_err(|e| {
                tracing::error!(%trace_id, "agent error: {e}");
                RuntimeError::Internal
            })?;

        extract_assistant_message(conv)
    })
    .await
}
