//! Parallel execution engine for sub-tasks.
//!
//! Fans out independent sub-tasks concurrently via Tokio, respecting
//! dependency ordering. Each sub-task gets its own focused context
//! window — no single model call handles more than ~8K tokens.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use openclaw_core::model::ModelProvider;
use openclaw_core::orchestration::{OrchestrationConfig, SubTask, SubTaskResult};
use openclaw_core::types::{
    Message, MessageContent, Role, ToolCall, ToolResult, ToolSchema,
};
use openclaw_core::{Result, Tool};
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
    pub async fn execute(&self, tasks: &[SubTask]) -> Vec<SubTaskResult> {
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

            // Execute all ready tasks concurrently.
            let mut handles = Vec::new();
            for task in &ready {
                // Gather dependency results as context.
                let dep_context: Vec<String> = task
                    .depends_on
                    .iter()
                    .filter_map(|dep_id| results.get(dep_id))
                    .filter(|r| r.success)
                    .map(|r| r.output.clone())
                    .collect();

                let task = (*task).clone();
                let provider = self.provider.clone();
                let tools = self.tools.clone();
                let sandbox = self.sandbox.clone();
                let config = self.config.clone();

                handles.push(tokio::spawn(async move {
                    execute_sub_task(&task, &dep_context, &provider, &tools, &sandbox, &config)
                        .await
                }));
            }

            for handle in handles {
                match handle.await {
                    Ok(result) => {
                        completed.insert(result.task_id);
                        results.insert(result.task_id, result);
                    }
                    Err(e) => {
                        tracing::error!("sub-task panicked: {e}");
                    }
                }
            }
        }

        // Return results in original task order.
        tasks
            .iter()
            .filter_map(|t| results.remove(&t.id))
            .collect()
    }
}

/// Execute a single sub-task with its own focused context.
async fn execute_sub_task(
    task: &SubTask,
    dep_context: &[String],
    provider: &Arc<dyn ModelProvider>,
    tools: &[Arc<dyn Tool>],
    sandbox: &Arc<dyn Sandbox>,
    config: &OrchestrationConfig,
) -> SubTaskResult {
    // Build focused context for this sub-task.
    let mut messages = Vec::new();

    // System context with dependency results.
    if !dep_context.is_empty() {
        let ctx = dep_context.join("\n\n---\n\n");
        messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::System,
            content: MessageContent::Text(format!(
                "Context from prerequisite tasks:\n{ctx}"
            )),
            created_at: Utc::now(),
        });
    }

    messages.push(Message {
        id: Uuid::new_v4(),
        role: Role::User,
        content: MessageContent::Text(task.description.clone()),
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

    // Run the model, handling up to 3 tool-call rounds per sub-task.
    let max_rounds = 3;
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
                let result = execute_tool_for_subtask(call, tools, sandbox).await;
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
        let output = response
            .message
            .content
            .as_text()
            .unwrap_or("")
            .to_string();

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

/// Execute a tool call within a sub-task (simplified, no session caps).
async fn execute_tool_for_subtask(
    call: &ToolCall,
    tools: &[Arc<dyn Tool>],
    sandbox: &Arc<dyn Sandbox>,
) -> ToolResult {
    let tool = tools.iter().find(|t| t.name() == call.name);
    match tool {
        Some(t) => {
            // Use a restricted policy instead of trusted() to limit orchestrator sub-tasks
            let policy = SandboxPolicy {
                allow_net: true,
                allow_fs_read: true,
                allow_fs_write: false,
                allow_spawn: false,
                ..SandboxPolicy::default()
            };
            if let Err(e) = sandbox.execute(&call.name, call.arguments.clone(), &policy).await {
                return ToolResult {
                    call_id: call.id.clone(),
                    output: serde_json::json!({ "error": format!("sandbox denied tool '{}': {e}", call.name) }),
                };
            }
            match t.execute(call.arguments.clone()).await {
                Ok(output) => ToolResult {
                    call_id: call.id.clone(),
                    output,
                },
                Err(e) => ToolResult {
                    call_id: call.id.clone(),
                    output: serde_json::json!({ "error": e.to_string() }),
                },
            }
        }
        None => ToolResult {
            call_id: call.id.clone(),
            output: serde_json::json!({ "error": format!("unknown tool: {}", call.name) }),
        },
    }
}

/// Summarize a long output to stay within context budgets.
async fn summarize_output(
    provider: &Arc<dyn ModelProvider>,
    output: &str,
) -> Result<String> {
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
