//! The full orchestration pipeline: Decompose → Execute → Synthesize → Verify/Refine.
//!
//! The pipeline selects which stages to run based on task complexity,
//! determined by the smart router. Trivial tasks skip the pipeline
//! entirely; complex tasks get the full treatment.

use std::sync::Arc;

use openclaw_core::model::ModelProvider;
use openclaw_core::orchestration::{
    OrchestrationConfig, PipelineStage, TaskComplexity, VoteResult, VotingStrategy,
};
use openclaw_core::{Result, Tool};

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
    async fn run_direct(
        &self,
        request: &str,
        context: Option<&str>,
    ) -> Result<PipelineResult> {
        use chrono::Utc;
        use openclaw_core::types::{Message, MessageContent, Role};
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
        let text = response
            .message
            .content
            .as_text()
            .unwrap_or("")
            .to_string();

        Ok(PipelineResult {
            response: text,
            stages_executed: vec![PipelineStage::Execute],
            sub_task_count: 0,
            vote: None,
            refinement_iterations: 0,
        })
    }

    /// Moderate pipeline: decompose + parallel execute + synthesize.
    async fn run_moderate(
        &self,
        request: &str,
        _context: Option<&str>,
    ) -> Result<PipelineResult> {
        let decomposer = Decomposer::new(self.provider.clone(), self.config.clone());
        let executor = ParallelExecutor::new(
            self.provider.clone(),
            self.tools.clone(),
            self.sandbox.clone(),
            self.config.clone(),
        );
        let synthesizer = Synthesizer::new(self.provider.clone());

        // Decompose.
        let sub_tasks = decomposer.decompose(request).await?;
        let sub_task_count = sub_tasks.len();

        // Execute in parallel.
        let results = executor.execute(&sub_tasks).await;

        // Synthesize.
        let response = synthesizer.synthesize(request, &results).await?;

        Ok(PipelineResult {
            response,
            stages_executed: vec![
                PipelineStage::Decompose,
                PipelineStage::Execute,
                PipelineStage::Synthesize,
            ],
            sub_task_count,
            vote: None,
            refinement_iterations: 0,
        })
    }

    /// Complex pipeline: decompose + execute + synthesize + refine.
    async fn run_complex(
        &self,
        request: &str,
        context: Option<&str>,
    ) -> Result<PipelineResult> {
        // Run moderate pipeline first.
        let mut result = self.run_moderate(request, context).await?;

        // Self-refinement.
        let refiner = RefinementLoop::new(self.provider.clone(), self.config.clone());
        let (refined, iterations) = refiner.refine(request, &result.response).await?;

        result.response = refined;
        result.refinement_iterations = iterations;
        result.stages_executed.push(PipelineStage::Refine);

        Ok(result)
    }

    /// Critical pipeline: decompose + execute + synthesize + vote + refine.
    async fn run_critical(
        &self,
        request: &str,
        context: Option<&str>,
    ) -> Result<PipelineResult> {
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
            let (refined, iterations) = refiner.refine(request, &vote.answer).await?;

            return Ok(PipelineResult {
                response: refined,
                stages_executed: vec![
                    PipelineStage::Verify,
                    PipelineStage::Refine,
                ],
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
