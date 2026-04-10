//! Parallel execution engine for sub-tasks.
//!
//! Fans out independent sub-tasks concurrently via Tokio, respecting
//! dependency ordering. Each sub-task gets its own focused context
//! window — no single model call handles more than ~8K tokens.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::{OrchestrationConfig, SubTask, SubTaskResult};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolResult, ToolSchema};
use rustykrab_core::{Result, Tool};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::sandbox::{Sandbox, SandboxPolicy};

/// Executes sub-tasks in parallel, respecting dependency ordering.
pub struct ParallelExecutor {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    config: OrchestrationConfig,
}

impl ParallelExecutor {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        sandbox: Arc<dyn Sandbox>,
        config: OrchestrationConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            sandbox,
            config,
        }
    }

    /// Execute all sub-tasks, respecting dependencies.
    ///
    /// Tasks with no dependencies run concurrently. Tasks with
    /// dependencies wait for their prerequisites to complete, then
    /// receive the prerequisite results as context.
    ///
    /// The optional `system_context` carries the agent's identity, tool
    /// permissions, and security policies so that sub-task model calls
    /// understand they are operating within an authorized agent.
    pub async fn execute(
        &self,
        tasks: &[SubTask],
        system_context: Option<&str>,
    ) -> Vec<SubTaskResult> {
        let mut results: HashMap<Uuid, SubTaskResult> = HashMap::new();
        let mut completed: HashSet<Uuid> = HashSet::new();
        let all_ids: HashSet<Uuid> = tasks.iter().map(|t| t.id).collect();

        // Process in waves: each wave contains tasks whose deps are all satisfied.
        loop {
            let ready: Vec<&SubTask> = tasks
                .iter()
                .filter(|t| {
                    !completed.contains(&t.id)
                        && t.depends_on
                            .iter()
                            .all(|dep| completed.contains(dep) || !all_ids.contains(dep))
                })
                .collect();

            if ready.is_empty() {
                break;
            }

            tracing::info!(wave_size = ready.len(), "executing sub-task wave");

            // Execute all ready tasks concurrently, bounded by a semaphore
            // to prevent pathological workloads from spawning unbounded
            // concurrent LLM calls.
            let semaphore = Arc::new(Semaphore::new(self.config.max_concurrent_tasks));
            let mut handles: Vec<(Uuid, _)> = Vec::new();
            for task in &ready {
                // Gather dependency results as context.
                let dep_context: Vec<String> = task
                    .depends_on
                    .iter()
                    .filter_map(|dep_id| results.get(dep_id))
                    .filter(|r| r.success)
                    .map(|r| r.output.clone())
                    .collect();

                let task_id = task.id;
                let task = (*task).clone();
                let provider = self.provider.clone();
                let tools = self.tools.clone();
                let sandbox = self.sandbox.clone();
                let config = self.config.clone();
                let sys_ctx = system_context.map(|s| s.to_string());
                let sem = semaphore.clone();

                handles.push((
                    task_id,
                    tokio::spawn(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");
                        execute_sub_task(
                            &task,
                            &dep_context,
                            sys_ctx.as_deref(),
                            &provider,
                            &tools,
                            &sandbox,
                            &config,
                        )
                        .await
                    }),
                ));
            }

            for (task_id, handle) in handles {
                match handle.await {
                    Ok(result) => {
                        completed.insert(result.task_id);
                        results.insert(result.task_id, result);
                    }
                    Err(e) => {
                        tracing::error!(task_id = %task_id, "sub-task panicked: {e}");
                        // Insert a failure result so downstream consumers know the
                        // task failed rather than silently receiving incomplete results.
                        completed.insert(task_id);
                        results.insert(
                            task_id,
                            SubTaskResult {
                                task_id,
                                output: String::new(),
                                success: false,
                                error: Some(format!("task panicked: {e}")),
                                tokens_used: 0,
                            },
                        );
                    }
                }
            }
        }

        // Return results in original task order.
        tasks.iter().filter_map(|t| results.remove(&t.id)).collect()
    }
}

