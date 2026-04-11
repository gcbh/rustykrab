//! REPL-style recursive call execution.
//!
//! Following the foundational RLM paper (Zhang, Kraska, Khattab —
//! arXiv 2512.24601), the context is stored **outside** the prompt as
//! an external variable.  The model explores it via tools:
//!
//! - `context_info`  — get metadata (length, tokens, preview)
//! - `context_peek`  — view a slice by character position
//! - `context_search` — regex search for patterns
//! - `sub_query`     — launch a recursive sub-call on a context slice
//!
//! This replaces the previous approach where context was truncated into
//! the prompt and the model emitted `[SUB_CALL: ...]` text markers.

use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::{Message, MessageContent, Role, ToolResult};
use rustykrab_core::{Error, Result};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::context_manager::estimate_tokens;
use super::repl_tools;

/// Executes recursive queries where the model explores context via tools.
pub struct RecursiveExecutor {
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
}

impl RecursiveExecutor {
    pub fn new(provider: Arc<dyn ModelProvider>, config: OrchestrationConfig) -> Self {
        Self { provider, config }
    }

    /// Execute a recursive query, allowing the model to explore context
    /// via REPL tools and delegate sub-queries on specific slices.
    ///
    /// The entire execution tree is bounded by `pipeline_timeout_secs`
    /// to prevent runaway recursion from consuming unbounded wall time.
    pub async fn execute(&self, prompt: &str, context: Option<&str>) -> Result<String> {
        let pipeline_timeout = self.config.pipeline_timeout_secs;
        tracing::info!(
            max_depth = self.config.max_recursion_depth,
            max_tool_rounds = self.config.max_tool_rounds,
            pipeline_timeout_secs = pipeline_timeout,
            prompt_len = prompt.len(),
            context_len = context.map(|c| c.len()).unwrap_or(0),
            "RLM REPL: starting recursive execution"
        );

        let start = std::time::Instant::now();
        let context_arc = Arc::new(context.unwrap_or_default().to_string());
        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrent_tasks));

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(pipeline_timeout),
            execute_repl_call(
                self.provider.clone(),
                self.config.clone(),
                prompt.to_string(),
                context_arc,
                0,
                semaphore,
            ),
        )
        .await;

        let elapsed = start.elapsed();
        match result {
            Ok(Ok(ref text)) => tracing::info!(
                duration_ms = elapsed.as_millis() as u64,
                response_len = text.len(),
                "RLM REPL: recursive execution completed"
            ),
            Ok(Err(ref e)) => tracing::error!(
                duration_ms = elapsed.as_millis() as u64,
                error = %e,
                "RLM REPL: recursive execution failed"
            ),
            Err(_) => tracing::error!(
                duration_ms = elapsed.as_millis() as u64,
                pipeline_timeout_secs = pipeline_timeout,
                "RLM REPL: pipeline timeout exceeded"
            ),
        }

        match result {
            Ok(inner) => inner,
            Err(_) => Err(Error::Internal(format!(
                "RLM pipeline timed out after {pipeline_timeout}s"
            ))),
        }
    }
}

