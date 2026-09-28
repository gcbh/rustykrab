use rustykrab_core::LateToolBinding;
use serde::{Deserialize, Serialize};

use crate::runner::AgentConfig;
use crate::CompactionStrategy;

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
    /// Explicit compaction policy; routing must preserve the operator's choice.
    pub compaction_strategy: CompactionStrategy,

    // --- Tool delivery ---
    /// How a tool found mid-run reaches the model: `"append"` (its schema
    /// as text in a tool result, the tools array fixed) or `"rerender"`
    /// (added to the tools array). Unset takes the provider's capability
    /// data. Set it for a model whose chat template rejects calls to
    /// undeclared tools although its provider usually accepts them: a
    /// model difference expressed as data, never as a code path (plan
    /// section 12.1).
    pub late_tool_binding: Option<LateToolBinding>,
    /// How many `tools_list` searches for one need may find nothing in a
    /// run before the host answers the next as final (no tool provides it;
    /// tell the user, or in a worker run report `needs_tool`) and records a
    /// `capability_gap/tool`. Told nothing matched, both default local
    /// models kept searching until the iteration cap (scenario 10,
    /// 2026-09-28).
    pub tool_search_miss_limit: usize,
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
            compaction_strategy: CompactionStrategy::default(),
            late_tool_binding: None,
            tool_search_miss_limit: rustykrab_core::DEFAULT_SEARCH_MISS_LIMIT,
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
            compaction_strategy: self.compaction_strategy,
            late_tool_binding: self.late_tool_binding,
            tool_search_miss_limit: self.tool_search_miss_limit,
            ..AgentConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_policy_roundtrips_and_old_profiles_keep_default() {
        let old: HarnessProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(old.compaction_strategy, CompactionStrategy::default());
        let profile: HarnessProfile =
            serde_json::from_str(r#"{"compaction_strategy":"structured-message-tail"}"#).unwrap();
        assert_eq!(
            profile.to_agent_config().compaction_strategy,
            CompactionStrategy::StructuredMessageTail
        );
        assert_eq!(
            serde_json::to_value(profile).unwrap()["compaction_strategy"],
            "structured-message-tail"
        );
        assert!(
            serde_json::from_str::<HarnessProfile>(r#"{"compaction_strategy":"typo"}"#).is_err()
        );
    }

    #[test]
    fn late_tool_binding_is_profile_data_and_unset_by_default() {
        let old: HarnessProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(old.late_tool_binding, None);
        assert_eq!(old.to_agent_config().late_tool_binding, None);
        let profile: HarnessProfile =
            serde_json::from_str(r#"{"late_tool_binding":"rerender"}"#).unwrap();
        assert_eq!(
            profile.to_agent_config().late_tool_binding,
            Some(LateToolBinding::Rerender)
        );
        assert!(serde_json::from_str::<HarnessProfile>(r#"{"late_tool_binding":"x"}"#).is_err());
    }

    #[test]
    fn the_search_miss_limit_is_profile_data_defaulting_to_two() {
        let old: HarnessProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(old.tool_search_miss_limit, 2);
        assert_eq!(old.to_agent_config().tool_search_miss_limit, 2);
        let profile: HarnessProfile =
            serde_json::from_str(r#"{"tool_search_miss_limit":4}"#).unwrap();
        assert_eq!(profile.to_agent_config().tool_search_miss_limit, 4);
    }
}
