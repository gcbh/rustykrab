use std::sync::Arc;

use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::TaskComplexity;

use crate::harness::HarnessProfile;

/// Routes incoming messages to the right harness profile automatically.
///
/// Uses Rust-side keyword heuristics to classify the user's intent,
/// then returns the appropriate profile. No LLM call needed — profile
/// detection happens instantly based on message content.
pub struct HarnessRouter {
    /// Profile fields the operator named explicitly, which a preset must
    /// not override. Empty means "the operator said nothing", so the
    /// preset decides everything.
    pinned: Vec<String>,
    /// Base profile to use as a template. Task-specific fields get overlaid.
    base: HarnessProfile,
    /// A model provider kept for potential future use (e.g. RLM context
    /// management). Not used for profile classification.
    _classifier: Arc<dyn ModelProvider>,
}

impl HarnessRouter {
    /// Create a router with a model provider reference.
    pub fn new(classifier: Arc<dyn ModelProvider>) -> Self {
        Self {
            _classifier: classifier,
            base: HarnessProfile::default(),
            pinned: Vec::new(),
        }
    }

    /// Pin the profile fields the operator set explicitly, so a routed
    /// preset cannot quietly replace them.
    pub fn with_pinned_fields(mut self, fields: Vec<String>) -> Self {
        self.pinned = fields;
        self
    }

    /// Use a custom base profile that gets task-specific overlays applied.
    pub fn with_base(mut self, base: HarnessProfile) -> Self {
        self.base = base;
        self
    }

    /// Classify the complexity of a user message for RLM routing.
    pub async fn classify_complexity(&self, user_message: &str) -> TaskComplexity {
        classify_complexity_keywords(user_message)
    }

    /// Classify a user message and return the appropriate harness profile.
    ///
    /// Uses keyword heuristics instead of an LLM call — instant and free.
    pub async fn route(&self, user_message: &str) -> HarnessProfile {
        let profile_name = classify_profile_keywords(user_message);
        let mut profile = match profile_name {
            "coding" => HarnessProfile::coding(),
            "research" => HarnessProfile::research(),
            "creative" => HarnessProfile::creative(),
            _ => self.base.clone(),
        };
        // Preserve user customizations from the base profile.
        //
        // These three are unconditional: they describe the deployment, not
        // the task.
        profile.agent_name = self.base.agent_name.clone();
        profile.max_context_tokens = self.base.max_context_tokens;
        profile.compaction_threshold_pct = self.base.compaction_threshold_pct;

        // The loop parameters are the preset's to choose — that is what a
        // preset is for — *unless* the operator named them. "Named" rather
        // than "differs from the default": an operator who writes
        // `max_tool_retries = 2` means 2, even though 2 is also the
        // default, and a preset asking for 3 would still be overriding a
        // stated choice.
        for field in &self.pinned {
            match field.as_str() {
                "max_iterations" => profile.max_iterations = self.base.max_iterations,
                "soft_iteration_warning" => {
                    profile.soft_iteration_warning = self.base.soft_iteration_warning
                }
                "max_consecutive_errors" => {
                    profile.max_consecutive_errors = self.base.max_consecutive_errors
                }
                "max_tool_retries" => profile.max_tool_retries = self.base.max_tool_retries,
                _ => {}
            }
        }
        profile
    }
}

/// Classify complexity using keyword heuristics. No LLM call — instant.
///
/// Heuristics:
/// - Multiple sub-questions or "and then" / "after that" -> Complex
/// - "compare", "analyze", "research", "step by step" -> Moderate
/// - Short, single-action requests -> Simple
/// - Very short greetings/questions -> Trivial
pub fn classify_complexity_keywords(text: &str) -> TaskComplexity {
    let lower = text.to_lowercase();
    let word_count = lower.split_whitespace().count();

    // Trivial: very short, no action
    if word_count <= 5 {
        return TaskComplexity::Trivial;
    }

    // Count complexity signals
    let complex_signals = [
        "and then",
        "after that",
        "once you",
        "next step",
        "first do",
        "then do",
        "finally",
        "multiple",
        "step by step",
        "break down",
        "break it down",
        "all of the",
        "each of the",
        "every",
    ];
    let moderate_signals = [
        "compare",
        "analyze",
        "analyse",
        "research",
        "investigate",
        "summarize",
        "review",
        "evaluate",
        "assess",
        "pros and cons",
        "difference between",
        "trade-off",
        "explain how",
        "explain why",
        "deep dive",
    ];

    let complex_count = complex_signals
        .iter()
        .filter(|s| lower.contains(**s))
        .count();
    let moderate_count = moderate_signals
        .iter()
        .filter(|s| lower.contains(**s))
        .count();

    // Count question marks and list items (numbered or bulleted)
    let question_marks = lower.matches('?').count();
    let list_items = lower
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("- ")
                || t.starts_with("* ")
                || t.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .count();

    if complex_count >= 2 || (complex_count >= 1 && moderate_count >= 1) || list_items >= 4 {
        TaskComplexity::Complex
    } else if moderate_count >= 1
        || complex_count >= 1
        || question_marks >= 2
        || list_items >= 2
        || word_count > 100
    {
        TaskComplexity::Moderate
    } else {
        TaskComplexity::Simple
    }
}

