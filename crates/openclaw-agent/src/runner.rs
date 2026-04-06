use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use openclaw_core::capability::Capability;
use openclaw_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent};
use openclaw_core::session::Session;
use openclaw_core::types::{
    Conversation, Message, MessageContent, Role, ToolCall, ToolResult, ToolSchema,
};
use openclaw_core::{Error, Result, Tool};
use uuid::Uuid;

use crate::sandbox::{Sandbox, SandboxPolicy};
use crate::trace::{ExecutionTracer, ToolTrace};

/// Events emitted by the agent loop during streaming execution.
///
/// These give callers (e.g. WebSocket handlers) real-time visibility
/// into the agent's progress: text tokens as they arrive, tool execution
/// lifecycle, and internal state changes like reflections and compression.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A partial text token from the model's response.
    TextDelta(String),
    /// The agent is about to execute a tool.
    ToolCallStart {
        tool_name: String,
        call_id: String,
    },
    /// A tool call has completed.
    ToolCallEnd {
        tool_name: String,
        call_id: String,
        success: bool,
    },
    /// The agent is reflecting after repeated errors.
    Reflecting,
    /// The agent is compressing conversation memory.
    Compressing,
    /// The agent loop has completed.
    Done,
}

/// Configuration for the agent runner.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum iterations before giving up.
    pub max_iterations: usize,
    /// Maximum consecutive errors before reflecting.
    pub max_consecutive_errors: usize,
    /// Maximum retries per failed tool call.
    pub max_tool_retries: u32,
    /// Estimated max context tokens for the model (used for budget calculations).
    /// Defaults to 128k which works for Claude and large Qwen models.
    pub max_context_tokens: usize,
    /// Fraction of context reserved for the conversation summary (0.0–1.0).
    /// The remaining budget is for recent messages + model response.
    /// Default 0.20 (20%) — enough for a rich summary without crowding live context.
    pub summary_budget_ratio: f64,
    /// Fraction of context reserved for the model's response.
    /// Default 0.15 (15%) — leaves room for a substantial reply.
    pub response_reserve_ratio: f64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            max_consecutive_errors: 3,
            max_tool_retries: 2,
            max_context_tokens: 128_000,
            summary_budget_ratio: 0.20,
            response_reserve_ratio: 0.15,
        }
    }
}

/// Runs the agent loop: send conversation to model, execute tool calls, repeat.
///
/// Improvements over a basic loop:
/// - **Multi-tool**: executes multiple tool calls in parallel when the model
///   requests them (e.g. Anthropic's parallel tool use)
/// - **Retry with reflection**: on tool errors, feeds the error back to the
///   model so it can self-correct rather than blindly retrying
/// - **Memory**: summarizes older messages to keep context within limits
pub struct AgentRunner {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    config: AgentConfig,
    tracer: ExecutionTracer,
}

