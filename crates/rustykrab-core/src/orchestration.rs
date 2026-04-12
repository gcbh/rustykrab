//! Types for the recursive agentic orchestration layer.
//!
//! These types define the pipeline stages, task complexity levels,
//! and intermediate results that flow through the orchestration system.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How complex a task is — determines which pipeline stages to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    /// Simple acknowledgment or lookup — direct response, no pipeline.
    Trivial,
    /// Single tool call or straightforward answer.
    Simple,
    /// Needs decomposition and parallel execution.
    Moderate,
    /// Full pipeline: decompose + execute + synthesize + refine.
    Complex,
    /// High-stakes or ambiguous: full pipeline + self-consistency voting.
    Critical,
}

/// Which stage of the orchestration pipeline a task is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    /// Breaking the request into atomic sub-tasks.
    Decompose,
    /// Executing sub-tasks in parallel.
    Execute,
    /// Aggregating results into a coherent response.
    Synthesize,
    /// Verifying consistency (optional).
    Verify,
    /// Self-refinement pass (optional).
    Refine,
}

/// A sub-task produced by the decomposition stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    /// Unique identifier for this sub-task.
    pub id: Uuid,
    /// The parent task or sub-task that spawned this one.
    pub parent_id: Option<Uuid>,
    /// Human-readable description of what to do.
    pub description: String,
    /// Optional tool to invoke for this sub-task.
    pub tool_hint: Option<String>,
    /// Whether this sub-task requires model reasoning (vs. pure tool call).
    pub requires_reasoning: bool,
    /// Maximum context tokens to allocate for this sub-task.
    pub context_budget: usize,
    /// Dependencies — IDs of sub-tasks that must complete first.
    pub depends_on: Vec<Uuid>,
}

impl SubTask {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent_id: None,
            description: description.into(),
            tool_hint: None,
            requires_reasoning: true,
            context_budget: 16384,
            depends_on: Vec::new(),
        }
    }

    pub fn with_parent(mut self, parent_id: Uuid) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    pub fn with_tool_hint(mut self, tool: impl Into<String>) -> Self {
        self.tool_hint = Some(tool.into());
        self.requires_reasoning = false;
        self
    }

    pub fn with_context_budget(mut self, budget: usize) -> Self {
        self.context_budget = budget;
        self
    }

    pub fn with_dependency(mut self, dep_id: Uuid) -> Self {
        self.depends_on.push(dep_id);
        self
    }
}

/// Result of executing a single sub-task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTaskResult {
    /// The sub-task this result belongs to.
    pub task_id: Uuid,
    /// The output text (possibly summarized).
    pub output: String,
    /// Whether execution succeeded.
    pub success: bool,
    /// Optional error message.
    pub error: Option<String>,
    /// Token usage for this sub-task.
    pub tokens_used: usize,
}

/// Configuration for the orchestration pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    /// Maximum context tokens per sub-task call.
    pub sub_task_context_budget: usize,
    /// Maximum number of sub-tasks from decomposition.
    pub max_sub_tasks: usize,
    /// Maximum recursion depth for RLM pattern.
    pub max_recursion_depth: usize,
    /// Number of samples for self-consistency voting.
    pub consistency_samples: usize,
    /// Temperature spread for voting: [base, base+spread, base+2*spread, ...].
    pub consistency_temperature_spread: f32,
    /// Maximum refinement iterations.
    pub max_refinement_iterations: usize,
    /// Maximum retries per failed tool call within sub-tasks.
    pub max_tool_retries: u32,
    /// Maximum tool-call rounds per sub-task before giving up.
    pub max_tool_rounds: usize,
    /// Whether to summarize sub-task results before synthesis.
    pub summarize_sub_results: bool,
    /// Which model to use for simple/trivial tasks (fallback model name).
    pub fallback_model: Option<String>,
    /// Which model to use for complex tasks (primary model name).
    pub primary_model: Option<String>,
    /// Maximum number of concurrent LLM/tool tasks spawned in parallel.
    /// Prevents pathological workloads from overwhelming the system
    /// (fixes ASYNC-M1).
    pub max_concurrent_tasks: usize,
    /// Timeout in seconds for individual model/LLM calls within the pipeline.
    pub model_call_timeout_secs: u64,
    /// Timeout in seconds for the entire orchestration pipeline.
    pub pipeline_timeout_secs: u64,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            sub_task_context_budget: 16384,
            max_sub_tasks: 8,
            max_recursion_depth: 3,
            consistency_samples: 3,
            consistency_temperature_spread: 0.1,
            max_refinement_iterations: 3,
            max_tool_retries: 2,
            max_tool_rounds: 10,
            summarize_sub_results: true,
            fallback_model: None,
            primary_model: None,
            max_concurrent_tasks: 10,
            model_call_timeout_secs: 120,
            pipeline_timeout_secs: 1800,
        }
    }
}

/// A node in the recursive call tree (RLM pattern).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveCall {
    /// Unique call ID.
    pub id: Uuid,
    /// Parent call (None for root).
    pub parent_id: Option<Uuid>,
    /// The prompt/question for this call.
    pub prompt: String,
    /// Context budget for this call.
    pub context_budget: usize,
    /// Current recursion depth.
    pub depth: usize,
    /// Child calls spawned by this call.
    pub children: Vec<Uuid>,
    /// Result once resolved.
    pub result: Option<String>,
}

impl RecursiveCall {
    pub fn root(prompt: impl Into<String>, context_budget: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent_id: None,
            prompt: prompt.into(),
            context_budget,
            depth: 0,
            children: Vec::new(),
            result: None,
        }
    }

    pub fn child(
        parent_id: Uuid,
        prompt: impl Into<String>,
        context_budget: usize,
        depth: usize,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent_id: Some(parent_id),
            prompt: prompt.into(),
            context_budget,
            depth,
            children: Vec::new(),
            result: None,
        }
    }
}

/// Result of a self-consistency vote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteResult {
    /// The winning answer.
    pub answer: String,
    /// Number of samples that agreed with the winner.
    pub agreement_count: usize,
    /// Total number of samples.
    pub total_samples: usize,
    /// Whether the vote was unanimous.
    pub unanimous: bool,
    /// All individual responses for inspection.
    pub responses: Vec<String>,
    /// Confidence score (0.0-1.0) based on agreement ratio.
    pub confidence: f64,
}

/// Voting strategy for self-consistency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VotingStrategy {
    /// Take the most common answer.
    Majority,
    /// Require all samples to agree, otherwise escalate.
    UnanimousOrEscalate,
}
