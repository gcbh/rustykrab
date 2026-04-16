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
        }
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
        profile.agent_name = self.base.agent_name.clone();
        profile.max_context_tokens = self.base.max_context_tokens;
        profile.compaction_threshold_pct = self.base.compaction_threshold_pct;
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
