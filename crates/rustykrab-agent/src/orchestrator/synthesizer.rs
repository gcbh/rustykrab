//! Result synthesis — aggregates sub-task results into a coherent response.
//!
//! After parallel execution completes, the synthesizer feeds condensed
//! results back to the model for a final reasoning pass that produces
//! the user-facing response.

use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::SubTaskResult;
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::Result;
use uuid::Uuid;

/// Synthesizes sub-task results into a final coherent response.
pub struct Synthesizer {
    provider: Arc<dyn ModelProvider>,
}

const SYNTHESIZE_PROMPT: &str = r#"You are synthesizing results from multiple sub-tasks into a single coherent response for the user.

Original request: {request}

Sub-task results:
{results}

Instructions:
- Combine all results into a clear, well-structured response
- Resolve any contradictions between sub-task results
- If any sub-task failed, acknowledge what information is missing
- Do NOT mention the sub-task decomposition process — respond naturally
- Be concise and direct"#;

impl Synthesizer {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self { provider }
    }

    /// Synthesize sub-task results into a final response.
    ///
    /// Takes the original user request and all sub-task results,
    /// then makes a model call to produce a coherent answer.
    /// The optional `context` carries agent identity so the model
    /// doesn't second-guess legitimate tool outputs.
    pub async fn synthesize(
        &self,
        original_request: &str,
        results: &[SubTaskResult],
        context: Option<&str>,
    ) -> Result<String> {
        // If there's only one result and it succeeded, just return it directly.
        // Don't take this fast path when other sub-tasks failed, since the
        // synthesizer should acknowledge incomplete/missing information.
        let has_failures = results.iter().any(|r| !r.success);
        let successful: Vec<&SubTaskResult> = results.iter().filter(|r| r.success).collect();
        if successful.len() == 1 && !has_failures {
            return Ok(successful[0].output.clone());
        }

        // Build the results summary.
        let mut results_text = String::new();
        for (i, result) in results.iter().enumerate() {
            if result.success {
                results_text.push_str(&format!("--- Result {} ---\n{}\n\n", i + 1, result.output));
            } else {
                let err = result.error.as_deref().unwrap_or("unknown error");
                results_text.push_str(&format!(
                    "--- Result {} (FAILED) ---\nError: {}\n\n",
                    i + 1,
                    err
                ));
            }
        }

        let prompt = SYNTHESIZE_PROMPT
            .replace(
                "{request}",
                &format!("<user_input>\n{original_request}\n</user_input>"),
            )
            .replace(
                "{results}",
                &format!("<agent_response>\n{results_text}\n</agent_response>"),
            );

        let mut messages = Vec::new();
        if let Some(ctx) = context {
            messages.push(Message {
                id: Uuid::new_v4(),
                role: Role::System,
                content: MessageContent::Text(ctx.to_string()),
                created_at: Utc::now(),
            });
        }
        messages.push(Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(prompt),
            created_at: Utc::now(),
        });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.provider.chat(&messages, &[]),
        )
        .await
        .map_err(|_| {
            rustykrab_core::Error::Internal("synthesis model call timed out after 120s".into())
        })??;
        Ok(response.message.content.as_text().unwrap_or("").to_string())
    }
}
