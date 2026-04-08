//! Task decomposition — breaks a user request into atomic sub-tasks.
//!
//! The decomposition step is itself a model call. The key insight:
//! no single LLM call should handle a huge context. The orchestrator
//! manages memory and routing — the model gets focused, digestible chunks.

use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::{OrchestrationConfig, SubTask};
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::{Error, Result};
use uuid::Uuid;

/// Decomposes a user request into atomic sub-tasks using a model call.
pub struct Decomposer {
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
}

const DECOMPOSE_PROMPT: &str = r#"You are a task decomposition engine. Break the user's request into atomic sub-tasks that can be executed independently.

Rules:
- Each sub-task should be self-contained and require minimal context
- Identify which sub-tasks can run in parallel (no dependencies between them)
- Identify which sub-tasks depend on others (must run sequentially)
- If a sub-task needs a specific tool, mention it
- Keep each sub-task description concise (1-2 sentences)
- Maximum {max_tasks} sub-tasks

Respond in this exact JSON format (no markdown, no explanation):
{
  "tasks": [
    {
      "description": "what to do",
      "tool_hint": "tool_name or null",
      "depends_on": [],
      "requires_reasoning": true
    }
  ]
}

The "depends_on" field contains indices (0-based) of tasks that must complete first.
"requires_reasoning" is true if the task needs model reasoning, false if it's a pure tool call."#;

impl Decomposer {
    pub fn new(provider: Arc<dyn ModelProvider>, config: OrchestrationConfig) -> Self {
        Self { provider, config }
    }

    /// Decompose a user request into sub-tasks.
    ///
    /// Returns a list of `SubTask`s with dependency information.
    /// If decomposition fails or the request is simple enough,
    /// returns a single sub-task wrapping the original request.
    pub async fn decompose(
        &self,
        user_request: &str,
        context: Option<&str>,
        available_tools: &[&str],
    ) -> Result<Vec<SubTask>> {
        let decompose_instructions = DECOMPOSE_PROMPT.replace(
            "{max_tasks}",
            &self.config.max_sub_tasks.to_string(),
        );

        // Append the concrete list of tool names so the model generates
        // accurate tool_hint values instead of guessing.
        let decompose_instructions = if available_tools.is_empty() {
            decompose_instructions
        } else {
            let tool_list = available_tools.join(", ");
            format!(
                "{decompose_instructions}\n\nAvailable tools (use these exact names for tool_hint): [{tool_list}]"
            )
        };

        // Combine agent system context with decomposition instructions so the
        // model understands the agent's identity, available tools, and
        // permissions when planning sub-tasks.
        let system_prompt = match context {
            Some(ctx) => format!("{ctx}\n\n---\n\n{decompose_instructions}"),
            None => decompose_instructions,
        };

        let messages = vec![
            Message {
                id: Uuid::new_v4(),
                role: Role::System,
                content: MessageContent::Text(system_prompt),
                created_at: Utc::now(),
            },
            Message {
                id: Uuid::new_v4(),
                role: Role::User,
                content: MessageContent::Text(format!(
                    "<user_input>\n{user_request}\n</user_input>"
                )),
                created_at: Utc::now(),
            },
        ];

        let response = self.provider.chat(&messages, &[]).await?;
        let text = response
            .message
            .content
            .as_text()
            .ok_or_else(|| Error::Internal("decomposer returned non-text response".into()))?;

        match self.parse_decomposition(text) {
            Ok(tasks) if !tasks.is_empty() => {
                tracing::info!(task_count = tasks.len(), "decomposed request into sub-tasks");
                Ok(tasks)
            }
            Ok(_) | Err(_) => {
                tracing::debug!("decomposition failed or empty, wrapping as single task");
                Ok(vec![SubTask::new(user_request)])
            }
        }
    }

    /// Parse the model's JSON response into sub-tasks.
    fn parse_decomposition(&self, text: &str) -> Result<Vec<SubTask>> {
        // Try to extract JSON from the response (model might wrap in markdown).
        let json_str = extract_json(text);

        let parsed: DecomposeResponse = serde_json::from_str(json_str)
            .map_err(|e| Error::Internal(format!("failed to parse decomposition: {e}")))?;

        // Build SubTasks, resolving index-based dependencies to UUIDs.
        let mut tasks: Vec<SubTask> = parsed
            .tasks
            .iter()
            .map(|t| {
                let mut st = SubTask::new(&t.description);
                st.requires_reasoning = t.requires_reasoning;
                st.context_budget = self.config.sub_task_context_budget;
                if let Some(ref tool) = t.tool_hint {
                    st = st.with_tool_hint(tool);
                }
                st
            })
            .collect();

        // Resolve index-based dependencies to UUID-based.
        let ids: Vec<Uuid> = tasks.iter().map(|t| t.id).collect();
        for (i, raw_task) in parsed.tasks.iter().enumerate() {
            for &dep_idx in &raw_task.depends_on {
                if let Some(&dep_id) = ids.get(dep_idx) {
                    tasks[i].depends_on.push(dep_id);
                }
            }
        }

        // Enforce max sub-tasks.
        tasks.truncate(self.config.max_sub_tasks);

        Ok(tasks)
    }
}

/// Extract JSON from text that might be wrapped in markdown code blocks.
fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();
    // Try to find JSON block in markdown.
    if let Some(start) = trimmed.find("```json") {
        let after_marker = &trimmed[start + 7..];
        if let Some(end) = after_marker.find("```") {
            return after_marker[..end].trim();
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after_marker = &trimmed[start + 3..];
        if let Some(end) = after_marker.find("```") {
            return after_marker[..end].trim();
        }
    }
    // Try to find raw JSON object.
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            return &trimmed[start..=end];
        }
    }
    trimmed
}

#[derive(serde::Deserialize)]
struct DecomposeResponse {
    tasks: Vec<RawTask>,
}

#[derive(serde::Deserialize)]
struct RawTask {
    description: String,
    #[serde(default)]
    tool_hint: Option<String>,
    #[serde(default)]
    depends_on: Vec<usize>,
    #[serde(default = "default_true")]
    requires_reasoning: bool,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_raw() {
        let input = r#"{"tasks": [{"description": "test"}]}"#;
        assert!(extract_json(input).contains("tasks"));
    }

    #[test]
    fn test_extract_json_markdown() {
        let input = "Here's the decomposition:\n```json\n{\"tasks\": []}\n```\nDone.";
        assert_eq!(extract_json(input), "{\"tasks\": []}");
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let input = "Sure! {\"tasks\": [{\"description\": \"do it\"}]} That's my plan.";
        let json = extract_json(input);
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
    }
}
