use serde::{Deserialize, Serialize};

use crate::runner::AgentConfig;

/// A serializable harness profile that bundles all agent behavior parameters
/// into a single, swappable configuration.
///
/// Profiles vary agent loop parameters (iteration limits, retry counts,
/// context budgets) without varying the system prompt — the prompt is
/// now minimal and uniform across all profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessProfile {
    /// Human-readable name for this profile.
    pub name: String,

    /// Agent identity injected into the system prompt.
    pub agent_name: String,

    // --- Agent loop parameters ---
    /// Maximum iterations before the agent gives up.
    pub max_iterations: usize,
    /// Iteration count at which a soft warning is injected, nudging the agent
    /// to wrap up or save progress. Set to 0 to disable.
    pub soft_iteration_warning: usize,
    /// Consecutive errors before injecting a reflection prompt.
    pub max_consecutive_errors: usize,
    /// Max retries per failed tool call.
    pub max_tool_retries: u32,

    // --- Context budget ---
    /// Model's context window size in tokens.
    pub max_context_tokens: usize,
    /// Fraction of `max_context_tokens` at which compaction fires (0.0–1.0).
    /// Default is 0.85 per the RLM paper.
    pub compaction_threshold_pct: f64,
}

impl Default for HarnessProfile {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            agent_name: "RustyKrab".to_string(),
            max_iterations: 200,
            soft_iteration_warning: 150,
            max_consecutive_errors: 3,
            max_tool_retries: 2,
            max_context_tokens: 128_000,
            compaction_threshold_pct: 0.85,
        }
    }
}

impl HarnessProfile {
    /// Preset optimized for coding tasks: reflect sooner on errors, more retries.
    pub fn coding() -> Self {
        Self {
            name: "coding".to_string(),
            max_consecutive_errors: 2,
            max_tool_retries: 3,
            ..Self::default()
        }
    }

    /// Preset optimized for research: same loop params, different name.
    pub fn research() -> Self {
        Self {
            name: "research".to_string(),
            ..Self::default()
        }
    }

    /// Preset for creative tasks: fewer iterations needed.
    pub fn creative() -> Self {
        Self {
            name: "creative".to_string(),
            max_iterations: 100,
            soft_iteration_warning: 75,
            max_tool_retries: 1,
            ..Self::default()
        }
    }

    /// Convert this profile into an AgentConfig for the runner.
    pub fn to_agent_config(&self) -> AgentConfig {
        AgentConfig {
            max_iterations: self.max_iterations,
            soft_iteration_warning: self.soft_iteration_warning,
            max_consecutive_errors: self.max_consecutive_errors,
            max_tool_retries: self.max_tool_retries,
            max_context_tokens: self.max_context_tokens,
            compaction_threshold_pct: self.compaction_threshold_pct,
            ..AgentConfig::default()
        }
    }
}
