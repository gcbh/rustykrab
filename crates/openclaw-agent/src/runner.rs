use std::sync::Arc;

use chrono::Utc;
use openclaw_core::capability::Capability;
use openclaw_core::model::{ModelProvider, ModelResponse, StopReason};
use openclaw_core::session::Session;
use openclaw_core::types::{
    Conversation, Message, MessageContent, Role, ToolCall, ToolResult, ToolSchema,
};
use openclaw_core::{Error, Result, Tool};
use uuid::Uuid;

use crate::sandbox::{Sandbox, SandboxPolicy};

/// Configuration for the agent runner.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum iterations before giving up.
    pub max_iterations: usize,
    /// Maximum consecutive errors before reflecting.
    pub max_consecutive_errors: usize,
    /// Maximum retries per failed tool call.
    pub max_tool_retries: u32,
    /// Number of messages to keep in full before summarizing.
    pub memory_window: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            max_consecutive_errors: 3,
            max_tool_retries: 2,
            memory_window: 40,
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
        }
    }

    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
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
            // Compress memory if conversation is getting long.
            if conv.messages.len() > self.config.memory_window {
                self.compress_memory(conv).await?;
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

                // Execute all tool calls in parallel.
                let results = self
                    .execute_tools_parallel(calls, session)
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

    /// Execute multiple tool calls in parallel using tokio::JoinSet.
    async fn execute_tools_parallel(
        &self,
        calls: Vec<&ToolCall>,
        session: &Session,
    ) -> Vec<Result<ToolResult>> {
        if calls.len() == 1 {
            // Single call — no need for JoinSet overhead.
            let result = self.execute_tool_checked(calls[0], session).await;
            return vec![result];
        }

        // Spawn all tool executions concurrently.
        let mut handles = Vec::with_capacity(calls.len());

        for call in calls {
            let call = call.clone();
            let tools = self.tools.clone();
            let sandbox = self.sandbox.clone();
            let session_caps = session.capabilities.clone();
            let session_id = session.id;

            handles.push(tokio::spawn(async move {
                execute_single_tool(&call, &tools, &sandbox, &session_caps, session_id).await
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(Err(Error::Internal(format!("task panicked: {e}")))),
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

    /// Compress older messages into a summary to stay within context limits.
    ///
    /// Keeps the system prompt, the summary, and the most recent N messages.
    /// Asks the model to produce the summary from the messages being dropped.
    async fn compress_memory(&self, conv: &mut Conversation) -> Result<()> {
        let keep = self.config.memory_window / 2;
        if conv.messages.len() <= keep {
            return Ok(());
        }

        let to_summarize = conv.messages.len() - keep;
        let old_messages = &conv.messages[..to_summarize];

        // Build a summary prompt from the old messages.
        let mut summary_text = String::new();
        for msg in old_messages {
            let role = match msg.role {
                Role::System => continue, // Keep system prompts.
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

        // Ask the model to summarize.
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
                 key facts, decisions, and any information that would be needed to \
                 continue the conversation:\n\n{summary_text}"
            )),
            created_at: Utc::now(),
        }];

        let response = self.provider.chat(&summary_prompt, &[]).await?;
        let summary = response
            .message
            .content
            .as_text()
            .unwrap_or("(summary failed)")
            .to_string();

        tracing::info!(
            dropped = to_summarize,
            kept = keep,
            "compressed conversation memory"
        );

        // Remove old messages and store summary.
        conv.messages = conv.messages.split_off(to_summarize);
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