impl AgentRunner {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        sandbox: Arc<dyn Sandbox>,
    ) -> Self {
        Self {
            provider,
            tools,
            sandbox,
            config: AgentConfig::default(),
            tracer: ExecutionTracer::new(),
        }
    }

    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// Access the execution tracer for this runner.
    pub fn tracer(&self) -> &ExecutionTracer {
        &self.tracer
    }

    /// Run the agent loop on a conversation within a session's capability scope.
    pub async fn run(&self, conv: &mut Conversation, session: &Session) -> Result<()> {
        if session.is_expired() {
            return Err(Error::Auth("session has expired".into()));
        }

        let schemas: Vec<ToolSchema> = self
            .tools
            .iter()
            .filter(|t| session.capabilities.can_use_tool(t.name()))
            .map(|t| t.schema())
            .collect();

        let mut consecutive_errors = 0;

        for iteration in 0..self.config.max_iterations {
            self.tracer.record_iteration();

            // Compress memory when estimated tokens exceed the budget.
            if self.should_compress(conv) {
                self.compress_memory(conv).await?;
                self.tracer.record_compression();
            }

            let ModelResponse {
                message,
                usage: _,
                stop_reason,
            } = self.provider.chat(&conv.messages, &schemas).await?;

            conv.messages.push(message.clone());
            conv.updated_at = Utc::now();

            // Handle tool calls (single or multi).
            if message.content.has_tool_calls() {
                let calls = message.content.tool_calls();
                tracing::info!(
                    iteration,
                    tool_count = calls.len(),
                    "executing tool calls"
                );

                // Execute all tool calls in parallel, recording traces.
                let results = self
                    .execute_tools_parallel_traced(calls, session)
                    .await;

                // Track errors for reflection.
                let had_errors = results.iter().any(|r| {
                    if let Ok(tr) = r {
                        tr.output.get("error").is_some()
                    } else {
                        true
                    }
                });

                // Push all results as messages.
                for result in results {
                    let tool_msg = match result {
                        Ok(tr) => Message {
                            id: Uuid::new_v4(),
                            role: Role::Tool,
                            content: MessageContent::ToolResult(tr),
                            created_at: Utc::now(),
                        },
                        Err(e) => {
                            // Create a synthetic error result.
                            Message {
                                id: Uuid::new_v4(),
                                role: Role::Tool,
                                content: MessageContent::ToolResult(ToolResult {
                                    call_id: "error".to_string(),
                                    output: serde_json::json!({ "error": e.to_string() }),
                                }),
                                created_at: Utc::now(),
                            }
                        }
                    };
                    conv.messages.push(tool_msg);
                }
                conv.updated_at = Utc::now();

                // Inject trace-informed guidance if tools are failing.
                if let Some(trace_guidance) = self.tracer.summary_for_prompt() {
                    // Only inject trace context every few iterations to avoid noise.
                    if iteration > 0 && iteration % 5 == 0 {
                        self.inject_trace_context(conv, &trace_guidance);
                    }
                }

                // Reflection on repeated errors.
                if had_errors {
                    consecutive_errors += 1;
                    if consecutive_errors >= self.config.max_consecutive_errors {
                        tracing::warn!(
                            consecutive_errors,
                            "injecting reflection prompt after repeated errors"
                        );
                        self.inject_reflection(conv);
                        consecutive_errors = 0;
                    }
                } else {
                    consecutive_errors = 0;
                }

                continue;
            }

            // If model hit max_tokens, it may be truncated — let it continue.
            if stop_reason == StopReason::MaxTokens {
                tracing::warn!("model hit max tokens, prompting to continue");
                conv.messages.push(Message {
                    id: Uuid::new_v4(),
                    role: Role::User,
                    content: MessageContent::Text("Continue.".to_string()),
                    created_at: Utc::now(),
                });
                continue;
            }

            // Text response — done.
            return Ok(());
        }

        Err(Error::Internal(format!(
            "agent exceeded max iterations ({})",
            self.config.max_iterations
        )))
    }

    /// Run the agent loop with streaming: text deltas are forwarded through
    /// the callback as they arrive, and tool lifecycle events are emitted
    /// so callers can show real-time progress.
    ///
    /// The callback must be `Send + Sync` because it may be invoked from
    /// the provider's streaming internals on a different task.
    pub async fn run_streaming(
        &self,
        conv: &mut Conversation,
        session: &Session,
        on_event: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<()> {
        if session.is_expired() {
            return Err(Error::Auth("session has expired".into()));
        }

        let schemas: Vec<ToolSchema> = self
            .tools
            .iter()
            .filter(|t| session.capabilities.can_use_tool(t.name()))
            .map(|t| t.schema())
            .collect();

        let mut consecutive_errors = 0;

        for iteration in 0..self.config.max_iterations {
            self.tracer.record_iteration();

            if self.should_compress(conv) {
                on_event(AgentEvent::Compressing);
                self.compress_memory(conv).await?;
                self.tracer.record_compression();
            }

            // Use chat_stream so text deltas are forwarded in real time.
            let stream_callback = |event: StreamEvent| {
                if let StreamEvent::TextDelta(delta) = event {
                    on_event(AgentEvent::TextDelta(delta));
                }
            };

            let ModelResponse {
                message,
                usage: _,
                stop_reason,
            } = self
                .provider
                .chat_stream(&conv.messages, &schemas, &stream_callback)
                .await?;

            conv.messages.push(message.clone());
            conv.updated_at = Utc::now();

            // Handle tool calls.
            if message.content.has_tool_calls() {
                let calls = message.content.tool_calls();
                tracing::info!(
                    iteration,
                    tool_count = calls.len(),
                    "executing tool calls"
                );

                // Emit start events.
                for call in &calls {
                    on_event(AgentEvent::ToolCallStart {
                        tool_name: call.name.clone(),
                        call_id: call.id.clone(),
                    });
                }

                let results = self
                    .execute_tools_parallel_traced(calls, session)
                    .await;

                let had_errors = results.iter().any(|r| {
                    if let Ok(tr) = r {
                        tr.output.get("error").is_some()
                    } else {
                        true
                    }
                });

                for result in results {
                    let (tool_msg, tool_name, call_id, success) = match result {
                        Ok(tr) => {
                            let msg = Message {
                                id: Uuid::new_v4(),
                                role: Role::Tool,
                                content: MessageContent::ToolResult(tr.clone()),
                                created_at: Utc::now(),
                            };
                            (msg, String::new(), tr.call_id, true)
                        }
                        Err(e) => {
                            let msg = Message {
                                id: Uuid::new_v4(),
                                role: Role::Tool,
                                content: MessageContent::ToolResult(ToolResult {
                                    call_id: "error".to_string(),
                                    output: serde_json::json!({ "error": e.to_string() }),
                                }),
                                created_at: Utc::now(),
                            };
                            (msg, String::new(), "error".to_string(), false)
                        }
                    };
                    on_event(AgentEvent::ToolCallEnd {
                        tool_name,
                        call_id,
                        success,
                    });
                    conv.messages.push(tool_msg);
                }
                conv.updated_at = Utc::now();

                if let Some(trace_guidance) = self.tracer.summary_for_prompt() {
                    if iteration > 0 && iteration % 5 == 0 {
                        self.inject_trace_context(conv, &trace_guidance);
                    }
                }

                if had_errors {
                    consecutive_errors += 1;
                    if consecutive_errors >= self.config.max_consecutive_errors {
                        tracing::warn!(
                            consecutive_errors,
                            "injecting reflection prompt after repeated errors"
                        );
                        on_event(AgentEvent::Reflecting);
                        self.inject_reflection(conv);
                        consecutive_errors = 0;
                    }
                } else {
                    consecutive_errors = 0;
                }

                continue;
            }

            if stop_reason == StopReason::MaxTokens {
                tracing::warn!("model hit max tokens, prompting to continue");
                conv.messages.push(Message {
                    id: Uuid::new_v4(),
                    role: Role::User,
                    content: MessageContent::Text("Continue.".to_string()),
                    created_at: Utc::now(),
                });
                continue;
            }

            // Text response — done.
            on_event(AgentEvent::Done);
            return Ok(());
        }

        Err(Error::Internal(format!(
            "agent exceeded max iterations ({})",
            self.config.max_iterations
        )))
    }

    /// Execute multiple tool calls in parallel, recording execution traces.
    async fn execute_tools_parallel_traced(
        &self,
        calls: Vec<&ToolCall>,
        session: &Session,
    ) -> Vec<Result<ToolResult>> {
        if calls.len() == 1 {
            let call = calls[0];
            let start = Instant::now();
            let result = self.execute_tool_checked(call, session).await;
            self.tracer.record(ToolTrace {
                tool_name: call.name.clone(),
                success: result.is_ok(),
                duration: start.elapsed(),
                error: result.as_ref().err().map(|e| e.to_string()),
            });
            return vec![result];
        }

        // Spawn all tool executions concurrently.
        let mut handles = Vec::with_capacity(calls.len());
        let call_names: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();

        for call in calls {
            let call = call.clone();
            let tools = self.tools.clone();
            let sandbox = self.sandbox.clone();
            let session_caps = session.capabilities.clone();
            let session_id = session.id;

            handles.push(tokio::spawn(async move {
                let start = Instant::now();
                let result =
                    execute_single_tool(&call, &tools, &sandbox, &session_caps, session_id).await;
                (result, call.name.clone(), start.elapsed())
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for (i, handle) in handles.into_iter().enumerate() {
            match handle.await {
                Ok((result, name, duration)) => {
                    self.tracer.record(ToolTrace {
                        tool_name: name,
                        success: result.is_ok(),
                        duration,
                        error: result.as_ref().err().map(|e| e.to_string()),
                    });
                    results.push(result);
                }
                Err(e) => {
                    self.tracer.record(ToolTrace {
                        tool_name: call_names.get(i).cloned().unwrap_or_default(),
                        success: false,
                        duration: std::time::Duration::ZERO,
                        error: Some(format!("task panicked: {e}")),
                    });
                    results.push(Err(Error::Internal(format!("task panicked: {e}"))));
                }
            }
        }

        results
    }

    async fn execute_tool_checked(
        &self,
        call: &ToolCall,
        session: &Session,
    ) -> Result<ToolResult> {
        execute_single_tool(
            call,
            &self.tools,
            &self.sandbox,
            &session.capabilities,
            session.id,
        )
        .await
    }

    /// Inject trace-informed context so the model can adapt its strategy.
    fn inject_trace_context(&self, conv: &mut Conversation, trace_summary: &str) {
        conv.messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(format!(
                "[Harness observation]\n{trace_summary}"
            )),
            created_at: Utc::now(),
        });
    }

    /// Inject a reflection message when the agent keeps hitting errors.
    /// This gives the model a chance to step back and try a different approach.
    fn inject_reflection(&self, conv: &mut Conversation) {
        conv.messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(
                "The previous tool calls produced errors. Please stop and reflect: \
                 What went wrong? What is a different approach you could take? \
                 Think step by step before making your next tool call."
                    .to_string(),
            ),
            created_at: Utc::now(),
        });
    }

    /// Estimate token count for a string using the 4-chars-per-token heuristic.
    /// This is intentionally conservative — slightly over-counting is safer
    /// than running into a context limit mid-generation.
    fn estimate_tokens(text: &str) -> usize {
        // ~4 chars per token for English; ~3 for code-heavy content.
        // We use 3.5 as a middle ground that errs on the safe side.
        (text.len() as f64 / 3.5).ceil() as usize
    }

    /// Estimate total token usage of all messages in a conversation.
    fn estimate_conversation_tokens(conv: &Conversation) -> usize {
        let mut total = 0;
        if let Some(ref summary) = conv.summary {
            total += Self::estimate_tokens(summary);
        }
        for msg in &conv.messages {
            total += match &msg.content {
                MessageContent::Text(t) => Self::estimate_tokens(t),
                MessageContent::ToolCall(tc) => {
                    Self::estimate_tokens(&tc.name)
                        + Self::estimate_tokens(&tc.arguments.to_string())
                }
                MessageContent::ToolResult(tr) => {
                    Self::estimate_tokens(&tr.output.to_string())
                }
                MessageContent::MultiToolCall(calls) => calls
                    .iter()
                    .map(|tc| {
                        Self::estimate_tokens(&tc.name)
                            + Self::estimate_tokens(&tc.arguments.to_string())
                    })
                    .sum(),
            };
            // Per-message overhead (role tokens, formatting).
            total += 4;
        }
        total
    }

    /// The token budget available for live messages (everything except
    /// the summary and the response reserve).
    fn live_message_budget(&self) -> usize {
        let total = self.config.max_context_tokens;
        let summary_budget = (total as f64 * self.config.summary_budget_ratio) as usize;
        let response_budget = (total as f64 * self.config.response_reserve_ratio) as usize;
        total.saturating_sub(summary_budget).saturating_sub(response_budget)
    }

    /// Maximum tokens the summary should occupy.
    fn summary_token_budget(&self) -> usize {
        (self.config.max_context_tokens as f64 * self.config.summary_budget_ratio) as usize
    }

    /// Determine whether the conversation needs compression.
    fn should_compress(&self, conv: &Conversation) -> bool {
        let estimated = Self::estimate_conversation_tokens(conv);
        let budget = self.live_message_budget();
        // Trigger compression when we've used 85% of the live budget.
        estimated > (budget as f64 * 0.85) as usize
    }

    /// Compress older messages into a summary to stay within context limits.
    ///
    /// Uses a token-aware budget: drops the oldest messages until the
    /// remaining messages fit within the live-message budget, then asks
    /// the model to summarize the dropped portion within the summary
    /// token budget.
    async fn compress_memory(&self, conv: &mut Conversation) -> Result<()> {
        let live_budget = self.live_message_budget();
        let summary_budget = self.summary_token_budget();
        // Approximate max chars for the summary (inverse of token estimate).
        let summary_max_chars = (summary_budget as f64 * 3.5) as usize;

        // Find the split point: walk backwards from the end, accumulating
        // tokens, until we've filled about 60% of the live budget.
        // This keeps a healthy buffer for the next few turns.
        let target = (live_budget as f64 * 0.60) as usize;
        let mut keep_tokens = 0;
        let mut keep_from = conv.messages.len();

        for (i, msg) in conv.messages.iter().enumerate().rev() {
            let msg_tokens = match &msg.content {
                MessageContent::Text(t) => Self::estimate_tokens(t),
                MessageContent::ToolCall(tc) => {
                    Self::estimate_tokens(&tc.name)
                        + Self::estimate_tokens(&tc.arguments.to_string())
                }
                MessageContent::ToolResult(tr) => {
                    Self::estimate_tokens(&tr.output.to_string())
                }
                MessageContent::MultiToolCall(calls) => calls
                    .iter()
                    .map(|tc| {
                        Self::estimate_tokens(&tc.name)
                            + Self::estimate_tokens(&tc.arguments.to_string())
                    })
                    .sum(),
            } + 4;

            if keep_tokens + msg_tokens > target {
                keep_from = i + 1;
                break;
            }
            keep_tokens += msg_tokens;
            keep_from = i;
        }

        // Don't compress if there's almost nothing to drop.
        if keep_from <= 2 {
            return Ok(());
        }

        let old_messages = &conv.messages[..keep_from];

        // Build the text of messages to summarize.
        let mut summary_text = String::new();
        for msg in old_messages {
            let role = match msg.role {
                Role::System => continue,
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::Tool => "Tool",
            };
            if let Some(text) = msg.content.as_text() {
                summary_text.push_str(&format!("{role}: {text}\n"));
            }
        }

        if summary_text.is_empty() {
            return Ok(());
        }

        // Incorporate existing summary for continuity.
        let existing_summary = conv
            .summary
            .as_deref()
            .map(|s| format!("Previous summary: {s}\n\n"))
            .unwrap_or_default();

        let summary_prompt = vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(format!(
                "{existing_summary}Summarize this conversation concisely, preserving \
                 key facts, decisions, tool results, and any information needed to \
                 continue the conversation. Your summary MUST be under \
                 {summary_max_chars} characters.\n\n{summary_text}"
            )),
            created_at: Utc::now(),
        }];

        let response = self.provider.chat(&summary_prompt, &[]).await?;
        let mut summary = response
            .message
            .content
            .as_text()
            .unwrap_or("(summary failed)")
            .to_string();

        // Hard-truncate if the model exceeded the budget (shouldn't happen
        // often, but guarantees we don't blow the context on re-injection).
        if Self::estimate_tokens(&summary) > summary_budget {
            // Truncate to stay within budget, leaving room for the ellipsis.
            let max_bytes = summary_max_chars.min(summary.len());
            // Find a clean char boundary.
            let truncate_at = summary
                .char_indices()
                .take_while(|(i, _)| *i < max_bytes)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(max_bytes);
            summary.truncate(truncate_at);
            summary.push_str("…");
            tracing::warn!("summary exceeded token budget, truncated");
        }

        tracing::info!(
            dropped = keep_from,
            kept = conv.messages.len() - keep_from,
            summary_tokens = Self::estimate_tokens(&summary),
            summary_budget = summary_budget,
            "compressed conversation memory"
        );

        // Drop old messages and store summary.
        conv.messages = conv.messages.split_off(keep_from);
        conv.summary = Some(summary.clone());

        // Insert summary as a system-level context message at the start.
        conv.messages.insert(
            0,
            Message {
                id: Uuid::new_v4(),
                role: Role::System,
                content: MessageContent::Text(format!(
                    "[Conversation summary from earlier messages]\n{summary}"
                )),
                created_at: Utc::now(),
            },
        );

        Ok(())
    }
}

