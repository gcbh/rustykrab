use serde::{Deserialize, Serialize};

use crate::runner::AgentConfig;

/// A serializable harness profile that bundles all agent behavior parameters
/// into a single, swappable configuration.
///
/// Inspired by the Meta-Harness paper (Lee et al., 2026): the "harness" is
/// everything around the LLM that shapes its behavior — prompt strategy,
/// memory policy, tool orchestration, and error recovery. By making this
/// a first-class serializable object, profiles can be:
///
/// - Stored as TOML/JSON files and version-controlled
/// - Swapped at runtime for different task types
/// - Optimized offline using trace data from previous sessions
/// - Shared across deployments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessProfile {
    /// Human-readable name for this profile.
    pub name: String,

    /// Agent identity injected into the system prompt.
    pub agent_name: String,
    pub agent_description: String,

    /// Whether to inject chain-of-thought guidance.
    pub chain_of_thought: bool,

    /// Whether to inject execution trace summaries into the conversation.
    /// Helps the model adapt when tools are failing.
    pub trace_informed_guidance: bool,

    /// How often (in iterations) to inject trace context.
    /// Lower = more frequent feedback. 0 = disabled.
    pub trace_injection_interval: usize,

    /// Task-type hint that adjusts prompt strategy.
    pub task_type: TaskType,

    // --- Agent loop parameters ---
    /// Maximum iterations before the agent gives up.
    pub max_iterations: usize,
    /// Consecutive errors before injecting a reflection prompt.
    pub max_consecutive_errors: usize,
    /// Max retries per failed tool call.
    pub max_tool_retries: u32,

    // --- Context budget ---
    /// Model's context window size in tokens.
    pub max_context_tokens: usize,
    /// Fraction of context for summary (0.0–1.0).
    pub summary_budget_ratio: f64,
    /// Fraction of context reserved for model response (0.0–1.0).
    pub response_reserve_ratio: f64,
}

/// Task-type hints that adjust prompt strategy.
///
/// Different tasks benefit from different harness configurations.
/// A coding task needs precise tool schemas; a chat task needs
/// personality and conversation flow; a research task needs
/// thorough chain-of-thought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// General-purpose assistant (default).
    General,
    /// Code generation and debugging.
    Coding,
    /// Research and information gathering.
    Research,
    /// Creative writing and content generation.
    Creative,
    /// Multi-step planning and execution.
    Planning,
}

impl Default for HarnessProfile {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            agent_name: "OpenClaw".to_string(),
            agent_description: "a capable AI assistant with tool access.".to_string(),
            chain_of_thought: true,
            trace_informed_guidance: true,
            trace_injection_interval: 5,
            task_type: TaskType::General,
            max_iterations: 30,
            max_consecutive_errors: 3,
            max_tool_retries: 2,
            max_context_tokens: 128_000,
            summary_budget_ratio: 0.20,
            response_reserve_ratio: 0.15,
        }
    }
}

impl HarnessProfile {
    /// Preset optimized for coding tasks: precise tool guidance, less chat.
    pub fn coding() -> Self {
        Self {
            name: "coding".to_string(),
            agent_name: "OpenClaw".to_string(),
            agent_description: "a precise coding assistant. You write correct, \
                idiomatic code and verify your work with tools."
                .to_string(),
            chain_of_thought: true,
            trace_informed_guidance: true,
            trace_injection_interval: 3, // More frequent feedback during coding.
            task_type: TaskType::Coding,
            max_iterations: 50, // Coding tasks often need more steps.
            max_consecutive_errors: 2, // Reflect sooner on code errors.
            max_tool_retries: 3,
            max_context_tokens: 128_000,
            summary_budget_ratio: 0.15, // Less summary, more live code context.
            response_reserve_ratio: 0.20, // Longer code responses.
        }
    }

    /// Preset optimized for research: thorough reasoning, broader context.
    pub fn research() -> Self {
        Self {
            name: "research".to_string(),
            agent_name: "OpenClaw".to_string(),
            agent_description: "a thorough research assistant. You gather information \
                from multiple sources, cross-reference facts, and provide \
                well-sourced answers."
                .to_string(),
            chain_of_thought: true,
            trace_informed_guidance: true,
            trace_injection_interval: 5,
            task_type: TaskType::Research,
            max_iterations: 40,
            max_consecutive_errors: 3,
            max_tool_retries: 2,
            max_context_tokens: 128_000,
            summary_budget_ratio: 0.25, // More summary budget for accumulated research.
            response_reserve_ratio: 0.15,
        }
    }

    /// Preset for creative tasks: less rigid structure, more personality.
    pub fn creative() -> Self {
        Self {
            name: "creative".to_string(),
            agent_name: "OpenClaw".to_string(),
            agent_description: "a creative writing assistant with a vivid imagination \
                and strong narrative instincts."
                .to_string(),
            chain_of_thought: false, // Don't impose rigid reasoning on creative work.
            trace_informed_guidance: false,
            trace_injection_interval: 0,
            task_type: TaskType::Creative,
            max_iterations: 20,
            max_consecutive_errors: 3,
            max_tool_retries: 1,
            max_context_tokens: 128_000,
            summary_budget_ratio: 0.25,
            response_reserve_ratio: 0.25, // Longer creative outputs.
        }
    }

    /// Convert this profile into an AgentConfig for the runner.
    pub fn to_agent_config(&self) -> AgentConfig {
        AgentConfig {
            max_iterations: self.max_iterations,
            max_consecutive_errors: self.max_consecutive_errors,
            max_tool_retries: self.max_tool_retries,
            max_context_tokens: self.max_context_tokens,
            summary_budget_ratio: self.summary_budget_ratio,
            response_reserve_ratio: self.response_reserve_ratio,
        }
    }

    /// Additional system prompt fragment based on task type.
    pub fn task_type_guidance(&self) -> Option<&'static str> {
        match self.task_type {
            TaskType::General => None,
            TaskType::Coding => Some(
                "CODING GUIDELINES:\n\
                 - Write correct, idiomatic code. Prefer clarity over cleverness.\n\
                 - Always verify your changes compile/run before declaring success.\n\
                 - When debugging, read the error message carefully. Reproduce first, then fix.\n\
                 - Use the most specific tool available (e.g. read a file before editing it).",
            ),
            TaskType::Research => Some(
                "RESEARCH GUIDELINES:\n\
                 - Gather information from multiple sources when possible.\n\
                 - Cross-reference facts between sources.\n\
                 - Clearly distinguish between established facts and your inferences.\n\
                 - Cite your sources and note when information may be outdated.",
            ),
            TaskType::Creative => Some(
                "CREATIVE GUIDELINES:\n\
                 - Be bold and original. Take creative risks.\n\
                 - Maintain consistent tone, style, and voice throughout.\n\
                 - Show, don't tell. Use vivid, specific details.\n\
                 - Respect the user's creative vision — enhance it, don't override it.",
            ),
            TaskType::Planning => Some(
                "PLANNING GUIDELINES:\n\
                 - Break complex tasks into clear, actionable steps.\n\
                 - Identify dependencies between steps.\n\
                 - Consider risks and fallback plans.\n\
                 - Validate feasibility before committing to an approach.",
            ),
        }
    }
}