/// Execute a single REPL call at the given depth.
///
/// Public within the crate so that [`repl_tools::SubQueryTool`] can
/// call it recursively.
pub(crate) fn execute_repl_call(
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    prompt: String,
    context: Arc<String>,
    depth: usize,
    semaphore: Arc<Semaphore>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> {
    Box::pin(execute_repl_call_impl(
        provider, config, prompt, context, depth, semaphore,
    ))
}

async fn execute_repl_call_impl(
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    prompt: String,
    context: Arc<String>,
    depth: usize,
    semaphore: Arc<Semaphore>,
) -> Result<String> {
    tracing::info!(
        depth,
        context_chars = context.len(),
        prompt_preview = &prompt[..prompt.len().min(100)],
        "RLM REPL: executing call"
    );

    // At max depth, answer directly with the context in the prompt.
    // The context at this level is a small, model-selected slice.
    if depth >= config.max_recursion_depth {
        tracing::warn!(
            depth,
            "RLM REPL: max recursion depth reached, answering directly"
        );
        return direct_call(&provider, &prompt, &context).await;
    }

    // Build REPL tools for this depth level.
    let tools = repl_tools::repl_tools(
        context.clone(),
        provider.clone(),
        config.clone(),
        depth,
        semaphore.clone(),
    );
    let schemas: Vec<_> = tools.iter().map(|t| t.schema()).collect();

    // System prompt — tells the model about the context variable and
    // available tools. Context text is NOT in the prompt.
    let estimated_tokens = estimate_tokens(&context);
    let system = format!(
        "You are a reasoning engine. Answer the question below.\n\n\
         You have access to a context variable ({} bytes, ~{} tokens, {} lines).\n\
         All positions in tools use byte offsets (snapped to UTF-8 boundaries).\n\
         Use the provided tools to examine, search, and query the context.\n\
         Do NOT try to answer from memory — always consult the context.\n\n\
         Strategy:\n\
         1. Start with context_info to understand the context structure.\n\
         2. Use context_search to find relevant sections.\n\
         3. Use context_peek to read specific sections in detail.\n\
         4. Use sub_query to delegate focused questions on specific \
            sections if the context is too large to process at once.\n\n\
         Be efficient with tool calls. Batch information into larger \
         peeks rather than many small ones.\n\
         Current depth: {}/{}",
        context.len(),
        estimated_tokens,
        context.lines().count(),
        depth,
        config.max_recursion_depth,
    );

    let mut messages = vec![
        Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(system),
            created_at: Utc::now(),
        },
        Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(prompt),
            created_at: Utc::now(),
        },
    ];

    // Tool-use loop — mirrors the pattern in orchestrator/executor.rs.
    let max_rounds = config.max_tool_rounds;
    let timeout_secs = config.model_call_timeout_secs;

    for round in 0..max_rounds {
        // Acquire a semaphore permit for the LLM call only — released
        // before tool execution so recursive sub_query calls can
        // acquire their own permits without deadlocking.
        let response = {
            let _permit = semaphore
                .acquire()
                .await
                .map_err(|_| Error::Internal("RLM semaphore closed".into()))?;
            match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                provider.chat(&messages, &schemas),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::error!(depth, round, error = %e, "RLM REPL: model call failed");
                    return Err(e);
                }
                Err(_) => {
                    tracing::error!(depth, round, "RLM REPL: model call timed out");
                    return Err(Error::Internal(format!(
                        "RLM model call timed out after {timeout_secs}s"
                    )));
                }
            }
        };

        // If the model wants to use tools, execute them and continue.
        if response.message.content.has_tool_calls() {
            messages.push(response.message.clone());

            let calls = response.message.content.tool_calls();
            tracing::debug!(
                depth,
                round,
                tool_calls = calls.len(),
                tools = ?calls.iter().map(|c| &c.name).collect::<Vec<_>>(),
                "RLM REPL: executing tool calls"
            );

            // Execute tool calls concurrently within a round so
            // parallel sub_queries don't block each other.
            let futs: Vec<_> = calls
                .into_iter()
                .map(|call| {
                    let call = call.clone();
                    let tools = tools.clone();
                    async move { execute_repl_tool(&call, &tools).await }
                })
                .collect();
            let results = futures::future::join_all(futs).await;
            for result in results {
                messages.push(Message {
                    id: Uuid::new_v4(),
                    role: Role::Tool,
                    content: MessageContent::ToolResult(result),
                    created_at: Utc::now(),
                });
            }
            continue;
        }

        // Text response — call is complete.
        let answer = response.message.content.as_text().unwrap_or("").to_string();
        tracing::info!(
            depth,
            rounds_used = round + 1,
            answer_len = answer.len(),
            "RLM REPL: call completed"
        );
        return Ok(answer);
    }

    tracing::error!(depth, max_rounds, "RLM REPL: exceeded max tool rounds");
    Err(Error::Internal("RLM exceeded max tool-call rounds".into()))
}

/// Execute a single REPL tool call by name.
async fn execute_repl_tool(
    call: &rustykrab_core::types::ToolCall,
    tools: &[Arc<dyn Tool>],
) -> ToolResult {
    let tool = match tools.iter().find(|t| t.name() == call.name.as_str()) {
        Some(t) => t,
        None => {
            return ToolResult {
                call_id: call.id.clone(),
                output: serde_json::json!({
                    "error": format!("unknown tool: {}", call.name)
                }),
                is_error: true,
            };
        }
    };

    match tool.execute(call.arguments.clone()).await {
        Ok(output) => ToolResult {
            call_id: call.id.clone(),
            output,
            is_error: false,
        },
        Err(e) => ToolResult {
            call_id: call.id.clone(),
            output: serde_json::json!({ "error": e.to_string() }),
            is_error: true,
        },
    }
}