/// Standalone function so it can be moved into a tokio::spawn.
async fn execute_single_tool(
    call: &ToolCall,
    tools: &[Arc<dyn Tool>],
    sandbox: &Arc<dyn Sandbox>,
    capabilities: &openclaw_core::capability::CapabilitySet,
    session_id: uuid::Uuid,
) -> Result<ToolResult> {
    if !capabilities.can_use_tool(&call.name) {
        tracing::warn!(
            tool = call.name,
            session = %session_id,
            "tool call denied: insufficient capabilities"
        );
        return Err(Error::Auth(format!(
            "session does not have permission to use tool '{}'",
            call.name
        )));
    }

    let tool = tools
        .iter()
        .find(|t| t.name() == call.name)
        .ok_or_else(|| Error::ToolExecution(format!("unknown tool: {}", call.name)))?;

    tracing::info!(tool = call.name, session = %session_id, "executing tool in sandbox");

    let policy = SandboxPolicy {
        allow_net: capabilities.has(&Capability::HttpRequest),
        allow_fs_read: capabilities.has(&Capability::FileRead),
        allow_fs_write: capabilities.has(&Capability::FileWrite),
        allow_spawn: capabilities.has(&Capability::ShellExec),
        ..SandboxPolicy::default()
    };

    sandbox
        .execute(&call.name, call.arguments.clone(), &policy)
        .await?;

    let output = tool.execute(call.arguments.clone()).await?;

    Ok(ToolResult {
        call_id: call.id.clone(),
        output,
    })
}