/// Classify a message into a profile name using keyword heuristics.
/// Returns one of: "coding", "research", "creative", "general".
fn classify_profile_keywords(text: &str) -> &'static str {
    let lower = text.to_lowercase();

    let coding_signals = [
        "code",
        "function",
        "bug",
        "error",
        "compile",
        "debug",
        "refactor",
        "implement",
        "class",
        "struct",
        "enum",
        "trait",
        "async",
        "await",
        "api",
        "endpoint",
        "database",
        "query",
        "sql",
        "rust",
        "python",
        "javascript",
        "typescript",
    ];
    let research_signals = [
        "research",
        "find out",
        "look up",
        "search for",
        "investigate",
        "compare",
        "what is",
        "how does",
        "difference between",
        "pros and cons",
    ];
    let creative_signals = [
        "write a story",
        "write a poem",
        "creative",
        "brainstorm",
        "imagine",
        "narrative",
        "fiction",
        "marketing copy",
    ];

    let coding_count = coding_signals
        .iter()
        .filter(|s| lower.contains(**s))
        .count();
    let research_count = research_signals
        .iter()
        .filter(|s| lower.contains(**s))
        .count();
    let creative_count = creative_signals
        .iter()
        .filter(|s| lower.contains(**s))
        .count();

    if creative_count > 0 && creative_count >= coding_count && creative_count >= research_count {
        "creative"
    } else if coding_count > 0 && coding_count >= research_count {
        "coding"
    } else if research_count > 0 {
        "research"
    } else {
        "general"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_complexity_keywords() {
        assert_eq!(
            classify_complexity_keywords("hello"),
            TaskComplexity::Trivial
        );
        assert_eq!(
            classify_complexity_keywords("write a function that sorts a list"),
            TaskComplexity::Simple
        );
        assert_eq!(
            classify_complexity_keywords("compare the pros and cons of Rust vs Go"),
            TaskComplexity::Moderate
        );
    }

    #[test]
    fn test_classify_profile_keywords() {
        assert_eq!(classify_profile_keywords("write a function"), "coding");
        assert_eq!(classify_profile_keywords("debug this error"), "coding");
        assert_eq!(
            classify_profile_keywords("research the best options"),
            "research"
        );
        assert_eq!(
            classify_profile_keywords("write a story about dragons"),
            "creative"
        );
        assert_eq!(classify_profile_keywords("hello there"), "general");
    }
}

#[cfg(test)]
mod base_preservation_tests {
    use super::*;

    use async_trait::async_trait;
    use chrono::Utc;
    use rustykrab_core::error::Result;
    use rustykrab_core::model::{ModelResponse, StopReason, Usage};
    use rustykrab_core::types::{Message, MessageContent, Role, ToolSchema};
    use uuid::Uuid;

    /// `route` classifies by keyword and never calls the model, so the
    /// classifier only has to exist.
    struct UnusedProvider;

    #[async_trait]
    impl ModelProvider for UnusedProvider {
        fn name(&self) -> &str {
            "unused"
        }
        async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> Result<ModelResponse> {
            Ok(ModelResponse {
                message: Message {
                    id: Uuid::new_v4(),
                    role: Role::Assistant,
                    content: MessageContent::Text(String::new()),
                    created_at: Utc::now(),
                    agent_version: None,
                },
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                text: None,
            })
        }
    }

    fn router_with_base(base: HarnessProfile) -> HarnessRouter {
        HarnessRouter::new(Arc::new(UnusedProvider)).with_base(base)
    }

    #[tokio::test]
    async fn a_named_loop_parameter_survives_a_preset() {
        // "write some code" classifies as coding, whose preset asks for 3
        // tool retries. An operator who wrote 1 in harness.toml meant 1.
        let base = HarnessProfile {
            max_tool_retries: 1,
            max_iterations: 20,
            ..HarnessProfile::default()
        };
        let routed = router_with_base(base)
            .with_pinned_fields(vec!["max_tool_retries".into(), "max_iterations".into()])
            .route("please write some code for me")
            .await;

        assert_eq!(routed.max_tool_retries, 1, "the operator's value must win");
        assert_eq!(routed.max_iterations, 20);
    }

    #[tokio::test]
    async fn a_named_parameter_wins_even_when_it_equals_the_default() {
        // Why this keys on "was it named" rather than "does it differ from
        // the default": 2 is also the default, so a differs-from-default
        // rule hands this to the preset's 3 and silently overrides a
        // stated choice.
        assert_eq!(HarnessProfile::default().max_tool_retries, 2);
        let base = HarnessProfile {
            max_tool_retries: 2,
            ..HarnessProfile::default()
        };
        let routed = router_with_base(base)
            .with_pinned_fields(vec!["max_tool_retries".into()])
            .route("please write some code for me")
            .await;

        assert_eq!(routed.max_tool_retries, 2);
    }

    #[tokio::test]
    async fn an_unnamed_parameter_still_comes_from_the_preset() {
        // Where the operator expressed no opinion, the preset decides —
        // that is the whole point of routing.
        let routed = router_with_base(HarnessProfile::default())
            .route("please write some code for me")
            .await;
        assert_eq!(
            routed.max_tool_retries,
            HarnessProfile::coding().max_tool_retries
        );
    }
}
