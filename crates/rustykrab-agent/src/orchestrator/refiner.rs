//! Self-refinement loop: Generate → Critique → Regenerate.
//!
//! First pass generates an initial response, second pass critiques it
//! for accuracy/completeness/tone, third pass regenerates incorporating
//! the critique. Fast with a 3B-active MoE model — 3 passes complete
//! in seconds.

use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::Result;
use uuid::Uuid;

/// Self-refinement loop that iteratively improves a response.
pub struct RefinementLoop {
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
}

const CRITIQUE_PROMPT: &str = r#"Review this response for:
1. Accuracy — are all facts correct?
2. Completeness — does it fully address the request?
3. Tone — is it appropriate for the context?
4. Clarity — is it well-structured and easy to understand?

If there are NO issues, respond with exactly: APPROVED
If there are issues, list them concisely and suggest specific improvements.

Original request: {request}

Response to review:
{response}"#;

const REFINE_PROMPT: &str = r#"Improve this response based on the critique below.

Original request: {request}

Current response:
{response}

Critique:
{critique}

Write an improved version that addresses all critique points. Respond with ONLY the improved response."#;

impl RefinementLoop {
    pub fn new(provider: Arc<dyn ModelProvider>, config: OrchestrationConfig) -> Self {
        Self { provider, config }
    }

    /// Run the refinement loop on a response.
    ///
    /// Returns the refined response and the number of iterations performed.
    /// Early-exits if the critique finds no issues. The optional `context`
    /// carries agent identity so the critic doesn't flag legitimate tool
    /// outputs (e.g. real Gmail data) as hallucinated.
    pub async fn refine(
        &self,
        original_request: &str,
        initial_response: &str,
        context: Option<&str>,
    ) -> Result<(String, usize)> {
        let mut current = initial_response.to_string();
        let max_iterations = self.config.max_refinement_iterations;

        for iteration in 0..max_iterations {
            // Critique pass.
            let critique = self.critique(original_request, &current, context).await?;

            // Check for approval (no issues found).
            if critique.trim().to_uppercase().contains("APPROVED") {
                tracing::info!(iteration, "refinement approved — no issues found");
                return Ok((current, iteration));
            }

            tracing::info!(iteration, "refining based on critique");

            // Regenerate pass.
            current = self
                .regenerate(original_request, &current, &critique, context)
                .await?;
        }

        tracing::info!(
            iterations = max_iterations,
            "refinement reached max iterations"
        );
        Ok((current, max_iterations))
    }

    /// Run the critique pass.
    async fn critique(&self, request: &str, response: &str, context: Option<&str>) -> Result<String> {
        let prompt = CRITIQUE_PROMPT
            .replace("{request}", &format!("<user_input>\n{request}\n</user_input>"))
            .replace("{response}", &format!("<agent_response>\n{response}\n</agent_response>"));

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

        let result = self.provider.chat(&messages, &[]).await?;
        Ok(result
            .message
            .content
            .as_text()
            .unwrap_or("APPROVED")
            .to_string())
    }

    /// Regenerate the response incorporating the critique.
    async fn regenerate(
        &self,
        request: &str,
        response: &str,
        critique: &str,
        context: Option<&str>,
    ) -> Result<String> {
        let prompt = REFINE_PROMPT
            .replace("{request}", &format!("<user_input>\n{request}\n</user_input>"))
            .replace("{response}", &format!("<agent_response>\n{response}\n</agent_response>"))
            .replace("{critique}", &format!("<agent_response>\n{critique}\n</agent_response>"));

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

        let result = self.provider.chat(&messages, &[]).await?;
        Ok(result
            .message
            .content
            .as_text()
            .unwrap_or(response)
            .to_string())
    }
}