/// Execute a single sub-task with its own focused context.
async fn execute_sub_task(
    task: &SubTask,
    dep_context: &[String],
    system_context: Option<&str>,
    provider: &Arc<dyn ModelProvider>,
    tools: &[Arc<dyn Tool>],
    sandbox: &Arc<dyn Sandbox>,
    config: &OrchestrationConfig,
) -> SubTaskResult {
    // Build focused context for this sub-task.
    let mut messages = Vec::new();

    // System context: agent identity/permissions + dependency results.
    let mut system_parts = Vec::new();
    if let Some(sys_ctx) = system_context {
        system_parts.push(sys_ctx.to_string());
    }
    if !dep_context.is_empty() {
        let ctx = dep_context.join("\n\n---\n\n");
        system_parts.push(format!("Context from prerequisite tasks:\n{ctx}"));
    }
    if !system_parts.is_empty() {
        messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(system_parts.join("\n\n---\n\n")),
            created_at: Utc::now(),
        });
    }

    // Frame the sub-task as a direct action instruction, not a topic to
    // discuss. Without this framing the model tends to describe how to do
    // the task or investigate tool availability rather than executing it.
    let instruction = format!(
        "Execute the following task now using your tools. Do NOT explain how to do it — \
         call the appropriate tool(s) and return the results.\n\nTask: {}",
        task.description
    );
    messages.push(Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(instruction),
        created_at: Utc::now(),
    });

    // Get available tool schemas.
    let schemas: Vec<ToolSchema> = if task.requires_reasoning {
        tools.iter().map(|t| t.schema()).collect()
    } else if let Some(ref hint) = task.tool_hint {
        // Only expose the hinted tool for focused tool calls.
        tools
            .iter()
            .filter(|t| t.name() == hint.as_str())
            .map(|t| t.schema())
            .collect()
    } else {
        tools.iter().map(|t| t.schema()).collect()
    };

    // Run the model, handling multiple tool-call rounds per sub-task.
    let max_rounds = config.max_tool_rounds;
    for _round in 0..max_rounds {
        let response = match provider.chat(&messages, &schemas).await {
            Ok(r) => r,
            Err(e) => {
                return SubTaskResult {
                    task_id: task.id,
                    output: String::new(),
                    success: false,
                    error: Some(e.to_string()),
                    tokens_used: 0,
                };
            }
        };

        let tokens = response.usage.prompt_tokens + response.usage.completion_tokens;

        // If the model wants to use tools, execute them and continue.
        if response.message.content.has_tool_calls() {
            messages.push(response.message.clone());

            let calls = response.message.content.tool_calls();
            for call in calls {
                let result =
                    execute_tool_for_subtask(call, tools, sandbox, config.max_tool_retries).await;
                messages.push(Message {
                    id: Uuid::new_v4(),
                    role: Role::Tool,
                    content: MessageContent::ToolResult(result),
                    created_at: Utc::now(),
                });
            }
            continue;
        }

        // Text response — sub-task complete.
        let output = response.message.content.as_text().unwrap_or("").to_string();

        // Summarize if output is too long.
        let output = if config.summarize_sub_results && output.len() > 2000 {
            summarize_output(provider, &output).await.unwrap_or(output)
        } else {
            output
        };

        return SubTaskResult {
            task_id: task.id,
            output,
            success: true,
            error: None,
            tokens_used: tokens as usize,
        };
    }

    SubTaskResult {
        task_id: task.id,
        output: String::new(),
        success: false,
        error: Some("exceeded max tool-call rounds for sub-task".into()),
        tokens_used: 0,
    }
}

/// Execute a tool call within a sub-task, retrying transient failures.
async fn execute_tool_for_subtask(
    call: &ToolCall,
    tools: &[Arc<dyn Tool>],
    sandbox: &Arc<dyn Sandbox>,
    max_retries: u32,
) -> ToolResult {
    let tool = match tools.iter().find(|t| t.name() == call.name) {
        Some(t) => t,
        None => {
            return ToolResult {
                call_id: call.id.clone(),
                output: serde_json::json!({ "error": format!("unknown tool: {}", call.name) }),
                is_error: true,
            };
        }
    };

    // Sub-tasks need the same capabilities as the main agent loop —
    // writing files (e.g. saving downloaded documents) and spawning
    // processes (e.g. pip install) are required for real task completion.
    let policy = SandboxPolicy {
        allow_net: true,
        allow_fs_read: true,
        allow_fs_write: true,
        allow_spawn: true,
        ..SandboxPolicy::default()
    };

    // Enforce sandbox check — fail the tool call if denied.
    if let Err(e) = sandbox
        .execute(&call.name, call.arguments.clone(), &policy)
        .await
    {
        return ToolResult {
            call_id: call.id.clone(),
            output: serde_json::json!({ "error": format!("sandbox denied tool '{}': {e}", call.name) }),
            is_error: true,
        };
    }

    let mut last_err = None;
    for attempt in 0..=max_retries {
        match tool.execute(call.arguments.clone()).await {
            Ok(output) => {
                return ToolResult {
                    call_id: call.id.clone(),
                    output,
                    is_error: false,
                };
            }
            Err(e) => {
                tracing::warn!(
                    tool = call.name,
                    attempt = attempt + 1,
                    max_retries,
                    error = %e,
                    "sub-task tool call failed, retrying"
                );
                last_err = Some(e);
                if attempt < max_retries {
                    let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    ToolResult {
        call_id: call.id.clone(),
        output: serde_json::json!({ "error": last_err.unwrap_or_else(|| rustykrab_core::Error::ToolExecution("all retries exhausted".into())).to_string() }),
        is_error: true,
    }
}

/// Summarize a long output to stay within context budgets.
async fn summarize_output(provider: &Arc<dyn ModelProvider>, output: &str) -> Result<String> {
    let messages = vec![Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(format!(
            "Summarize this concisely, preserving all key facts and data:\n\n{output}"
        )),
        created_at: Utc::now(),
    }];

    let response = provider.chat(&messages, &[]).await?;
    Ok(response
        .message
        .content
        .as_text()
        .unwrap_or(output)
        .to_string())
}