/// Direct model call at max depth — context goes into the prompt since
/// the model has no tools to explore it. At this level the context is a
/// small, model-selected slice (not the original full blob).
async fn direct_call(
    provider: &Arc<dyn ModelProvider>,
    prompt: &str,
    context: &str,
) -> Result<String> {
    let mut messages = Vec::new();
    if !context.is_empty() {
        messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(format!("Context:\n{context}")),
            created_at: Utc::now(),
        });
    }
    messages.push(Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(prompt.to_string()),
        created_at: Utc::now(),
    });

    let response = provider.chat(&messages, &[]).await?;
    Ok(response.message.content.as_text().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use rustykrab_core::model::{ModelResponse, StopReason, StreamEvent, Usage};
    use rustykrab_core::types::ToolSchema;
    use serde_json::Value;
    use std::sync::Mutex;

    /// A mock provider that returns canned responses.
    struct MockProvider {
        responses: Mutex<Vec<ModelResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }

        fn text_response(text: &str) -> ModelResponse {
            ModelResponse {
                message: Message {
                    id: Uuid::new_v4(),
                    role: Role::Assistant,
                    content: MessageContent::Text(text.to_string()),
                    created_at: Utc::now(),
                },
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 10,
                    ..Default::default()
                },
                stop_reason: StopReason::EndTurn,
                text: Some(text.to_string()),
            }
        }

        fn tool_call_response(tool_name: &str, args: Value) -> ModelResponse {
            use rustykrab_core::types::ToolCall;
            ModelResponse {
                message: Message {
                    id: Uuid::new_v4(),
                    role: Role::Assistant,
                    content: MessageContent::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: tool_name.to_string(),
                        arguments: args,
                    }),
                    created_at: Utc::now(),
                },
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 10,
                    ..Default::default()
                },
                stop_reason: StopReason::ToolUse,
                text: None,
            }
        }
    }

    #[async_trait]
    impl ModelProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> Result<ModelResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(Self::text_response("(no more responses)"))
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolSchema],
            _on_event: &(dyn Fn(StreamEvent) + Send + Sync),
        ) -> Result<ModelResponse> {
            self.chat(messages, tools).await
        }
    }

    #[tokio::test]
    async fn test_direct_answer_no_tools() {
        // Model immediately returns a text answer.
        let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
            "The answer is 42",
        )]));
        let config = OrchestrationConfig::default();
        let executor = RecursiveExecutor::new(provider, config);

        let result = executor
            .execute("What is the answer?", Some("some context"))
            .await
            .unwrap();
        assert_eq!(result, "The answer is 42");
    }

    #[tokio::test]
    async fn test_tool_use_then_answer() {
        // Model first calls context_info, then answers.
        let provider = Arc::new(MockProvider::new(vec![
            MockProvider::tool_call_response("context_info", serde_json::json!({})),
            MockProvider::text_response("The context has 11 characters"),
        ]));
        let config = OrchestrationConfig::default();
        let executor = RecursiveExecutor::new(provider, config);

        let result = executor
            .execute("How long is the context?", Some("hello world"))
            .await
            .unwrap();
        assert_eq!(result, "The context has 11 characters");
    }

    #[tokio::test]
    async fn test_peek_then_answer() {
        let provider = Arc::new(MockProvider::new(vec![
            MockProvider::tool_call_response(
                "context_peek",
                serde_json::json!({"start": 0, "end": 5}),
            ),
            MockProvider::text_response("The first word is Hello"),
        ]));
        let config = OrchestrationConfig::default();
        let executor = RecursiveExecutor::new(provider, config);

        let result = executor
            .execute("What is the first word?", Some("Hello, world!"))
            .await
            .unwrap();
        assert_eq!(result, "The first word is Hello");
    }

    #[tokio::test]
    async fn test_max_depth_direct_call() {
        // At max depth, context goes directly into the prompt.
        let provider = Arc::new(MockProvider::new(vec![MockProvider::text_response(
            "Direct answer",
        )]));
        let config = OrchestrationConfig {
            max_recursion_depth: 0,
            ..Default::default()
        };
        let executor = RecursiveExecutor::new(provider, config);

        let result = executor.execute("question", Some("ctx")).await.unwrap();
        assert_eq!(result, "Direct answer");
    }
}
