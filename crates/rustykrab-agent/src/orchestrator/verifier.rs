//! Self-consistency voting for high-stakes decisions.
//!
//! Runs the same query multiple times with temperature variation,
//! compares outputs for consistency, and either takes the majority
//! answer or flags inconsistencies for human review.

use std::sync::Arc;

use chrono::Utc;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::{OrchestrationConfig, VoteResult, VotingStrategy};
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::Result;
use uuid::Uuid;

/// Self-consistency voter that runs multiple samples and compares.
pub struct ConsistencyVoter {
    /// Multiple providers with different temperatures, or a single
    /// provider that we call multiple times.
    provider: Arc<dyn ModelProvider>,
    config: OrchestrationConfig,
    strategy: VotingStrategy,
}

const COMPARE_PROMPT: &str = r#"Compare these responses to the same question and determine the consensus answer.

Question: {question}

Responses:
{responses}

Instructions:
1. Identify the most common answer/recommendation across all responses
2. If all responses agree, return the best-stated version
3. If responses disagree, return the majority position
4. Note any significant disagreements

Respond with ONLY the consensus answer (no meta-commentary about the comparison process)."#;

impl ConsistencyVoter {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        config: OrchestrationConfig,
        strategy: VotingStrategy,
    ) -> Self {
        Self {
            provider,
            config,
            strategy,
        }
    }

    /// Run self-consistency voting on a query.
    ///
    /// Executes the query `consistency_samples` times, then compares
    /// results using the configured voting strategy.
    pub async fn vote(&self, query: &str, context: Option<&str>) -> Result<VoteResult> {
        let num_samples = self.config.consistency_samples;
        tracing::info!(num_samples, "running self-consistency voting");

        // Run all samples concurrently.
        let mut handles = Vec::with_capacity(num_samples);
        for i in 0..num_samples {
            let provider = self.provider.clone();
            let query = query.to_string();
            let context = context.map(|s| s.to_string());

            handles.push(tokio::spawn(async move {
                let mut messages = Vec::new();
                if let Some(ctx) = context {
                    messages.push(Message {
                        id: Uuid::new_v4(),
                        role: Role::System,
                        content: MessageContent::Text(ctx),
                        created_at: Utc::now(),
                    });
                }
                messages.push(Message {
                    id: Uuid::new_v4(),
                    role: Role::User,
                    content: MessageContent::Text(query),
                    created_at: Utc::now(),
                });

                let result = provider.chat(&messages, &[]).await;
                tracing::debug!(sample = i, "consistency sample completed");
                result
            }));
        }

        // Collect responses.
        let mut responses = Vec::with_capacity(num_samples);
        for handle in handles {
            match handle.await {
                Ok(Ok(response)) => {
                    if let Some(text) = response.message.content.as_text() {
                        responses.push(text.to_string());
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("consistency sample failed: {e}");
                }
                Err(e) => {
                    tracing::warn!("consistency sample panicked: {e}");
                }
            }
        }

        if responses.is_empty() {
            return Err(rustykrab_core::Error::Internal(
                "all consistency samples failed".into(),
            ));
        }

        // If only one response, return it directly.
        if responses.len() == 1 {
            return Ok(VoteResult {
                answer: responses[0].clone(),
                agreement_count: 1,
                total_samples: 1,
                unanimous: true,
                responses,
                confidence: 1.0,
            });
        }

        // Use the model to compare and find consensus.
        let consensus = self.find_consensus(query, &responses).await?;
        let confidence = consensus.agreement_count as f64 / consensus.total_samples as f64;

        // Apply voting strategy.
        match self.strategy {
            VotingStrategy::Majority => Ok(consensus),
            VotingStrategy::UnanimousOrEscalate => {
                if consensus.unanimous {
                    Ok(consensus)
                } else {
                    // Return the result but with low confidence to signal escalation.
                    Ok(VoteResult {
                        confidence: confidence * 0.5, // Penalize non-unanimous
                        ..consensus
                    })
                }
            }
        }
    }

    /// Use the model to find consensus among responses.
    async fn find_consensus(
        &self,
        question: &str,
        responses: &[String],
    ) -> Result<VoteResult> {
        let mut responses_text = String::new();
        for (i, resp) in responses.iter().enumerate() {
            responses_text.push_str(&format!("--- Response {} ---\n{}\n\n", i + 1, resp));
        }

        let prompt = COMPARE_PROMPT
            .replace("{question}", question)
            .replace("{responses}", &responses_text);

        let messages = vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(prompt),
            created_at: Utc::now(),
        }];

        let result = self.provider.chat(&messages, &[]).await?;
        let answer = result
            .message
            .content
            .as_text()
            .unwrap_or("")
            .to_string();

        // Estimate agreement by simple similarity check.
        let agreement_count = responses
            .iter()
            .filter(|r| {
                // Rough similarity: check if key phrases overlap.
                let r_lower = r.to_lowercase();
                let a_lower = answer.to_lowercase();
                // Consider "agreeing" if they share significant word overlap.
                let answer_words: std::collections::HashSet<&str> =
                    a_lower.split_whitespace().collect();
                let response_words: std::collections::HashSet<&str> =
                    r_lower.split_whitespace().collect();
                let overlap = answer_words.intersection(&response_words).count();
                let total = answer_words.len().max(1);
                (overlap as f64 / total as f64) > 0.3
            })
            .count();

        let total = responses.len();
        let unanimous = agreement_count == total;
        let confidence = agreement_count as f64 / total as f64;

        Ok(VoteResult {
            answer,
            agreement_count,
            total_samples: total,
            unanimous,
            responses: responses.to_vec(),
            confidence,
        })
    }
}
