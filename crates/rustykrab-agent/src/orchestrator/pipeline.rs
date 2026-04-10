//! The full orchestration pipeline: Decompose → Execute → Synthesize → Verify/Refine.
//!
//! The pipeline selects which stages to run based on task complexity,
//! determined by the smart router. Trivial tasks skip the pipeline
//! entirely; complex tasks get the full treatment.

use std::sync::Arc;

use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::{
    OrchestrationConfig, PipelineStage, TaskComplexity, VoteResult, VotingStrategy,
};
use rustykrab_core::{Result, Tool};

use crate::sandbox::Sandbox;

use super::decomposer::Decomposer;
use super::executor::ParallelExecutor;
use super::refiner::RefinementLoop;
use super::synthesizer::Synthesizer;
use super::verifier::ConsistencyVoter;

/// Result of running the orchestration pipeline.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// The final response text.
    pub response: String,
    /// Which stages were executed.
    pub stages_executed: Vec<PipelineStage>,
    /// Number of sub-tasks decomposed (0 if no decomposition).
    pub sub_task_count: usize,
    /// Self-consistency vote result (if voting was performed).
    pub vote: Option<VoteResult>,
    /// Number of refinement iterations (0 if no refinement).
    pub refinement_iterations: usize,
}

/// The full orchestration pipeline.
///
/// Adapts its behavior based on task complexity:
/// - Trivial/Simple: direct model call
/// - Moderate: decompose + parallel execute + synthesize
/// - Complex: + self-refinement
/// - Critical: + self-consistency voting
pub struct OrchestrationPipeline {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    config: OrchestrationConfig,
}

impl OrchestrationPipeline {
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

    /// Run the pipeline for a given request at the specified complexity level.
    pub async fn run(
        &self,
        request: &str,
        complexity: TaskComplexity,
        context: Option<&str>,
    ) -> Result<PipelineResult> {
        tracing::info!(?complexity, "running orchestration pipeline");

        match complexity {
            TaskComplexity::Trivial | TaskComplexity::Simple => {
                self.run_direct(request, context).await
            }
            TaskComplexity::Moderate => self.run_moderate(request, context).await,
            TaskComplexity::Complex => self.run_complex(request, context).await,
            TaskComplexity::Critical => self.run_critical(request, context).await,
        }
    }

    /// Direct response — no pipeline, single model call.
    async fn run_direct(&self, request: &str, context: Option<&str>) -> Result<PipelineResult> {
        use chrono::Utc;
        use rustykrab_core::types::{Message, MessageContent, Role};
        use uuid::Uuid;

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
            content: MessageContent::Text(request.to_string()),
            created_at: Utc::now(),
        });

        let schemas: Vec<_> = self.tools.iter().map(|t| t.schema()).collect();
        let response = self.provider.chat(&messages, &schemas).await?;
        let text = match response.message.content.as_text() {
            Some(t) => t.to_string(),
            None => {
                tracing::warn!(
                    "model returned non-text response in direct pipeline, using empty string"
                );
                String::new()
            }
        };

        Ok(PipelineResult {
            response: text,
            stages_executed: vec![PipelineStage::Execute],
            sub_task_count: 0,
            vote: None,
            refinement_iterations: 0,
        })
    }

    /// Moderate pipeline: decompose + parallel execute + synthesize,
    /// with a continuation loop that re-decomposes remaining work
    /// until the task is complete or max_recursion_depth is reached.
    async fn run_moderate(&self, request: &str, context: Option<&str>) -> Result<PipelineResult> {
        let decomposer = Decomposer::new(self.provider.clone(), self.config.clone());
        let executor = ParallelExecutor::new(
            self.provider.clone(),
            self.tools.clone(),
            self.sandbox.clone(),
            self.config.clone(),
        );
        let synthesizer = Synthesizer::new(self.provider.clone());
        let tool_names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();

        let max_cycles = self.config.max_recursion_depth.max(1);
        let mut total_sub_tasks = 0;
        let mut accumulated_results = Vec::new();
        let mut stages = Vec::new();

        for cycle in 0..max_cycles {
            // Build the decomposition request: on the first cycle, use the
            // original request. On subsequent cycles, ask the decomposer to
            // plan the remaining work given what has been completed so far.
            let decompose_request = if cycle == 0 {
                request.to_string()
            } else {
                let progress = synthesizer
                    .synthesize(request, &accumulated_results, context)
                    .await?;
                format!(
                    "Original request:\n{request}\n\n\
                     Progress so far:\n{progress}\n\n\
                     The above work is incomplete. Decompose ONLY the remaining \
                     tasks that have not been completed yet. Do not repeat work \
                     that is already done."
                )
            };

            // Decompose.
            let sub_tasks = decomposer
                .decompose(&decompose_request, context, &tool_names)
                .await?;
            total_sub_tasks += sub_tasks.len();
            stages.push(PipelineStage::Decompose);

            // Execute.
            let results = executor.execute(&sub_tasks, context).await;
            stages.push(PipelineStage::Execute);
            accumulated_results.extend(results);

            // Check if the task is complete.
            if cycle + 1 < max_cycles {
                let is_complete = self
                    .check_completion(request, &accumulated_results, context)
                    .await?;
                if is_complete {
                    tracing::info!(cycle = cycle + 1, "task complete after cycle");
                    break;
                }
                tracing::info!(cycle = cycle + 1, "task incomplete, continuing");
            }
        }

        // Final synthesis across all accumulated results.
        stages.push(PipelineStage::Synthesize);
        let response = synthesizer
            .synthesize(request, &accumulated_results, context)
            .await?;

        Ok(PipelineResult {
            response,
            stages_executed: stages,
            sub_task_count: total_sub_tasks,
            vote: None,
            refinement_iterations: 0,
        })
    }

    /// Quick model call to check whether the task is complete.
    async fn check_completion(
        &self,
        request: &str,
        results: &[rustykrab_core::orchestration::SubTaskResult],
        context: Option<&str>,
    ) -> Result<bool> {
        use chrono::Utc;
        use rustykrab_core::types::{Message, MessageContent, Role};
        use uuid::Uuid;

        let mut results_summary = String::new();
        for (i, r) in results.iter().enumerate() {
            if r.success {
                // Truncate long outputs to keep the completion check cheap.
                // Use floor_char_boundary to avoid splitting multi-byte UTF-8.
                let output = if r.output.len() > 500 {
                    let end = r.output.floor_char_boundary(500);
                    format!("{}...", &r.output[..end])
                } else {
                    r.output.clone()
                };
                results_summary.push_str(&format!("- Task {}: {}\n", i + 1, output));
            } else {
                let err = r.error.as_deref().unwrap_or("failed");
                results_summary.push_str(&format!("- Task {} FAILED: {}\n", i + 1, err));
            }
        }

        let prompt = format!(
            "Original request:\n{request}\n\n\
             Completed work:\n{results_summary}\n\n\
             Has the original request been FULLY completed? Consider whether \
             all requested items have been retrieved/processed.\n\
             Answer with exactly one word: COMPLETE or INCOMPLETE"
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

        let response = self.provider.chat(&messages, &[]).await?;
        let text = match response.message.content.as_text() {
            Some(t) => t.to_uppercase(),
            None => {
                tracing::warn!(
                    "model returned non-text response in completion check, assuming incomplete"
                );
                return Ok(false);
            }
        };

        Ok(text.contains("COMPLETE") && !text.contains("INCOMPLETE"))
    }

    /// Complex pipeline: decompose + execute + synthesize + refine.
    async fn run_complex(&self, request: &str, context: Option<&str>) -> Result<PipelineResult> {
        // Run moderate pipeline first.
        let mut result = self.run_moderate(request, context).await?;

        // Self-refinement (with system context so the critic knows the agent has tool access).
        let refiner = RefinementLoop::new(self.provider.clone(), self.config.clone());
        let (refined, iterations) = refiner.refine(request, &result.response, context).await?;

        result.response = refined;
        result.refinement_iterations = iterations;
        result.stages_executed.push(PipelineStage::Refine);

        Ok(result)
    }

    /// Critical pipeline: decompose + execute + synthesize + vote + refine.
    async fn run_critical(&self, request: &str, context: Option<&str>) -> Result<PipelineResult> {
        // Self-consistency voting first.
        let voter = ConsistencyVoter::new(
            self.provider.clone(),
            self.config.clone(),
            VotingStrategy::Majority,
        );
        let vote = voter.vote(request, context).await?;

        // If high confidence, use the voted answer directly and refine.
        if vote.confidence >= 0.8 {
            let refiner = RefinementLoop::new(self.provider.clone(), self.config.clone());
            let (refined, iterations) = refiner.refine(request, &vote.answer, context).await?;

            return Ok(PipelineResult {
                response: refined,
                stages_executed: vec![PipelineStage::Verify, PipelineStage::Refine],
                sub_task_count: 0,
                vote: Some(vote),
                refinement_iterations: iterations,
            });
        }

        // Low confidence — fall back to full decomposition pipeline.
        let mut result = self.run_complex(request, context).await?;
        result.vote = Some(vote);
        result.stages_executed.insert(0, PipelineStage::Verify);

        Ok(result)
    }
}
